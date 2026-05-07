use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use walkdir::WalkDir;

use crate::settings::{NameConflict, OutputDest, Settings, SourceFileAction};

const SUPPORTED_EXTS: &[&str] = &[
    "wav", "mp3", "m4a", "aac", "flac", "mp4", "ogg", "opus", "wma", "aiff", "aif", "alac",
];

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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionPayload {
    pub job_id: String,
    pub results: Vec<FileResult>,
    pub success_count: usize,
    pub error_count: usize,
}

struct FileInfo {
    path: PathBuf,
    duration_secs: f64,
    tags: HashMap<String, String>,
    bits_per_sample: u32,
}

// --- FFmpeg/FFprobe path resolution ---

fn ffmpeg_path() -> String {
    for p in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"] {
        if Path::new(p).exists() {
            return p.to_string();
        }
    }
    "ffmpeg".to_string()
}

fn ffprobe_path() -> String {
    for p in ["/opt/homebrew/bin/ffprobe", "/usr/local/bin/ffprobe"] {
        if Path::new(p).exists() {
            return p.to_string();
        }
    }
    "ffprobe".to_string()
}

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
                if let Some(ext) = entry.path().extension() {
                    let ext_lower = ext.to_string_lossy().to_lowercase();
                    if SUPPORTED_EXTS.contains(&ext_lower.as_str()) {
                        files.push(entry.path().to_path_buf());
                    }
                }
            }
        } else if path.is_file() {
            if let Some(ext) = path.extension() {
                let ext_lower = ext.to_string_lossy().to_lowercase();
                if SUPPORTED_EXTS.contains(&ext_lower.as_str()) {
                    files.push(path);
                }
            }
        }
    }
    files
}

// --- FFprobe info ---

async fn probe_file(path: &Path) -> FileInfo {
    let ffprobe = ffprobe_path();
    let output = tokio::process::Command::new(&ffprobe)
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-select_streams",
            "a:0",
            path.to_str().unwrap_or(""),
        ])
        .output()
        .await
        .ok();

    let mut duration = 0.0f64;
    let mut tags = HashMap::new();
    let mut bits_per_sample = 16u32;

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
            // Also check stream-level tags
            if let Some(stream_tags) = json["streams"][0]["tags"].as_object() {
                for (k, v) in stream_tags {
                    if let Some(s) = v.as_str() {
                        tags.entry(k.to_lowercase()).or_insert_with(|| s.to_string());
                    }
                }
            }
            if let Some(bps) = json["streams"][0]["bits_per_raw_sample"]
                .as_str()
                .and_then(|s| s.parse::<u32>().ok())
                .or_else(|| json["streams"][0]["bits_per_raw_sample"].as_u64().map(|v| v as u32))
            {
                if bps > 0 {
                    bits_per_sample = bps;
                }
            }
        }
    }

    FileInfo {
        path: path.to_path_buf(),
        duration_secs: duration,
        tags,
        bits_per_sample,
    }
}

// --- Output path resolution ---

