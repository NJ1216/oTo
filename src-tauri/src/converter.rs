use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use walkdir::WalkDir;

use crate::settings::{NameConflict, OutputDest, Settings, SourceFileAction};

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvertRequest {
    pub paths: Vec<String>,
    pub mode: String,
    pub format: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPayload {
    pub job_id: String,
    pub percent: f64,
    pub current_file: String,
    pub file_index: usize,
    pub file_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileResult {
    pub input_path: String,
    pub output_path: String,
    pub success: bool,
    pub error: Option<String>,
}

impl FileResult {
    fn error(input_path: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            input_path: input_path.into(),
            output_path: String::new(),
            success: false,
            error: Some(msg.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionPayload {
    pub job_id: String,
    pub results: Vec<FileResult>,
    pub success_count: usize,
    pub error_count: usize,
}

struct FileInfo {
    duration_secs: f64,
    tags: HashMap<String, String>,
    bits_per_sample: u32,
    cover_art_stream_idx: Option<usize>, // MJPEG/PNG attached_pic のストリームインデックス
    has_media: bool,
}

// --- FFmpeg/FFprobe path resolution ---

fn resolve_binary(name: &str) -> String {
    for dir in ["/opt/homebrew/bin/", "/usr/local/bin/"] {
        let path = format!("{}{}", dir, name);
        if Path::new(&path).exists() {
            return path;
        }
    }
    name.to_string()
}

fn ffmpeg_path() -> String { resolve_binary("ffmpeg") }
fn ffprobe_path() -> String { resolve_binary("ffprobe") }

// --- File collection ---

pub fn collect_audio_files(paths: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path_str in paths {
        let path = PathBuf::from(path_str);
        if path.is_dir() {
            for entry in WalkDir::new(&path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                files.push(entry.path().to_path_buf());
            }
        } else if path.is_file() {
            files.push(path);
        }
    }
    files.sort(); // 辞書順で安定化
    files
}

fn common_ancestor(paths: &[PathBuf]) -> Option<PathBuf> {
    let dirs: Vec<&Path> = paths.iter().filter_map(|p| p.parent()).collect();
    if dirs.is_empty() {
        return None;
    }
    let mut ancestor = dirs[0].to_path_buf();
    for dir in &dirs[1..] {
        while !dir.starts_with(&ancestor) {
            ancestor = ancestor.parent()?.to_path_buf();
        }
    }
    Some(ancestor)
}

// --- FFprobe info ---

async fn probe_file(path: &Path) -> FileInfo {
    let ffprobe = ffprobe_path();
    let output = match tokio::process::Command::new(&ffprobe)
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            path.to_str().unwrap_or(""),
        ])
        .output()
        .await
    {
        Ok(o) => Some(o),
        Err(e) => {
            eprintln!("ffprobe spawn failed for {}: {e}", path.display());
            None
        }
    };

    let mut duration = 0.0f64;
    let mut tags = HashMap::new();
    let mut bits_per_sample = 16u32;
    let mut cover_art_stream_idx: Option<usize> = None;
    let mut has_media = false;

    if let Some(out) = output {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            if let Some(d) = json["format"]["duration"].as_str() {
                duration = d.parse().unwrap_or(0.0);
            }
            if let Some(tag_obj) = json["format"]["tags"].as_object() {
                for (k, v) in tag_obj {
                    if let Some(s) = v.as_str() {
                        tags.insert(k.to_lowercase(), s.to_string());
                    }
                }
            }
            if let Some(streams) = json["streams"].as_array() {
                for (stream_idx, stream) in streams.iter().enumerate() {
                    match stream["codec_type"].as_str().unwrap_or("") {
                        "audio" => {
                            has_media = true;
                            if let Some(stream_tags) = stream["tags"].as_object() {
                                for (k, v) in stream_tags {
                                    if let Some(s) = v.as_str() {
                                        tags.entry(k.to_lowercase()).or_insert_with(|| s.to_string());
                                    }
                                }
                            }
                            if let Some(bps) = stream["bits_per_raw_sample"]
                                .as_str()
                                .and_then(|s| s.parse::<u32>().ok())
                                .or_else(|| stream["bits_per_raw_sample"].as_u64().map(|v| v as u32))
                            {
                                if bps > 0 {
                                    bits_per_sample = bps;
                                }
                            }
                        }
                        "video" => {
                            // MJPEG/PNGのattached_picのみカバーアートとして扱う
                            // H264などのattached_picはMP3/FLACコンテナが非対応でエラーになるため除外
                            if stream["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 1
                                && cover_art_stream_idx.is_none()
                            {
                                let codec = stream["codec_name"].as_str().unwrap_or("");
                                if matches!(codec, "mjpeg" | "png") {
                                    cover_art_stream_idx = Some(stream_idx);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    FileInfo {
        duration_secs: duration,
        tags,
        bits_per_sample,
        cover_art_stream_idx,
        has_media,
    }
}

// --- Output path resolution ---

async fn ask_overwrite_dialog(app: &AppHandle, filename: &str) -> bool {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    app.dialog()
        .message(format!("\"{}\" はすでに存在します。上書きしますか？", filename))
        .title("ファイルの競合")
        .buttons(MessageDialogButtons::OkCancelCustom("上書き".into(), "別名保存".into()))
        .show(move |result| { let _ = tx.send(result); });
    rx.await.unwrap_or(false)
}

async fn resolve_output_path(
    input: &Path,
    format: &str,
    settings: &Settings,
    base_dir: Option<&Path>,
    app: &AppHandle,
    dialog_sem: &Semaphore,
) -> Result<PathBuf> {
    let stem = input
        .file_stem()
        .ok_or_else(|| anyhow!("invalid filename"))?
        .to_string_lossy();

    let mut output_dir = match &settings.output_dest {
        OutputDest::SourceFolder => input
            .parent()
            .ok_or_else(|| anyhow!("no parent dir"))?
            .to_path_buf(),
        OutputDest::Desktop => dirs::desktop_dir().ok_or_else(|| anyhow!("no desktop dir"))?,
        OutputDest::Downloads => {
            dirs::download_dir().ok_or_else(|| anyhow!("no downloads dir"))?
        }
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

    // ALAC・AAC は M4A コンテナを使う
    let ext = match format {
        "alac" | "aac" => "m4a",
        other => other,
    };
    let filename = format!("{}.{}", stem, ext);
    let candidate = output_dir.join(&filename);

    if !candidate.exists() || settings.name_conflict == NameConflict::ForceOverwrite {
        return Ok(candidate);
    }

    match settings.name_conflict {
        NameConflict::AutoRename => {
            let mut i = 1u32;
            loop {
                let name = format!("{}_{}.{}", stem, i, ext);
                let path = output_dir.join(&name);
                if !path.exists() { return Ok(path); }
                i += 1;
            }
        }
        NameConflict::ConfirmDialog => {
            let _permit = dialog_sem.acquire().await;
            let display = candidate.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| filename.clone());
            if ask_overwrite_dialog(app, &display).await {
                return Ok(candidate); // 上書き
            }
            // 別名保存
            let mut i = 1u32;
            loop {
                let name = format!("{}_{}.{}", stem, i, ext);
                let path = output_dir.join(&name);
                if !path.exists() { return Ok(path); }
                i += 1;
            }
        }
        NameConflict::ForceOverwrite => Ok(candidate),
    }
}

// --- FFmpeg codec args ---

fn build_codec_args(format: &str, settings: &Settings, info: &FileInfo) -> Vec<String> {
    match format {
        "mp3" => {
            let mut args = vec!["-c:a".into(), "libmp3lame".into()];
            if settings.mp3_preset == "custom" {
                if settings.mp3_mode == "vbr" {
                    args.extend(["-q:a".into(), settings.mp3_vbr_quality.to_string()]);
                } else {
                    args.extend(["-b:a".into(), format!("{}k", settings.mp3_bitrate)]);
                }
                if settings.mp3_sample_rate > 0 {
                    args.extend(["-ar".into(), settings.mp3_sample_rate.to_string()]);
                }
                match settings.mp3_channel_mode.as_str() {
                    "mono"   => args.extend(["-ac".into(), "1".into()]),
                    "stereo" => args.extend(["-ac".into(), "2".into()]),
                    _ => {} // "joint_stereo" / "auto" はソースに従う
                }
            } else {
                let bitrate: u32 = settings.mp3_preset.parse().unwrap_or(192);
                args.extend(["-b:a".into(), format!("{}k", bitrate)]);
            }
            args
        }
        "aac" => {
            let mut args = vec!["-c:a".into(), "aac".into()];
            if settings.aac_preset == "custom" {
                if settings.aac_mode == "vbr" {
                    args.extend(["-vbr".into(), settings.aac_vbr_quality.to_string()]);
                } else {
                    args.extend(["-b:a".into(), format!("{}k", settings.m4a_bitrate)]);
                }
                if settings.aac_sample_rate > 0 {
                    args.extend(["-ar".into(), settings.aac_sample_rate.to_string()]);
                }
                match settings.aac_channels {
                    1 => args.extend(["-ac".into(), "1".into()]),
                    2 => args.extend(["-ac".into(), "2".into()]),
                    _ => {}
                }
            } else {
                let bitrate: u32 = settings.aac_preset.parse().unwrap_or(128);
                args.extend(["-b:a".into(), format!("{}k", bitrate)]);
            }
            args
        }
        "opus" => {
            let mut args = vec!["-c:a".into(), "libopus".into()];
            if settings.opus_preset == "custom" {
                if settings.opus_mode == "cbr" {
                    args.extend(["-vbr".into(), "off".into()]);
                }
                args.extend(["-b:a".into(), format!("{}k", settings.opus_bitrate)]);
                args.extend(["-compression_level".into(), settings.opus_complexity.to_string()]);
            } else {
                let bitrate: u32 = settings.opus_preset.parse().unwrap_or(128);
                args.extend(["-b:a".into(), format!("{}k", bitrate)]);
            }
            args
        }
        "flac" => {
            let level: u8 = if settings.flac_preset == "custom" {
                settings.flac_compression
            } else {
                settings.flac_preset.parse().unwrap_or(5)
            };
            vec!["-c:a".into(), "flac".into(), "-compression_level".into(), level.to_string()]
        }
        "alac" => {
            let mut args = vec!["-c:a".into(), "alac".into()];
            if settings.alac_preset == "custom" && settings.alac_bit_depth == 24 {
                args.extend(["-sample_fmt".into(), "s32p".into()]);
            }
            args
        }
        "wav" => {
            let pcm_codec = match info.bits_per_sample {
                24 => "pcm_s24le",
                32 => "pcm_s32le",
                _ => "pcm_s16le",
            };
            vec!["-c:a".into(), pcm_codec.into()]
        }
        "aiff" => {
            let pcm_codec = match info.bits_per_sample {
                24 => "pcm_s24be",
                32 => "pcm_s32be",
                _ => "pcm_s16be",
            };
            vec!["-c:a".into(), pcm_codec.into()]
        }
        _ => vec![],
    }
}

// --- Single file conversion ---

async fn convert_one(
    input: &Path,
    output: &Path,
    format: &str,
    settings: &Settings,
    info: &FileInfo,
    duration_secs: f64,
    on_progress: impl Fn(f64) + Send,
    on_pid: impl Fn(u32) + Send,
) -> Result<()> {
    // キャンセルやエラー時に不完全な出力ファイルを削除するガード
    struct OutputGuard {
        path: PathBuf,
        keep: bool,
    }
    impl Drop for OutputGuard {
        fn drop(&mut self) {
            if !self.keep {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
    let mut output_guard = OutputGuard { path: output.to_path_buf(), keep: false };

    let ffmpeg = ffmpeg_path();
    let cpu_count = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads_per_job = (cpu_count / settings.parallel_count.max(1)).max(1);
    let mut args: Vec<String> = vec![
        "-threads".into(), threads_per_job.to_string(),
        "-y".into(),
        "-i".into(),
        input.to_string_lossy().into_owned(),
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
        args.push(format!("{}={}", k, v));
    }

    args.extend(build_codec_args(format, settings, info));

    // Progress to stdout
    args.push("-progress".into());
    args.push("pipe:1".into());
    args.push("-nostats".into());

    args.push(output.to_string_lossy().into_owned());

    let mut cmd = tokio::process::Command::new(&ffmpeg);
    cmd.args(&args)
       .stdout(Stdio::piped())
       .stderr(Stdio::piped());

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

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
        if duration_secs > 0.0 {
            on_progress(1.0);
        }
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

    if settings.source_file_action == SourceFileAction::Delete {
        let _ = std::fs::remove_file(input);
    }

    output_guard.keep = true; // 正常完了：出力ファイルを保持
    Ok(())
}

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
    let mut skip_results: Vec<FileResult> = Vec::new();
    let mut file_paths: Vec<PathBuf> = Vec::new();
    for path in all_paths {
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_FILE_SIZE => {
                skip_results.push(FileResult::error(
                    path.to_string_lossy(),
                    format!("File size exceeds 10 GiB limit ({:.1} GiB)", meta.len() as f64 / 1_073_741_824.0),
                ));
            }
            Ok(_) => file_paths.push(path),
            Err(e) => {
                skip_results.push(FileResult::error(path.to_string_lossy(), e.to_string()));
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
    let sem = Arc::new(Semaphore::new(settings.parallel_count.max(1)));
    let dialog_sem = Arc::new(Semaphore::new(1)); // ダイアログは同時1件

    // probe 完了と変換完了を tokio::select! でパイプライン処理。
    // probe が終わったファイルから即座に変換タスクをスポーンし、全 probe 完了待ちを排除。
    let file_count = file_paths.len(); // 上限値（非メディアファイル分は progress 進捗に影響しない）
    let progress_secs = Arc::new(tokio::sync::Mutex::new(vec![0.0f64; file_count]));
    let total_duration = Arc::new(tokio::sync::Mutex::new(0.0f64)); // probe 完了ごとに加算

    let mut probe_set: JoinSet<(usize, PathBuf, FileInfo)> = JoinSet::new();
    for (i, path) in file_paths.into_iter().enumerate() {
        probe_set.spawn(async move {
            let info = probe_file(&path).await;
            (i, path, info)
        });
    }

    let mut conv_set: JoinSet<FileResult> = JoinSet::new();
    let mut results: Vec<FileResult> = skip_results;

    loop {
        tokio::select! {
            Some(Ok((i, path, info))) = probe_set.join_next(), if !probe_set.is_empty() => {
                if !info.has_media {
                    results.push(FileResult::error(
                        path.to_string_lossy(),
                        "No audio stream found".to_string(),
                    ));
                    continue;
                }
                {
                    let mut td = total_duration.lock().await;
                    *td = (*td + info.duration_secs).max(1.0);
                }
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
                        Err(e) => return FileResult::error(path.to_string_lossy(), e.to_string()),
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
                                ps[i] = secs;
                                let td = *td_w.lock().await;
                                (ps.iter().sum::<f64>() / td * 100.0).min(100.0)
                            };
                            if app_w.emit("progress", ProgressPayload {
                                job_id: (*job_id_w).clone(),
                                percent,
                                current_file: name_w.clone(),
                                file_index: i,
                                file_count,
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
                        move |ratio| { let _ = progress_tx.send(ratio); },
                        move |pid| {
                            let p = pgids_for_spawn.clone();
                            tokio::spawn(async move { p.lock().await.push(pid as i32); });
                        },
                    )
                    .await;

                    {
                        let mut ps = progress_secs.lock().await;
                        ps[i] = file_duration;
                    }

                    match result {
                        Ok(()) => FileResult {
                            input_path: path.to_string_lossy().into(),
                            output_path: output_path.to_string_lossy().into(),
                            success: true,
                            error: None,
                        },
                        Err(e) => FileResult {
                            input_path: path.to_string_lossy().into(),
                            output_path: output_path.to_string_lossy().into(),
                            success: false,
                            error: Some(e.to_string()),
                        },
                    }
                });
            }
            Some(Ok(result)) = conv_set.join_next(), if !conv_set.is_empty() => {
                results.push(result);
            }
            else => break,
        }
    }

    let success_count = results.iter().filter(|r| r.success).count();
    let error_count = results.len() - success_count;

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
            file_index: file_count,
            file_count,
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
