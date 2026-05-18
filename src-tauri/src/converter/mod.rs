mod binary;
mod codec_args;
mod file_collector;
mod output;
mod probe;
pub mod silence;
mod types;

pub use binary::ffmpeg_path;
pub use file_collector::collect_audio_files;
pub use silence::run_silence_detect;
pub use types::{CompletionPayload, ConvertRequest, OverwriteChoice, ProgressPayload};

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;

use crate::settings::{OutputDest, Settings, SourceFileAction};
use codec_args::build_codec_args;
use file_collector::{common_ancestor, select_best_sources};
use output::resolve_output_path;
use probe::probe_file;
use silence::detect_boundary_silence;
use types::{FileInfo, FileResult as FR};

#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(windows)]
static PROGRESS_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

// --- Single file conversion ---

#[allow(clippy::too_many_arguments)]
async fn convert_one(
    input: &Path,
    output: &Path,
    format: &str,
    settings: &Settings,
    info: &FileInfo,
    duration_secs: f64,
    threads_per_job: usize,
    on_progress: impl Fn(f64) + Send,
    on_pid: impl Fn(u32) + Send,
) -> Result<()> {
    // キャンセルやエラー時に不完全な出力ファイルを削除するガード
    // 上書きの場合は既存ファイルを一時ファイルに退避し、失敗時にリストア
    struct OutputGuard {
        path: PathBuf,
        keep: bool,
        backup: Option<PathBuf>,
    }
    impl Drop for OutputGuard {
        fn drop(&mut self) {
            if !self.keep {
                let _ = std::fs::remove_file(&self.path);
                if let Some(backup) = &self.backup {
                    let _ = std::fs::rename(backup, &self.path);
                }
            }
        }
    }

    let backup_path = if output.exists() {
        let mut backup = output.to_path_buf();
        let ext = output.extension().map(|e| e.to_string_lossy().into_owned());
        let stem = output.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(e) = &ext {
            backup.set_file_name(format!("{}.backup.{}", stem, e));
        } else {
            backup.set_file_name(format!("{}.backup", stem));
        }
        let _ = std::fs::rename(output, &backup);
        Some(backup)
    } else {
        None
    };
    let mut output_guard = OutputGuard { path: output.to_path_buf(), keep: false, backup: backup_path };

    let ffmpeg = ffmpeg_path();
    // Input/output paths are added to cmd directly as OsStr (see below) to
    // support non-UTF-8 filenames. Only non-path args go in this Vec.
    let mut args: Vec<String> = vec![
        "-threads".into(), threads_per_job.to_string(),
        "-y".into(),
        "-map_metadata".into(),
        "0".into(),
        "-map".into(),
        "0:a".into(),
    ];

    // カバーアート（埋め込み画像）の引き継ぎ
    // WAV/OGG/OPUS/AIFFはコンテナ仕様上カバーアート非対応。
    // 特定ストリームインデックスを使うことで、H264など非対応コーデックのattached_picを
    // 誤ってマップするバグ（m4vのH264サムネイルでMP3変換失敗）を防ぐ。
    if matches!(format, "mp3" | "aac" | "flac" | "alac") {
        if let Some(idx) = info.cover_art_stream_idx {
            args.extend([
                "-map".into(),
                format!("0:{}", idx),
                "-c:v".into(),
                "copy".into(),
                "-disposition:v:0".into(),
                "attached_pic".into(),
            ]);
        }
    }

    // Explicit tag copy (source tags take priority)
    for (k, v) in &info.tags {
        args.push("-metadata".into());
        // Vorbis Comment (FLAC/OPUS) conventionally uses UPPERCASE keys
        let key = if matches!(format, "flac" | "opus") {
            k.to_uppercase()
        } else {
            k.clone()
        };
        args.push(format!("{}={}", key, v));
    }

    // Silence trim (-af silenceremove) — only apply when silence actually exists
    let trim_enabled = settings.silence_trim_enabled;
    if trim_enabled {
        let dur = settings.silence_trim_duration_ms as f64 / 1000.0;
        let db  = settings.silence_trim_db;
        let (has_start, has_end) = detect_boundary_silence(input, db, dur, info.duration_secs);

        if has_start || has_end {
            let start_part = if has_start {
                format!("start_periods=1:start_silence={dur:.4}:start_threshold={db}dB")
            } else {
                String::new()
            };
            let stop_part = if has_end {
                format!("stop_periods=-1:stop_silence={dur:.4}:stop_threshold={db}dB")
            } else {
                String::new()
            };

            let filter = if has_start && has_end {
                format!("silenceremove={start_part}:{stop_part}")
            } else if has_start {
                format!("silenceremove={start_part}")
            } else {
                format!("silenceremove={stop_part}")
            };

            args.extend(["-af".into(), filter]);
        }
    }

    args.extend(build_codec_args(format, settings, info));

    // Progress: Unix → stdout pipe; Windows → 一時ファイル経由
    // CREATE_NO_WINDOW 環境では pipe:1 が正しく動作しないため Windows のみ別方式を使用
    #[cfg(not(windows))]
    {
        args.push("-progress".into());
        args.push("pipe:1".into());
    }
    #[cfg(windows)]
    let progress_path = {
        let mut p = std::env::temp_dir();
        let id = PROGRESS_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("oto_p{}.txt", id));
        args.push("-progress".into());
        args.push(p.to_string_lossy().into_owned());
        p
    };
    args.push("-nostats".into());

    // args[..3] = [-threads, N, -y]; args[3..] = [-map_metadata … -nostats]
    // Input/output are passed as OsStr via .arg() to handle non-UTF-8 filenames.
    let mut cmd = tokio::process::Command::new(&ffmpeg);
    cmd.args(&args[..3])
       .arg("-i")
       .arg(input)
       .args(&args[3..])
       .arg(output);
    #[cfg(not(windows))]
    {
        cmd.stdout(Stdio::piped())
           .stderr(Stdio::piped());
    }
    #[cfg(windows)]
    {
        cmd.stdin(Stdio::null())
           .stdout(Stdio::null())   // プログレスは一時ファイルへ。stdout は不使用
           .stderr(Stdio::piped())
           .creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.kill_on_drop(true);
    let mut child = cmd.spawn()
        .map_err(|e| anyhow!("failed to spawn ffmpeg: {}", e))?;

    if let Some(pid) = child.id() {
        on_pid(pid);
    }

    let stderr_task = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut buf = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                buf.push(line);
            }
            buf.join("\n")
        })
    });

    #[cfg(not(windows))]
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(value) = line.strip_prefix("out_time_us=") {
                let out_us: u64 = value.trim().parse().unwrap_or(0);
                if duration_secs > 0.0 {
                    let ratio = (out_us as f64 / 1_000_000.0) / duration_secs;
                    on_progress(ratio.min(1.0));
                }
            }
        }
        // OPUS は pre-skip の影響で最終 out_time_us が duration を下回ることがある。
        // child.wait() の await yield を利用して受信側タスクに確実に 1.0 を届ける。
        on_progress(1.0);
    }

    // Windows: 一時ファイルを 200ms 間隔でポーリングしてプログレスを更新
    // ffmpeg が "progress=end" を書くか stderr タスクが終了したらループを抜ける
    #[cfg(windows)]
    {
        let mut prev_us = 0u64;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            if let Ok(content) = tokio::fs::read_to_string(&progress_path).await {
                let mut last_us = 0u64;
                for line in content.lines() {
                    if let Some(val) = line.strip_prefix("out_time_us=") {
                        last_us = val.trim().parse().unwrap_or(0);
                    }
                }
                if last_us > prev_us && duration_secs > 0.0 {
                    prev_us = last_us;
                    let ratio = (last_us as f64 / 1_000_000.0) / duration_secs;
                    on_progress(ratio.min(1.0));
                }
                if content.lines().any(|l| l.trim() == "progress=end") {
                    break;
                }
            }
            // ffmpeg が終了した場合（正常・異常問わず）確実にループを抜ける
            if child.try_wait().map(|opt| opt.is_some()).unwrap_or(false) {
                // 終了直後の最終プログレスを取り逃さないよう再読込
                if let Ok(content) = tokio::fs::read_to_string(&progress_path).await {
                    let mut last_us = 0u64;
                    for line in content.lines() {
                        if let Some(val) = line.strip_prefix("out_time_us=") {
                            last_us = val.trim().parse().unwrap_or(0);
                        }
                    }
                    if last_us > prev_us && duration_secs > 0.0 {
                        let ratio = (last_us as f64 / 1_000_000.0) / duration_secs;
                        on_progress(ratio.min(1.0));
                    }
                }
                break;
            }
        }
        on_progress(1.0);
        let _ = tokio::fs::remove_file(&progress_path).await;
    }

    let status = child.wait().await?;
    let stderr_text = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };

    if !status.success() {
        let tail: String = stderr_text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(anyhow!("{}", if tail.is_empty() { "unknown error (no stderr)".into() } else { tail }));
    }

    if settings.source_file_action == SourceFileAction::Delete && output.exists() {
        if let Ok(meta) = std::fs::metadata(output) {
            if meta.len() > 0 {
                let _ = std::fs::remove_file(input);
            }
        }
    }

    output_guard.keep = true; // 正常完了：出力ファイルを保持
    Ok(())
}