fn resolve_output_path(
    input: &Path,
    format: &str,
    settings: &Settings,
) -> Result<PathBuf> {
    let stem = input
        .file_stem()
        .ok_or_else(|| anyhow!("invalid filename"))?
        .to_string_lossy();

    let output_dir = match &settings.output_dest {
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

    std::fs::create_dir_all(&output_dir)?;

    let filename = format!("{}.{}", stem, format);
    let candidate = output_dir.join(&filename);

    if !candidate.exists() || settings.name_conflict == NameConflict::ForceOverwrite {
        return Ok(candidate);
    }

    match settings.name_conflict {
        NameConflict::AutoRename | NameConflict::ConfirmDialog => {
            let mut i = 1u32;
            loop {
                let name = format!("{}_{}.{}", stem, i, format);
                let path = output_dir.join(&name);
                if !path.exists() {
                    return Ok(path);
                }
                i += 1;
            }
        }
        NameConflict::ForceOverwrite => Ok(candidate),
    }
}

// --- FFmpeg codec args ---

fn build_codec_args(format: &str, settings: &Settings, info: &FileInfo) -> Vec<String> {
    match format {
        "mp3" => vec![
            "-c:a".into(),
            "libmp3lame".into(),
            "-b:a".into(),
            format!("{}k", settings.mp3_bitrate),
        ],
        "m4a" => vec![
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            format!("{}k", settings.m4a_bitrate),
        ],
        "flac" => vec![
            "-c:a".into(),
            "flac".into(),
            "-compression_level".into(),
            settings.flac_compression.to_string(),
        ],
        "wav" => {
            let pcm_codec = match info.bits_per_sample {
                24 => "pcm_s24le",
                32 => "pcm_s32le",
                _ => "pcm_s16le",
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
    let ffmpeg = ffmpeg_path();
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        input.to_str().unwrap().to_string(),
        "-map_metadata".into(),
        "0".into(),
        "-map".into(),
        "0:a".into(),
    ];

    // カバーアート（埋め込み画像）の引き継ぎ
    // WAVはコンテナ仕様上カバーアート非対応。他フォーマットは画像ストリームをそのままコピーして埋め込む。
    // -c:v copy でJPEGを再エンコードせずに保持し、attached_pic でカバーアートとしてマーク。
    if format != "wav" {
        args.extend([
            "-map".into(),
            "0:v?".into(),
            "-c:v".into(),
            "copy".into(),
            "-disposition:v:0".into(),
            "attached_pic".into(),
        ]);
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

    args.push(output.to_str().unwrap().to_string());

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
        "wav".to_string()
    } else {
        request.format.clone()
    };

    // Collect files
    let all_paths = collect_audio_files(&request.paths);

    if all_paths.is_empty() {
        let _ = app.emit(
            "conversion_complete",
            CompletionPayload {
                job_id,
                results: vec![],
                success_count: 0,
                error_count: 0,
            },
        );
        return;
    }

    // Reject files exceeding 10 GiB
    let mut skip_results: Vec<FileResult> = Vec::new();
    let mut file_paths: Vec<PathBuf> = Vec::new();
    for path in all_paths {
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_FILE_SIZE => {
                skip_results.push(FileResult {
                    input_path: path.to_string_lossy().into(),
                    output_path: String::new(),
                    success: false,
                    error: Some(format!(
                        "File size exceeds 10 GiB limit ({:.1} GiB)",
                        meta.len() as f64 / 1_073_741_824.0
                    )),
                });
            }
            _ => file_paths.push(path),
        }
    }

    let file_count = file_paths.len();

    if file_count == 0 {
        let error_count = skip_results.len();
        let _ = app.emit(
            "conversion_complete",
            CompletionPayload {
                job_id,
                results: skip_results,
                success_count: 0,
                error_count,
            },
        );
        return;
    }

    // Probe all files in parallel (order preserved via indexed join)
    let probe_handles: Vec<_> = file_paths
        .iter()
        .map(|p| {
            let path = p.clone();
            tokio::spawn(async move { probe_file(&path).await })
        })
        .collect();

    let mut file_infos: Vec<FileInfo> = Vec::with_capacity(file_count);
    for h in probe_handles {
        file_infos.push(h.await.unwrap_or_else(|_| FileInfo {
            path: PathBuf::new(),
            duration_secs: 0.0,
            tags: HashMap::new(),
            bits_per_sample: 16,
        }));
    }

    let total_duration: f64 = file_infos.iter().map(|i| i.duration_secs).sum::<f64>().max(1.0);

    // Shared progress state: each file's completed seconds
    let progress_secs = Arc::new(tokio::sync::Mutex::new(vec![0.0f64; file_count]));

    let sem = Arc::new(Semaphore::new(settings.parallel_count.max(1)));
    let mut join_set: JoinSet<FileResult> = JoinSet::new();

    for (i, info) in file_infos.into_iter().enumerate() {
        let sem = sem.clone();
        let app = app.clone();
        let job_id = job_id.clone();
        let format = format.clone();
        let settings = settings.clone();
        let progress_secs = progress_secs.clone();
        let input_path = info.path.clone();
        let file_duration = info.duration_secs;
        let pgids_for_spawn = pgids.clone();

        join_set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            let output_path = match resolve_output_path(&input_path, &format, &settings) {
                Ok(p) => p,
                Err(e) => {
                    return FileResult {
                        input_path: input_path.to_string_lossy().into(),
                        output_path: String::new(),
                        success: false,
                        error: Some(e.to_string()),
                    }
                }
            };

            let app2 = app.clone();
            let job_id2 = job_id.clone();
            let progress_secs2 = progress_secs.clone();
            let input_display = input_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            let result = convert_one(
                &input_path,
                &output_path,
                &format,
                &settings,
                &info,
                file_duration,
                move |ratio| {
                    let secs = ratio * file_duration;
                    let app = app2.clone();
                    let job_id = job_id2.clone();
                    let progress_secs = progress_secs2.clone();
                    let name = input_display.clone();
                    tokio::spawn(async move {
                        {
                            let mut ps = progress_secs.lock().await;
                            ps[i] = secs;
                        }
                        let completed: f64 = progress_secs.lock().await.iter().sum();
                        let percent = (completed / total_duration * 100.0).min(100.0);
                        let _ = app.emit(
                            "progress",
                            ProgressPayload {
                                job_id,
                                percent,
                                current_file: name,
                                file_index: i,
                                file_count,
                            },
                        );
                    });
                },
                move |pid| {
                    let p = pgids_for_spawn.clone();
                    tokio::spawn(async move { p.lock().await.push(pid as i32); });
                },
            )
            .await;

            // Mark this file as fully done for accurate final progress
            {
                let mut ps = progress_secs.lock().await;
                ps[i] = file_duration;
            }

            match result {
                Ok(()) => FileResult {
                    input_path: input_path.to_string_lossy().into(),
                    output_path: output_path.to_string_lossy().into(),
                    success: true,
                    error: None,
                },
                Err(e) => FileResult {
                    input_path: input_path.to_string_lossy().into(),
                    output_path: output_path.to_string_lossy().into(),
                    success: false,
                    error: Some(e.to_string()),
                },
            }
        });
    }

    let mut results: Vec<FileResult> = skip_results;
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(r) => results.push(r),
            Err(e) => results.push(FileResult {
                input_path: String::new(),
                output_path: String::new(),
                success: false,
                error: Some(e.to_string()),
            }),
        }
    }

    let success_count = results.iter().filter(|r| r.success).count();
    let error_count = results.len() - success_count;

    // Open in Finder for the first successful output
    if settings.open_in_finder {
        if let Some(first_success) = results.iter().find(|r| r.success) {
            let _ = tokio::process::Command::new("open")
                .arg("-R")
                .arg(&first_success.output_path)
                .spawn();
        }
    }

    // Emit final 100% progress before switching to standby
    let _ = app.emit(
        "progress",
        ProgressPayload {
            job_id: job_id.clone(),
            percent: 100.0,
            current_file: String::new(),
            file_index: file_count,
            file_count,
        },
    );

    let _ = app.emit(
        "conversion_complete",
        CompletionPayload {
            job_id,
            results,
            success_count,
            error_count,
        },
    );
}
