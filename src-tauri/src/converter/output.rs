use crate::settings::{NameConflict, OutputDest, Settings};
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Semaphore;

/// 同一マシン上の別インスタンスとの、完成ファイル単位の予約。
/// 正常終了時は Drop で消え、クラッシュ時は次回にPIDを確認して回収する。
pub struct OutputPathLock {
    path: PathBuf,
}

impl Drop for OutputPathLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn output_lock_path(output: &Path) -> PathBuf {
    let mut hash = 0xcbf29ce484222325u64; // stable FNV-1a (プロセス間で同じ名前になる)
    for byte in output
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .as_bytes()
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".oto-output-lock-{hash:016x}"))
}

fn lock_owner_is_running(path: &Path) -> bool {
    let Ok(pid) = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .ok_or(())
    else {
        return false;
    };
    let system = sysinfo::System::new_all();
    system.process(sysinfo::Pid::from_u32(pid)).is_some()
}

fn try_acquire_output_lock(output: &Path) -> Result<Option<OutputPathLock>> {
    let path = output_lock_path(output);
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(std::process::id().to_string().as_bytes())?;
                return Ok(Some(OutputPathLock { path }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_owner_is_running(&path) {
                    return Ok(None);
                }
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

async fn ask_overwrite_dialog(app: &AppHandle, filename: &str) -> super::OverwriteChoice {
    #[derive(Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload<'a> {
        dialog_id: &'a str,
        filename: &'a str,
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<super::OverwriteChoice>();
    let dialog_id = uuid::Uuid::new_v4().to_string();
    {
        let state = app.state::<crate::AppState>();
        *state.overwrite_tx.lock().unwrap() = Some(crate::OverwritePrompt {
            id: dialog_id.clone(),
            sender: tx,
        });
    }
    if let Some(w) = app.get_webview_window("main") {
        w.emit(
            "overwrite_confirm",
            Payload {
                dialog_id: &dialog_id,
                filename,
            },
        )
        .ok();
    }
    rx.await.unwrap_or(super::OverwriteChoice::CancelAll)
}

/// 出力形式はFFmpegに委ねる。Unicodeを含む任意の拡張子を許可し、パス移動に
/// つながる区切り文字だけを拒否する。
fn validate_output_extension(ext: &str) -> Result<()> {
    if ext.is_empty() || ext.contains('/') || ext.contains('\0') {
        return Err(anyhow!(
            "output format must be a single non-empty file extension"
        ));
    }
    #[cfg(windows)]
    if ext
        .chars()
        .any(|c| matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err(anyhow!(
            "output format contains characters unsupported by Windows filenames"
        ));
    }
    Ok(())
}

fn folder_named_candidate(
    input: &Path,
    output_dir: &Path,
    stem: &str,
    ext: &str,
    level: usize,
) -> Option<PathBuf> {
    let parts: Vec<String> = input
        .parent()?
        .ancestors()
        .filter_map(|p| p.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .take(level)
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(output_dir.join(format!("{}({}).{}", stem, parts.join("-"), ext)))
    }
}

/// 同一ジョブで既に予約された出力名と衝突したとき、親フォルダ名を加えた候補を返す。
fn reserve_output_path(
    input: &Path,
    output_dir: &Path,
    stem: &str,
    ext: &str,
    settings: &Settings,
    reservations: &Arc<Mutex<HashMap<PathBuf, u64>>>,
    batch_order: u64,
) -> (PathBuf, bool) {
    let base = output_dir.join(format!("{}.{}", stem, ext));
    let mut reserved = reservations.lock().unwrap();
    let mut preferred = base.clone();
    let base_owner = reserved.get(&preferred).copied();
    let same_batch_collision = base_owner == Some(batch_order);
    let later_batch_collision = base_owner.is_some() && !same_batch_collision;
    if same_batch_collision
        || (later_batch_collision && settings.name_conflict == NameConflict::AutoRename)
    {
        let ancestor_count = input.parent().map(|p| p.ancestors().count()).unwrap_or(0);
        for level in 1..=ancestor_count {
            if let Some(candidate) = folder_named_candidate(input, output_dir, stem, ext, level) {
                if !reserved.contains_key(&candidate) {
                    preferred = candidate;
                    break;
                }
            }
        }
        // 同一の祖先名まで重なる場合だけ、最後の手段として連番を付ける。
        if reserved.contains_key(&preferred) {
            let mut i = 1u32;
            loop {
                let candidate = output_dir.join(format!("{}_{}.{}", stem, i, ext));
                if !reserved.contains_key(&candidate) {
                    preferred = candidate;
                    break;
                }
                i += 1;
            }
        }
    }

    let final_path =
        if !preferred.exists() || settings.name_conflict == NameConflict::ForceOverwrite {
            preferred
        } else if settings.name_conflict == NameConflict::AutoRename {
            let parent = preferred.parent().unwrap_or(output_dir);
            let preferred_stem = preferred.file_stem().unwrap_or_default().to_string_lossy();
            let mut i = 1u32;
            loop {
                let candidate = parent.join(format!("{}_{}.{}", preferred_stem, i, ext));
                if !candidate.exists() && !reserved.contains_key(&candidate) {
                    break candidate;
                }
                i += 1;
            }
        } else {
            preferred
        };
    let session_collision =
        later_batch_collision && settings.name_conflict != NameConflict::AutoRename;
    reserved.entry(final_path.clone()).or_insert(batch_order);
    (final_path, session_collision)
}

pub struct OutputResolutionContext<'a> {
    pub format: &'a str,
    pub settings: &'a Settings,
    pub base_dir: Option<&'a Path>,
    pub app: &'a AppHandle,
    pub dialog_sem: &'a Semaphore,
    pub reservations: &'a Arc<Mutex<HashMap<PathBuf, u64>>>,
    pub cancellation: &'a crate::JobCancellation,
    pub batch_order: u64,
}

pub async fn resolve_output_path(
    input: &Path,
    context: &OutputResolutionContext<'_>,
) -> Result<(PathBuf, OutputPathLock)> {
    let stem = input
        .file_stem()
        .ok_or_else(|| anyhow!("invalid filename"))?
        .to_string_lossy()
        .into_owned();
    resolve_output_path_for_stem(input, &stem, context).await
}

pub async fn resolve_output_path_for_stem(
    input: &Path,
    stem: &str,
    context: &OutputResolutionContext<'_>,
) -> Result<(PathBuf, OutputPathLock)> {
    let format = context.format;
    let settings = context.settings;
    let base_dir = context.base_dir;
    let app = context.app;
    let dialog_sem = context.dialog_sem;
    let reservations = context.reservations;
    let cancellation = context.cancellation;
    let mut output_dir = match &settings.output_dest {
        OutputDest::SourceFolder => input
            .parent()
            .ok_or_else(|| anyhow!("no parent dir"))?
            .to_path_buf(),
        OutputDest::Desktop => dirs::desktop_dir().ok_or_else(|| anyhow!("no desktop dir"))?,
        OutputDest::Downloads => dirs::download_dir().ok_or_else(|| anyhow!("no downloads dir"))?,
        OutputDest::Custom => {
            let p = settings
                .custom_output_path
                .as_deref()
                .ok_or_else(|| anyhow!("custom path not set"))?;
            PathBuf::from(p)
        }
    };

    if settings.preserve_folder_structure && settings.output_dest != OutputDest::SourceFolder {
        if let (Some(base), Some(parent)) = (base_dir, input.parent()) {
            if let Ok(rel) = parent.strip_prefix(base) {
                output_dir = output_dir.join(rel);
            }
        }
    }

    std::fs::create_dir_all(&output_dir)?;
    output_dir = std::fs::canonicalize(&output_dir).unwrap_or(output_dir);

    // ALAC・AAC は M4A コンテナを使う
    let ext = match format {
        "alac" | "aac" => "m4a",
        other => other,
    };
    validate_output_extension(ext)?;
    let filename = format!("{}.{}", stem, ext);
    let (candidate, session_collision) = reserve_output_path(
        input,
        &output_dir,
        stem,
        ext,
        settings,
        reservations,
        context.batch_order,
    );

    let must_confirm = session_collision
        || (candidate.exists() && settings.name_conflict == NameConflict::ConfirmDialog);
    let resolved = if !must_confirm {
        candidate
    } else {
        match if session_collision {
            NameConflict::ConfirmDialog
        } else {
            settings.name_conflict.clone()
        } {
            NameConflict::AutoRename => {
                // reserve_output_path がディスク上・ジョブ内の両方の衝突を解決済み。
                candidate
            }
            NameConflict::ConfirmDialog => {
                let permit = tokio::select! {
                    permit = dialog_sem.acquire() => permit,
                    _ = cancellation.cancelled() => return Err(anyhow!("__CANCELLED__")),
                };
                let _permit = permit.map_err(|_| anyhow!("overwrite dialog closed"))?;
                if cancellation.is_cancelled() {
                    return Err(anyhow!("__CANCELLED__"));
                }
                let display = candidate
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| filename.clone());
                match ask_overwrite_dialog(app, &display).await {
                    super::OverwriteChoice::Overwrite => candidate,
                    super::OverwriteChoice::Skip => {
                        if !session_collision {
                            reservations.lock().unwrap().remove(&candidate);
                        }
                        return Err(anyhow!("__SKIPPED__"));
                    }
                    super::OverwriteChoice::CancelAll => {
                        if !session_collision {
                            reservations.lock().unwrap().remove(&candidate);
                        }
                        cancellation.cancel();
                        return Err(anyhow!("__CANCELLED__"));
                    }
                    super::OverwriteChoice::Rename => {
                        if !session_collision {
                            reservations.lock().unwrap().remove(&candidate);
                        }
                        // 既存ファイルへの上書きだけを避け、同一ジョブの予約も維持して再解決する。
                        let mut renamed_settings = settings.clone();
                        renamed_settings.name_conflict = NameConflict::AutoRename;
                        reserve_output_path(
                            input,
                            &output_dir,
                            stem,
                            ext,
                            &renamed_settings,
                            reservations,
                            context.batch_order,
                        )
                        .0
                    }
                }
            }
            NameConflict::ForceOverwrite => candidate,
        }
    };

    // 他インスタンスが同じ完成ファイルを予約済みなら、この候補をジョブ内でも予約済みに
    // したまま呼び出し元へエラーを返す。次のファイルは親フォルダ名付き候補へ回避する。
    loop {
        match try_acquire_output_lock(&resolved)? {
            Some(lock) => return Ok((resolved, lock)),
            None if session_collision => {
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(anyhow!("__CANCELLED__")),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            }
            None => return Err(anyhow!("__OUTPUT_PATH_BUSY__")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_output_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oto-output-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn same_job_collision_uses_parent_folder_name() {
        let dir = temp_output_dir();
        let reservations = Arc::new(Mutex::new(HashMap::new()));
        let settings = Settings::default();
        let first = reserve_output_path(
            Path::new("/library/Album A/song.flac"),
            &dir,
            "song",
            "mp3",
            &settings,
            &reservations,
            1,
        );
        let second = reserve_output_path(
            Path::new("/library/Album B/song.flac"),
            &dir,
            "song",
            "mp3",
            &settings,
            &reservations,
            1,
        );
        assert_eq!(first.0.file_name().unwrap(), "song.mp3");
        assert_eq!(second.0.file_name().unwrap(), "song(Album B).mp3");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn same_leaf_folder_name_adds_higher_ancestor() {
        let dir = temp_output_dir();
        let reservations = Arc::new(Mutex::new(HashMap::new()));
        let settings = Settings::default();
        reserve_output_path(
            Path::new("/library/Original/song.flac"),
            &dir,
            "song",
            "mp3",
            &settings,
            &reservations,
            1,
        );
        let second = reserve_output_path(
            Path::new("/library/Artist 1/Disc/song.flac"),
            &dir,
            "song",
            "mp3",
            &settings,
            &reservations,
            1,
        );
        let third = reserve_output_path(
            Path::new("/library/Artist 2/Disc/song.flac"),
            &dir,
            "song",
            "mp3",
            &settings,
            &reservations,
            1,
        );
        assert_eq!(second.0.file_name().unwrap(), "song(Disc).mp3");
        assert_eq!(third.0.file_name().unwrap(), "song(Disc-Artist 2).mp3");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_file_uses_auto_rename_without_reusing_reservation() {
        let dir = temp_output_dir();
        std::fs::write(dir.join("song.mp3"), b"existing").unwrap();
        let reservations = Arc::new(Mutex::new(HashMap::new()));
        let settings = Settings::default();
        let output = reserve_output_path(
            Path::new("/library/Album/song.flac"),
            &dir,
            "song",
            "mp3",
            &settings,
            &reservations,
            1,
        );
        assert_eq!(output.0.file_name().unwrap(), "song_1.mp3");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn later_force_overwrite_batch_reports_a_session_collision() {
        let dir = temp_output_dir();
        let reservations = Arc::new(Mutex::new(HashMap::new()));
        let settings = Settings {
            name_conflict: NameConflict::ForceOverwrite,
            ..Settings::default()
        };
        let first = reserve_output_path(
            Path::new("/library/song.flac"),
            &dir,
            "song",
            "mp3",
            &settings,
            &reservations,
            1,
        );
        let second = reserve_output_path(
            Path::new("/library/song.wav"),
            &dir,
            "song",
            "mp3",
            &settings,
            &reservations,
            2,
        );
        assert_eq!(first.0, second.0);
        assert!(!first.1);
        assert!(second.1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn later_auto_rename_batch_reserves_a_distinct_path_without_prompting() {
        let dir = temp_output_dir();
        let reservations = Arc::new(Mutex::new(HashMap::new()));
        let settings = Settings::default();
        reserve_output_path(
            Path::new("/library/song.flac"),
            &dir,
            "song",
            "mp3",
            &settings,
            &reservations,
            1,
        );
        let second = reserve_output_path(
            Path::new("/library/song.wav"),
            &dir,
            "song",
            "mp3",
            &settings,
            &reservations,
            2,
        );
        assert_ne!(second.0.file_name().unwrap(), "song.mp3");
        assert!(!second.1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn output_path_lock_excludes_second_instance_until_released() {
        let dir = temp_output_dir();
        let output = dir.join("song.mp3");
        let first = try_acquire_output_lock(&output)
            .unwrap()
            .expect("first lock");
        assert!(try_acquire_output_lock(&output).unwrap().is_none());
        drop(first);
        assert!(try_acquire_output_lock(&output).unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn output_extension_keeps_unicode_but_rejects_path_separator() {
        assert!(validate_output_extension("音楽🎵").is_ok());
        assert!(validate_output_extension("../mp3").is_err());
        assert!(validate_output_extension("").is_err());
    }
}