/// UNCパスまたはマップ済みネットワークドライブが入力に含まれるか判定する
#[cfg(windows)]
fn has_network_input(paths: &[String]) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    const DRIVE_REMOTE: u32 = 4;
    for path in paths {
        if path.starts_with("\\\\") {
            return true;
        }
        let mut chars = path.chars();
        if let (Some(d), Some(':')) = (chars.next(), chars.next()) {
            if d.is_ascii_alphabetic() {
                let root: Vec<u16> = format!("{}:\\\0", d.to_ascii_uppercase())
                    .encode_utf16()
                    .collect();
                if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(not(windows))]
fn has_network_input(_paths: &[String]) -> bool { false }

// --- Main conversion runner ---

pub async fn run_conversion(
    app: AppHandle,
    job_id: String,
    request: ConvertRequest,
    settings: Settings,
    pgids: Arc<tokio::sync::Mutex<Vec<i32>>>,
) {
    let format = if request.mode == "decode" {
        // DECODE モードは wav または aiff のみ許可、それ以外はデフォルト wav
        match request.format.as_str() {
            "aiff" => "aiff".to_string(),
            _ => "wav".to_string(),
        }
    } else {
        request.format.clone()
    };

    // Collect files
    let all_paths = collect_audio_files(&request.paths);

    if all_paths.is_empty() {
        if app.emit(
            "conversion_complete",
            CompletionPayload {
                job_id,
                results: vec![],
                success_count: 0,
                error_count: 0,
            },
        ).is_err() {
            eprintln!("emit conversion_complete failed");
        }
        return;
    }

    // Reject files exceeding 10 GiB
    let mut skip_results: Vec<FR> = Vec::new();
    let mut file_paths: Vec<PathBuf> = Vec::new();
    for path in all_paths {
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_FILE_SIZE => {
                skip_results.push(FR::error(
                    path.to_string_lossy(),
                    format!("File size exceeds 10 GiB limit ({:.1} GiB)", meta.len() as f64 / 1_073_741_824.0),
                ));
            }
            Ok(_) => file_paths.push(path),
            Err(e) => {
                skip_results.push(FR::error(path.to_string_lossy(), e.to_string()));
            }
        }
    }

    // ドロップされた元パスの親ディレクトリを基点にすることで、
    // ドロップしたフォルダ名自体も出力パスに含まれるようにする
    let base_dir: Option<PathBuf> = if settings.preserve_folder_structure
        && settings.output_dest != OutputDest::SourceFolder
    {
        let drop_paths: Vec<PathBuf> = request.paths.iter().map(PathBuf::from).collect();
        common_ancestor(&drop_paths)
    } else {
        None
    };

    let settings = Arc::new(settings);
    let job_id = Arc::new(job_id);
    // ネットワーク入力を検出した場合は並列数を1に制限して帯域飽和を防止
    let effective_parallel = if has_network_input(&request.paths) { 1 } else { settings.parallel_count.max(1) };
    // 並列数に応じてCPUスレッドを均等配分（1ジョブあたりのスレッド数）
    let cpu_count = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads_per_job = (cpu_count / effective_parallel).max(1);
    let sem = Arc::new(Semaphore::new(effective_parallel));
    let dialog_sem = Arc::new(Semaphore::new(1)); // ダイアログは同時1件

    // フェーズ1: 全ファイルを probe（probe 専用セマフォでネットワーク帯域を保護）
    let probe_sem = Arc::new(Semaphore::new(effective_parallel));
    let mut probe_set: JoinSet<(PathBuf, Result<FileInfo, String>)> = JoinSet::new();
    for path in file_paths {
        let probe_sem = probe_sem.clone();
        probe_set.spawn(async move {
            let _permit = probe_sem.acquire().await.unwrap();
            let result = probe_file(&path).await;
            (path, result)
        });
    }

    let mut non_media: Vec<FR> = Vec::new();
    let mut media_files: Vec<(PathBuf, FileInfo)> = Vec::new();
    while let Some(Ok((path, result))) = probe_set.join_next().await {
        match result {
            Ok(info) => {
                if info.has_media {
                    media_files.push((path, info));
                } else {
                    non_media.push(FR::skipped(path.to_string_lossy()));
                }
            }
            Err(e) => {
                non_media.push(FR::error(path.to_string_lossy(), e));
            }
        }
    }

    // フェーズ2: 同階層・同ステム重複を除去し最良ソースを選択
    let (selected, rejected_paths) = select_best_sources(media_files);
    let rejected_results: Vec<FR> = rejected_paths
        .iter()
        .map(|p| FR::skipped(p.to_string_lossy()))
        .collect();

    // フェーズ3: 選択ファイルを並列変換
    let selected_count = selected.len();
    let progress_secs = Arc::new(tokio::sync::Mutex::new(vec![0.0f64; selected_count]));
    let total_dur: f64 = selected
        .iter()
        .map(|(_, info)| info.duration_secs)
        .sum::<f64>()
        .max(1.0);
    let total_duration = Arc::new(tokio::sync::Mutex::new(total_dur));

    let mut conv_set: JoinSet<FR> = JoinSet::new();
    for (new_i, (path, info)) in selected.into_iter().enumerate() {
        let sem = sem.clone();
        let app = app.clone();
        let job_id = job_id.clone();
        let format = format.clone();
        let settings = settings.clone();
        let progress_secs = progress_secs.clone();
        let total_duration = total_duration.clone();
        let file_duration = info.duration_secs;
        let pgids_for_spawn = pgids.clone();
        let base_dir = base_dir.clone();
        let dialog_sem = dialog_sem.clone();

        conv_set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            let output_path = match resolve_output_path(
                &path, &format, &settings, base_dir.as_deref(), &app, &dialog_sem,
            ).await {
                Ok(p) => p,
                Err(e) => return FR::error(path.to_string_lossy(), e.to_string()),
            };

            let input_display = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            // watch channel でスロットリング（unbounded spawn を排除）
            let (progress_tx, mut progress_rx) = watch::channel(0.0f64);
            let app_w = app.clone();
            let job_id_w = job_id.clone();
            let ps_w = progress_secs.clone();
            let td_w = total_duration.clone();
            let name_w = input_display.clone();
            tokio::spawn(async move {
                while progress_rx.changed().await.is_ok() {
                    let ratio = *progress_rx.borrow_and_update();
                    let secs = ratio * file_duration;
                    let percent = {
                        let mut ps = ps_w.lock().await;
                        ps[new_i] = secs;
                        let td = *td_w.lock().await;
                        (ps.iter().sum::<f64>() / td * 100.0).min(100.0)
                    };
                    if app_w.emit("progress", ProgressPayload {
                        job_id: (*job_id_w).clone(),
                        percent,
                        current_file: name_w.clone(),
                        file_index: new_i,
                        file_count: selected_count,
                    }).is_err() {
                        eprintln!("emit progress failed");
                    }
                }
            });

            let result = convert_one(
                &path,
                &output_path,
                &format,
                &settings,
                &info,
                file_duration,
                threads_per_job,
                move |ratio| { let _ = progress_tx.send(ratio); },
                move |pid| {
                    let p = pgids_for_spawn.clone();
                    tokio::spawn(async move { p.lock().await.push(pid as i32); });
                },
            )
            .await;

            {
                let mut ps = progress_secs.lock().await;
                ps[new_i] = file_duration;
            }

            match result {
                Ok(()) => FR {
                    input_path: path.to_string_lossy().into(),
                    output_path: output_path.to_string_lossy().into(),
                    success: true,
                    skipped: false,
                    error: None,
                },
                Err(e) => FR {
                    input_path: path.to_string_lossy().into(),
                    output_path: output_path.to_string_lossy().into(),
                    success: false,
                    skipped: false,
                    error: Some(e.to_string()),
                },
            }
        });
    }

    let mut results: Vec<FR> = skip_results;
    results.extend(non_media);
    results.extend(rejected_results);
    while let Some(Ok(result)) = conv_set.join_next().await {
        results.push(result);
    }

    let success_count = results.iter().filter(|r| r.success).count();
    let error_count = results.iter().filter(|r| !r.success && !r.skipped).count();

    // 変換完了後に出力先をファイルマネージャで表示
    if settings.open_in_finder {
        if let Some(first_success) = results.iter().find(|r| r.success) {
            let path = &first_success.output_path;
            #[cfg(target_os = "macos")]
            let _ = tokio::process::Command::new("open").arg("-R").arg(path).spawn();
            #[cfg(target_os = "windows")]
            let _ = tokio::process::Command::new("explorer")
                .arg(format!("/select,{}", path))
                .spawn();
            #[cfg(target_os = "linux")]
            let _ = tokio::process::Command::new("xdg-open")
                .arg(std::path::Path::new(path).parent().unwrap_or(std::path::Path::new(".")))
                .spawn();
        }
    }

    // Emit final 100% progress before switching to standby
    if app.emit(
        "progress",
        ProgressPayload {
            job_id: (*job_id).clone(),
            percent: 100.0,
            current_file: String::new(),
            file_index: selected_count,
            file_count: selected_count,
        },
    ).is_err() {
        eprintln!("emit progress failed");
    }

    if app.emit(
        "conversion_complete",
        CompletionPayload {
            job_id: (*job_id).clone(),
            results,
            success_count,
            error_count,
        },
    ).is_err() {
        eprintln!("emit conversion_complete failed");
    }
}
