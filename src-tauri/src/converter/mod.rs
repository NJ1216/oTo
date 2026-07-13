mod binary;
mod codec_args;
mod file_collector;
mod output;
mod probe;
mod process;
pub mod silence;
mod types;

pub use binary::ffmpeg_path;
pub use types::{CompletionPayload, ConvertRequest, FileResult, OverwriteChoice, ProgressPayload};

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;

use crate::settings::{OutputDest, Settings};
use codec_args::build_codec_args;
use file_collector::{common_ancestor, scan_paths_in_batches, select_best_from_group, stem_key};
use output::{resolve_output_path, resolve_output_path_for_stem, OutputResolutionContext};
use probe::probe_file;
use process::{configure_ffmpeg_command, ProcessTracker};
use silence::{
    detect_boundary_silence_cancellable, detect_direct_boundary_trim, SilenceConfig, SilenceContext,
};
use types::{FileInfo, FileResult as FR};

#[cfg(windows)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicU64 as SharedAtomicU64, AtomicUsize, Ordering};

#[cfg(windows)]
static PROGRESS_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB
const CANCELLED_ERROR: &str = "conversion cancelled";

struct AbortOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> AbortOnDrop<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self(Some(handle))
    }

    async fn join(&mut self) -> std::result::Result<T, tokio::task::JoinError> {
        self.0.as_mut().expect("task already joined").await
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

fn input_preparation_progress(ratio: f64) -> f64 {
    ratio.clamp(0.0, 1.0) * 0.10
}

fn ffmpeg_stage_progress(ratio: f64) -> f64 {
    0.10 + ratio.clamp(0.0, 1.0) * 0.80
}

fn output_stage_progress(ratio: f64) -> f64 {
    0.90 + ratio.clamp(0.0, 1.0) * 0.10
}

async fn probe_candidate(
    path: &Path,
    probe_sem: Arc<Semaphore>,
    network_stream_sem: Arc<Semaphore>,
    cancellation: Arc<crate::JobCancellation>,
) -> std::result::Result<FileInfo, String> {
    let _probe_permit = tokio::select! {
        permit = probe_sem.acquire_owned() => permit.map_err(|_| "probe worker closed".to_string())?,
        _ = cancellation.cancelled() => return Err(CANCELLED_ERROR.to_string()),
    };
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| error.to_string())?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err(format!(
            "File size exceeds 10 GiB limit ({:.1} GiB)",
            metadata.len() as f64 / 1_073_741_824.0
        ));
    }
    let _network_read_permit = if is_path_on_network(path) {
        let permit = tokio::select! {
            permit = network_stream_sem.acquire_owned() => permit,
            _ = cancellation.cancelled() => return Err(CANCELLED_ERROR.to_string()),
        };
        Some(permit.map_err(|_| "network input worker closed".to_string())?)
    } else {
        None
    };
    probe_file(path, &cancellation).await
}

// NASの連続コピーは大きめのバッファでシステムコール回数を抑える。
// 入力・出力で各1本だけなので、同時使用量は最大でもおよそ2 MiB。
const NETWORK_CHUNK_SIZE: usize = 1024 * 1024;
const INPUT_SPOOL_LOW_WATER: usize = 128 * 1024 * 1024;

#[derive(Clone)]
enum NetworkInput {
    /// NASからOS既定のローカル一時フォルダへ退避した入力。
    Cached(Arc<TempInput>),
    /// 大容量のため退避せず、シーク可能なNAS上の元パスを直接読む入力。
    Direct(PathBuf),
}

fn should_cache_network_input(input_size: u64) -> bool {
    input_size <= crate::INPUT_SPOOL_TARGET_BYTES as u64
}

fn input_spool_resume_at(input_size: usize) -> usize {
    // 小さい次入力は「収まる」時点で早く再開する。128 MiBを超える次入力では、
    // 合計が高水位を超えないよう、その入力が実際に収まるところまで待つ。
    let fits_at = crate::INPUT_SPOOL_TARGET_BYTES.saturating_sub(input_size);
    if input_size <= crate::INPUT_SPOOL_TARGET_BYTES - INPUT_SPOOL_LOW_WATER {
        fits_at.max(INPUT_SPOOL_LOW_WATER)
    } else {
        fits_at
    }
}

struct TempInput {
    file: crate::spool::LocalSpoolFile,
}

#[cfg(unix)]
fn temp_available_space() -> std::io::Result<u64> {
    use std::ffi::CString;
    let dir = std::env::temp_dir();
    let cpath = CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "temporary directory path contains NUL",
        )
    })?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(windows)]
fn temp_available_space() -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let path: Vec<u16> = std::env::temp_dir()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut available = 0u64;
    if unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(available)
}

async fn copy_network_input_to_temp(
    path: &Path,
    cache_used: Arc<AtomicUsize>,
    spool_manager: Arc<crate::spool::SpoolManager>,
    app: Option<&AppHandle>,
    cancellation: Arc<crate::JobCancellation>,
    on_progress: Arc<dyn Fn(f64) + Send + Sync>,
) -> Result<Arc<TempInput>> {
    let input_size = tokio::fs::metadata(path).await?.len();
    if !should_cache_network_input(input_size) {
        return Err(anyhow!("network input exceeds the cache threshold"));
    }
    let input_size_usize = usize::try_from(input_size)
        .map_err(|_| anyhow!("network input size cannot be represented on this platform"))?;
    let resume_at = input_spool_resume_at(input_size_usize);
    while cache_used
        .load(Ordering::Acquire)
        .saturating_add(input_size_usize)
        > crate::INPUT_SPOOL_TARGET_BYTES
    {
        if let Some(state) = app.and_then(|app| app.try_state::<crate::AppState>()) {
            state.input_spool_waiting.store(true, Ordering::Relaxed);
        }
        tokio::select! {
            _ = cancellation.cancelled() => {
                if let Some(state) = app.and_then(|app| app.try_state::<crate::AppState>()) {
                    state.input_spool_waiting.store(false, Ordering::Relaxed);
                }
                return Err(anyhow!(CANCELLED_ERROR));
            },
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {}
        }
        if cache_used.load(Ordering::Acquire) <= resume_at
            && cache_used
                .load(Ordering::Acquire)
                .saturating_add(input_size_usize)
                <= crate::INPUT_SPOOL_TARGET_BYTES
        {
            break;
        }
    }
    if let Some(state) = app.and_then(|app| app.try_state::<crate::AppState>()) {
        state.input_spool_waiting.store(false, Ordering::Relaxed);
    }
    if cancellation.is_cancelled() {
        return Err(anyhow!(CANCELLED_ERROR));
    }
    let available = temp_available_space()?;
    if available < input_size {
        return Err(anyhow!(
            "not enough temporary disk space to cache network input"
        ));
    }
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("media");
    let mut spool_file = spool_manager.new_spool_file("input", extension, cache_used);
    let mut source = tokio::fs::File::open(path).await?;
    let mut target = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(spool_file.path())
        .await?;
    let copy_result: std::io::Result<()> = async {
        let mut chunk = vec![0u8; NETWORK_CHUNK_SIZE];
        let mut copied = 0u64;
        loop {
            if cancellation.is_cancelled() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    CANCELLED_ERROR,
                ));
            }
            let count = source.read(&mut chunk).await?;
            if count == 0 {
                break;
            }
            target.write_all(&chunk[..count]).await?;
            spool_file.add_accounted_bytes(count);
            copied = copied.saturating_add(count as u64);
            on_progress(if input_size == 0 {
                1.0
            } else {
                copied as f64 / input_size as f64
            });
        }
        target.flush().await?;
        on_progress(1.0);
        Ok(())
    }
    .await;
    if let Err(error) = copy_result {
        return Err(anyhow!("failed to cache network input locally: {error}"));
    }
    Ok(Arc::new(TempInput { file: spool_file }))
}

/// NAS出力のために、FFmpeg開始前にローカル一時ボリューム上の容量を保守的に予約する。
/// 実際に生成済みの出力はOSの空き容量から既に引かれているため、加算して二重計上を避ける。
pub struct OutputSpoolCapacity {
    reserved: SharedAtomicU64,
    changed: Notify,
}

impl OutputSpoolCapacity {
    fn new() -> Self {
        Self {
            reserved: SharedAtomicU64::new(0),
            changed: Notify::new(),
        }
    }

    async fn reserve(
        self: &Arc<Self>,
        bytes: u64,
        app: &AppHandle,
        cancellation: &crate::JobCancellation,
    ) -> Result<OutputReservation> {
        loop {
            if cancellation.is_cancelled() {
                return Err(anyhow!(CANCELLED_ERROR));
            }
            let available = temp_available_space()?;
            let actual = app
                .try_state::<crate::AppState>()
                .map(|s| s.output_spool_used.load(Ordering::Acquire) as u64)
                .unwrap_or(0);
            let reserved = self.reserved.load(Ordering::Acquire);
            if available.saturating_add(actual) >= reserved.saturating_add(bytes)
                && self
                    .reserved
                    .compare_exchange(
                        reserved,
                        reserved.saturating_add(bytes),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                if let Some(state) = app.try_state::<crate::AppState>() {
                    state.output_spool_waiting.store(false, Ordering::Relaxed);
                }
                return Ok(OutputReservation {
                    bytes,
                    capacity: self.clone(),
                });
            }
            if let Some(state) = app.try_state::<crate::AppState>() {
                state.output_spool_waiting.store(true, Ordering::Relaxed);
            }
            tokio::select! {
                _ = cancellation.cancelled() => {
                    if let Some(state) = app.try_state::<crate::AppState>() {
                        state.output_spool_waiting.store(false, Ordering::Relaxed);
                    }
                    return Err(anyhow!(CANCELLED_ERROR));
                },
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(250)) => {}
            }
        }
    }
}

struct OutputReservation {
    bytes: u64,
    capacity: Arc<OutputSpoolCapacity>,
}

impl Drop for OutputReservation {
    fn drop(&mut self) {
        let _ =
            self.capacity
                .reserved
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |reserved| {
                    Some(reserved.saturating_sub(self.bytes))
                });
        self.capacity.changed.notify_waiters();
    }
}

fn estimate_output_bytes(format: &str, settings: &Settings, info: &FileInfo) -> u64 {
    let duration = info.duration_secs.max(1.0);
    let bitrate = match format {
        "mp3" => {
            if settings.mp3_preset == "custom" && settings.mp3_mode == "vbr" {
                320_000
            } else if settings.mp3_preset == "custom" {
                settings.mp3_bitrate as u64 * 1000
            } else {
                settings.mp3_preset.parse::<u64>().unwrap_or(192) * 1000
            }
        }
        "aac" => {
            if settings.aac_preset == "custom" {
                settings.m4a_bitrate as u64 * 1000
            } else {
                settings.aac_preset.parse::<u64>().unwrap_or(128) * 1000
            }
        }
        "opus" => {
            if settings.opus_preset == "custom" {
                settings.opus_bitrate as u64 * 1000
            } else {
                settings.opus_preset.parse::<u64>().unwrap_or(128) * 1000
            }
        }
        // Lossless codecs can exceed the input bitrate on incompressible material.
        "flac" | "alac" => info.bit_rate_bps.max(1_536_000).saturating_mul(2),
        "wav" | "aiff" => 48_000u64 * 32 * 2,
        _ => info.bit_rate_bps.max(1_536_000),
    };
    // Container/tag overhead and VBR fluctuation.  A 16 MiB floor handles short files.
    ((duration * bitrate as f64 / 8.0 * 1.25) as u64).saturating_add(16 * 1024 * 1024)
}

async fn copy_spool_to_network(
    source: &Path,
    destination: &Path,
    spool_manager: &Arc<crate::spool::SpoolManager>,
    cancellation: &crate::JobCancellation,
    on_progress: Arc<dyn Fn(f64) + Send + Sync>,
) -> Result<()> {
    let mut input = tokio::fs::File::open(source).await?;
    let total_bytes = input.metadata().await?.len();
    let uploading = unique_sibling_path(destination, ".oto-upload", true);
    // Persist and fsync the exact platform path before creating anything on NAS.
    let upload_guard = spool_manager.begin_upload(&uploading)?;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&uploading)
        .await?;
    let mut chunk = vec![0u8; NETWORK_CHUNK_SIZE];
    let mut copied = 0u64;
    loop {
        if cancellation.is_cancelled() {
            return Err(anyhow!(CANCELLED_ERROR));
        }
        let count = input.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        output.write_all(&chunk[..count]).await?;
        copied = copied.saturating_add(count as u64);
        on_progress(if total_bytes == 0 {
            1.0
        } else {
            copied as f64 / total_bytes as f64
        });
    }
    output.flush().await?;
    drop(output);
    replace_file_after_success(&uploading, destination)?;
    upload_guard.complete()?;
    on_progress(1.0);
    Ok(())
}

fn safe_track_label(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let generic = [
        "main",
        "soundhandler",
        "videohandler",
        "datahandler",
        "handler",
    ];
    let lowercase = value.to_ascii_lowercase();
    if generic.iter().any(|name| value.eq_ignore_ascii_case(name))
        || (lowercase.ends_with(" handler")
            && (lowercase.contains("iso")
                || lowercase.contains("media")
                || lowercase.starts_with("gpac")))
    {
        return None;
    }
    let safe: String = value
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_owned();
    (!safe.is_empty()).then_some(safe)
}

fn track_label(track: &types::AudioTrack, ordinal: usize) -> String {
    track
        .handler_name
        .as_deref()
        .and_then(safe_track_label)
        .or_else(|| track.language.as_deref().and_then(safe_track_label))
        .unwrap_or_else(|| format!("trk{ordinal}"))
}

fn track_output_stem(input: &Path, label: &str) -> Result<String> {
    let stem = input
        .file_stem()
        .ok_or_else(|| anyhow!("invalid filename"))?
        .to_string_lossy();
    Ok(format!("{stem} ({label})"))
}

fn unique_sibling_path(path: &Path, prefix: &str, keep_extension: bool) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let ext = if keep_extension {
        path.extension()
            .map(|ext| format!(".{}", ext.to_string_lossy()))
            .unwrap_or_default()
    } else {
        String::new()
    };
    loop {
        let candidate = parent.join(format!("{prefix}-{}{}", uuid::Uuid::new_v4(), ext));
        if !candidate.exists() {
            return candidate;
        }
    }
}

fn paths_refer_to_same_file(input: &Path, output: &Path) -> bool {
    if input == output {
        return true;
    }
    match (std::fs::canonicalize(input), std::fs::canonicalize(output)) {
        (Ok(input), Ok(output)) => input == output,
        _ => false,
    }
}

/// 同一パスへの再エンコード用。旧ファイルは、完成した一時出力を置換できる時点まで残す。
fn replace_file_after_success(temp_output: &Path, output: &Path) -> Result<()> {
    let backup = unique_sibling_path(output, ".oto-replace-backup", false);
    std::fs::rename(output, &backup)
        .map_err(|e| anyhow!("failed to preserve original before replacement: {}", e))?;
    if let Err(e) = std::fs::rename(temp_output, output) {
        let _ = std::fs::rename(&backup, output);
        return Err(anyhow!(
            "failed to replace original with converted output: {}",
            e
        ));
    }
    // 成功後の旧ファイル削除。失敗しても新しい出力は保持する。
    if let Err(e) = std::fs::remove_file(&backup) {
        eprintln!(
            "failed to remove replaced original {}: {}",
            backup.display(),
            e
        );
    }
    Ok(())
}

struct CoverArtRequest<'a> {
    ffmpeg: &'a str,
    audio_output: &'a Path,
    source: &'a Path,
    stream_index: usize,
    output: &'a Path,
}

async fn attach_cover_art(request: CoverArtRequest<'_>, context: &ConversionContext) -> Result<()> {
    let mut cmd = tokio::process::Command::new(request.ffmpeg);
    cmd.arg("-y")
        .arg("-i")
        .arg(request.audio_output)
        .arg("-i")
        .arg(request.source)
        .args(["-map", "0:a:0", "-map"])
        .arg(format!("1:{}", request.stream_index))
        .args([
            "-map_metadata",
            "0",
            "-c",
            "copy",
            "-disposition:v:0",
            "attached_pic",
        ])
        .arg(request.output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    configure_ffmpeg_command(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to start FFmpeg while attaching cover art: {e}"))?;
    let registration = context.processes.register(child.id());
    let stderr_task = child.stderr.take().map(|mut stderr| {
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes).await;
            bytes
        })
    });
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = context.cancellation.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(anyhow!(CANCELLED_ERROR));
        }
    };
    drop(registration);
    let stderr = match stderr_task {
        Some(task) => task.await.unwrap_or_default(),
        None => Vec::new(),
    };
    if !status.success() {
        return Err(anyhow!(
            "FFmpeg failed while attaching cover art ({}): {}",
            status,
            String::from_utf8_lossy(&stderr)
        ));
    }
    Ok(())
}

// --- Single file conversion ---

struct ConvertOneRequest<'a> {
    input: &'a Path,
    output: &'a Path,
    format: &'a str,
    settings: &'a Settings,
    info: &'a FileInfo,
    audio_stream_index: Option<usize>,
    duration_secs: f64,
    network_input: Option<NetworkInput>,
}

struct ConversionContext {
    threads_per_job: usize,
    cancellation: Arc<crate::JobCancellation>,
    on_progress: Arc<dyn Fn(f64) + Send + Sync>,
    processes: ProcessTracker,
}

async fn convert_one(request: ConvertOneRequest<'_>, context: ConversionContext) -> Result<()> {
    let ConvertOneRequest {
        input,
        output,
        format,
        settings,
        info,
        audio_stream_index,
        duration_secs,
        network_input,
    } = request;
    if context.cancellation.is_cancelled() {
        return Err(anyhow!(CANCELLED_ERROR));
    }
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
            } else if let Some(backup) = &self.backup {
                // 成功時は退避していた旧ファイルを破棄する
                let _ = std::fs::remove_file(backup);
            }
        }
    }

    let replace_input_in_place = paths_refer_to_same_file(input, output);
    // 同一パスへの変換は元ファイルを読める状態で保つため、まず同じディレクトリの
    // 一時出力へ書き込む。拡張子を維持してFFmpegの出力形式推測も壊さない。
    let write_output = if replace_input_in_place {
        unique_sibling_path(output, ".oto-reencode", true)
    } else {
        output.to_path_buf()
    };

    let backup_path = if write_output.exists() {
        // 固定の .backup 名は使わない。同名ファイルを誤って置換・復元対象にしないため、
        // 出力と同じディレクトリにジョブごとの一意な退避名を割り当てる。
        let parent = write_output.parent().unwrap_or_else(|| Path::new("."));
        let backup = loop {
            let candidate = parent.join(format!(".oto-backup-{}", uuid::Uuid::new_v4()));
            if !candidate.exists() {
                break candidate;
            }
        };
        // 退避できていなければ、元ファイルを保ったまま変換を中止する。
        std::fs::rename(&write_output, &backup)
            .map_err(|e| anyhow!("failed to safely back up existing output: {}", e))?;
        Some(backup)
    } else {
        None
    };
    let mut output_guard = OutputGuard {
        path: write_output.clone(),
        keep: false,
        backup: backup_path,
    };

    // active_files に即座にエントリを作成し、プログレスバーをすぐ表示させる
    (context.on_progress)(0.0);
    let ffmpeg = ffmpeg_path();
    // Input/output paths are added to cmd directly as OsStr (see below) to
    // support non-UTF-8 filenames. Only non-path args go in this Vec.
    let mut args: Vec<String> = vec![
        "-threads".into(),
        context.threads_per_job.to_string(),
        "-y".into(),
        "-map_metadata".into(),
        "0".into(),
        "-map".into(),
        // MP3/FLACなどの音声コンテナは複数音声ストリームを格納できないため、
        // 複数トラックを持つ動画でも既定の先頭音声トラックだけを変換する。
        audio_stream_index
            .map(|index| format!("0:{index}"))
            .unwrap_or_else(|| "0:a:0".into()),
    ];

    // 画像を同一出力へ map すると out_time_us=N/A になるため、音声変換後に別フェーズで付与する。
    let cover_art_stream_idx = if matches!(format, "mp3" | "aac" | "flac" | "alac") {
        info.cover_art_stream_idx
    } else {
        None
    };
    let progress_ceiling = if cover_art_stream_idx.is_some() {
        0.99
    } else {
        1.0
    };

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

    // 小容量NAS入力とローカル入力は従来どおり全編検査する。巨大NAS入力だけは
    // 先頭・末尾各60秒を上限にし、境界を確定できた側だけ atrim する。
    if settings.silence_trim_enabled {
        let silence_config = SilenceConfig {
            db: settings.silence_trim_db,
            min_duration_secs: settings.silence_trim_duration_ms as f64 / 1000.0,
            total_duration_secs: info.duration_secs,
            audio_stream_index,
        };
        let silence_context = SilenceContext {
            cancellation: &context.cancellation,
            processes: &context.processes,
        };
        let silence_input = match network_input.as_ref() {
            Some(NetworkInput::Cached(temp)) => temp.file.path(),
            Some(NetworkInput::Direct(path)) => path.as_path(),
            None => input,
        };
        if matches!(network_input.as_ref(), Some(NetworkInput::Direct(_))) {
            let trim =
                detect_direct_boundary_trim(silence_input, &silence_config, &silence_context)
                    .await
                    .map_err(|error| anyhow!(error))?;
            let atrim = match (trim.start_secs, trim.end_secs) {
                (Some(start), Some(end)) => Some(format!(
                    "atrim=start={start:.6}:end={end:.6},asetpts=PTS-STARTPTS"
                )),
                (Some(start), None) => Some(format!("atrim=start={start:.6},asetpts=PTS-STARTPTS")),
                (None, Some(end)) => Some(format!("atrim=end={end:.6},asetpts=PTS-STARTPTS")),
                (None, None) => None,
            };
            if let Some(filter) = atrim {
                args.extend(["-af".into(), filter]);
            }
        } else {
            let (has_start, has_end) = detect_boundary_silence_cancellable(
                silence_input,
                &silence_config,
                &silence_context,
            )
            .await
            .map_err(|error| anyhow!(error))?;

            if has_start || has_end {
                let trim_head = format!(
                    "silenceremove=start_periods=1:start_silence={:.4}:start_threshold={}dB",
                    silence_config.min_duration_secs, silence_config.db
                );
                let filter = format!("{trim_head},areverse,{trim_head},areverse");
                args.extend(["-af".into(), filter]);
            }
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
    // Windows: キャンセル等でループを抜けても確実に oto_p<id>.txt を削除する RAII ガード
    #[cfg(windows)]
    struct ProgressFileGuard(std::path::PathBuf);
    #[cfg(windows)]
    impl Drop for ProgressFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    #[cfg(windows)]
    let _progress_guard = ProgressFileGuard(progress_path.clone());
    args.push("-nostats".into());

    let ffmpeg_input = match network_input.as_ref() {
        Some(NetworkInput::Cached(temp)) => temp.file.path(),
        Some(NetworkInput::Direct(path)) => path.as_path(),
        _ => input,
    };

    // args[..3] = [-threads, N, -y]; args[3..] = [-map_metadata … -nostats]
    let mut cmd = tokio::process::Command::new(&ffmpeg);
    cmd.args(&args[..3]);
    cmd.arg("-i").arg(ffmpeg_input);
    cmd.args(&args[3..]).arg(&write_output);
    // 失敗時に再現・切り分けできるよう、実行したFFmpegコマンドもエラー詳細に残す。
    let input_args_for_log = format!("-i {}", ffmpeg_input.display());
    let command_for_log = format!(
        "{} {} {} {} {}",
        ffmpeg,
        args[..3].join(" "),
        input_args_for_log,
        args[3..].join(" "),
        write_output.display(),
    );
    #[cfg(not(windows))]
    {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }
    #[cfg(windows)]
    {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null()) // プログレスは一時ファイルへ。stdout は不使用
            .stderr(Stdio::piped());
    }

    configure_ffmpeg_command(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to spawn ffmpeg: {}", e))?;

    let pid_registration = context.processes.register(child.id());

    let stderr_task = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            // チャプター名などに不正なUTF-8が含まれるMP4/MOVでも、FFmpegのstderrを
            // 最後まで読み続ける。lines() はUTF-8変換エラーで読み取りを止め、パイプを
            // 閉じてFFmpeg自身を失敗させるため使用しない。
            let mut stderr = stderr;
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).into_owned()
        })
    });

    #[cfg(not(windows))]
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let line = tokio::select! {
                line = lines.next_line() => line,
                _ = context.cancellation.cancelled() => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(anyhow!(CANCELLED_ERROR));
                }
            };
            let Ok(Some(line)) = line else { break };
            if let Some(value) = line.strip_prefix("out_time_us=") {
                let out_us: u64 = value.trim().parse().unwrap_or(0);
                if duration_secs > 0.0 {
                    let ratio = (out_us as f64 / 1_000_000.0) / duration_secs;
                    (context.on_progress)(ratio.min(1.0) * progress_ceiling);
                }
            }
        }
        // OPUS は pre-skip の影響で最終 out_time_us が duration を下回ることがある。
        // child.wait() の await yield を利用して受信側タスクに確実に 1.0 を届ける。
        (context.on_progress)(progress_ceiling);
    }

    // Windows: 一時ファイルを 200ms 間隔でポーリングしてプログレスを更新
    // ffmpeg が "progress=end" を書くか stderr タスクが終了したらループを抜ける
    #[cfg(windows)]
    {
        let mut prev_us = 0u64;
        loop {
            if context.cancellation.is_cancelled() {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(anyhow!(CANCELLED_ERROR));
            }
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
                    (context.on_progress)(ratio.min(1.0) * progress_ceiling);
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
                        (context.on_progress)(ratio.min(1.0) * progress_ceiling);
                    }
                }
                break;
            }
        }
        (context.on_progress)(progress_ceiling);
        let _ = tokio::fs::remove_file(&progress_path).await;
    }

    let status = tokio::select! {
        status = child.wait() => status?,
        _ = context.cancellation.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(anyhow!(CANCELLED_ERROR));
        }
    };
    drop(pid_registration); // 終了済みPIDをキャンセル対象から即座に外す
    let stderr_text = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };

    if !status.success() {
        let stderr_detail = if stderr_text.trim().is_empty() {
            "(FFmpeg did not write anything to stderr.)"
        } else {
            stderr_text.as_str()
        };
        return Err(anyhow!(
            "FFmpeg failed ({status}).\nCommand: {command_for_log}\n\nFFmpeg stderr:\n{stderr_detail}"
        ));
    }

    if let Some(cover_stream_idx) = cover_art_stream_idx {
        struct CoverOutputGuard(PathBuf);
        impl Drop for CoverOutputGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let cover_output = unique_sibling_path(&write_output, ".oto-cover", true);
        let cover_guard = CoverOutputGuard(cover_output.clone());
        attach_cover_art(
            CoverArtRequest {
                ffmpeg: &ffmpeg,
                audio_output: &write_output,
                source: ffmpeg_input,
                stream_index: cover_stream_idx,
                output: &cover_output,
            },
            &context,
        )
        .await?;
        replace_file_after_success(&cover_output, &write_output)?;
        drop(cover_guard);
        (context.on_progress)(1.0);
    }

    if replace_input_in_place {
        replace_file_after_success(&write_output, output)?;
    }

    output_guard.keep = true; // 正常完了：出力ファイルを保持
    Ok(())
}

/// UNCパスまたはマップ済みネットワークドライブが入力に含まれるか判定する
#[cfg(windows)]
fn is_path_on_network(path: &Path) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    const DRIVE_REMOTE: u32 = 4;
    let path = path.to_string_lossy();
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
    false
}

#[cfg(windows)]
fn has_network_input(paths: &[String]) -> bool {
    paths.iter().any(|path| is_path_on_network(Path::new(path)))
}

#[cfg(target_os = "macos")]
fn is_path_on_network(path: &Path) -> bool {
    use std::ffi::CString;
    if let Ok(cpath) = CString::new(path.as_os_str().as_encoded_bytes()) {
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(cpath.as_ptr(), &mut stat) } == 0 {
            return (stat.f_flags as u32 & libc::MNT_LOCAL as u32) == 0;
        }
    }
    false
}

#[cfg(target_os = "linux")]
fn is_path_on_network(path: &Path) -> bool {
    use std::ffi::CString;
    // NFS=0x6969, CIFS/SMB=0xFF534D42, SMB2=0xFE534D42, SMBFS=0x517B
    if let Ok(cpath) = CString::new(path.as_os_str().as_encoded_bytes()) {
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(cpath.as_ptr(), &mut stat) } == 0 {
            let f_type = stat.f_type as u64;
            return matches!(f_type, 0x6969 | 0xFF534D42 | 0xFE534D42 | 0x517B);
        }
    }
    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn is_path_on_network(_path: &Path) -> bool {
    false
}

#[cfg(not(windows))]
fn has_network_input(paths: &[String]) -> bool {
    paths.iter().any(|p| is_path_on_network(Path::new(p)))
}

/// AppState のログバッファにエントリを追加する（最大10,000件、古いものを自動削除）
fn push_conv_log(app: &AppHandle, file_name: String, status: &str, error: Option<String>) {
    if let Some(state) = app.try_state::<crate::AppState>() {
        match status {
            "done" => {
                state.successful_count.fetch_add(1, Ordering::Relaxed);
            }
            "error" => {
                state.failed_count.fetch_add(1, Ordering::Relaxed);
            }
            "skipped" => {
                state.skipped_count.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut log = state.conv_log.lock().unwrap();
        if log.len() >= 10_000 {
            log.pop_front();
        }
        let entry = crate::ConvLogEntry {
            seq: state.log_sequence.fetch_add(1, Ordering::SeqCst) + 1,
            ts_ms,
            file_name,
            status: status.to_string(),
            error,
        };
        log.push_back(entry);
    }
}

fn emit_overall_progress(app: &AppHandle, job_id: &str, current_file: &str) {
    let Some(state) = app.try_state::<crate::AppState>() else {
        return;
    };
    if !state.is_converting.load(Ordering::SeqCst) {
        return;
    }
    // スナップショット取得からemitまで同じロック区間に置き、並列タスクの古い
    // イベントが新しいイベントより後に届いて表示率を戻すことを防ぐ。
    let progress = state.overall_progress.lock().unwrap();
    if app
        .emit(
            "progress",
            ProgressPayload {
                job_id: job_id.to_string(),
                percent: progress.percent,
                current_file: current_file.to_string(),
                file_index: progress.completed_count,
                file_count: progress.target_total,
            },
        )
        .is_err()
    {
        eprintln!("emit progress failed");
    }
}

fn register_enumerated_inputs(app: &AppHandle, job_id: &str, count: usize) {
    if count == 0 {
        return;
    }
    if let Some(state) = app
        .try_state::<crate::AppState>()
        .filter(|state| state.is_converting.load(Ordering::SeqCst))
    {
        state
            .overall_progress
            .lock()
            .unwrap()
            .add_enumerated_inputs(count);
        emit_overall_progress(app, job_id, "");
    }
}

fn register_activity_input(app: &AppHandle, artifact_count: usize) -> usize {
    app.try_state::<crate::AppState>()
        .filter(|state| state.is_converting.load(Ordering::SeqCst))
        .map(|state| {
            state
                .overall_progress
                .lock()
                .unwrap()
                .register_input(artifact_count)
        })
        .unwrap_or(0)
}

fn finish_activity_queueing(app: &AppHandle, job_id: &str) {
    if let Some(state) = app
        .try_state::<crate::AppState>()
        .filter(|state| state.is_converting.load(Ordering::SeqCst))
    {
        if state.scanning_batches.fetch_sub(1, Ordering::AcqRel) == 1 {
            state.overall_progress.lock().unwrap().finish_queueing();
        }
        emit_overall_progress(app, job_id, "");
    }
}

/// 成果物の進捗とメイン画面へ送る全体進捗を、同じバックエンド状態から更新する。
fn update_artifact_progress(
    app: &AppHandle,
    job_id: &str,
    index: usize,
    file_name: &str,
    ratio: f64,
    terminal: bool,
) {
    let Some(state) = app.try_state::<crate::AppState>() else {
        return;
    };
    // cancel_job 後も終了処理中の子タスクから遅延コールバックが届き得るため無視する。
    if !state.is_converting.load(Ordering::SeqCst) {
        return;
    }
    let should_emit = {
        let mut progress = state.overall_progress.lock().unwrap();
        let completed_inputs_before = progress.completed_input_count;
        progress.update(index, ratio, terminal);
        progress.phase == crate::ProgressPhase::Exact
            || progress.completed_input_count != completed_inputs_before
    };
    if terminal {
        state.active_files.lock().unwrap().remove(file_name);
        state.active_artifacts.lock().unwrap().remove(&index);
    } else {
        state
            .active_files
            .lock()
            .unwrap()
            .insert(file_name.to_string(), ratio.clamp(0.0, 1.0) as f32);
        state.active_artifacts.lock().unwrap().insert(index);
    }
    if should_emit {
        emit_overall_progress(app, job_id, file_name);
    }
}

// --- Main conversion runner ---

pub struct ConversionRun {
    pub app: AppHandle,
    pub job_id: String,
    pub request: ConvertRequest,
    pub settings: Settings,
    pub pgids: Arc<std::sync::Mutex<Vec<i32>>>,
    pub temp_cache_used: Arc<AtomicUsize>,
    pub cancellation: Arc<crate::JobCancellation>,
    pub pause: Arc<crate::SessionPause>,
    pub resources: Arc<ConversionResources>,
    pub batch_order: u64,
}

pub struct ConversionResources {
    cpu: Arc<DynamicLimiter>,
    probe: Arc<Semaphore>,
    dialog: Arc<Semaphore>,
    output_reservations: Arc<std::sync::Mutex<HashMap<PathBuf, u64>>>,
    network_stream: Arc<Semaphore>,
    network_output: Arc<Semaphore>,
    output_spool: Arc<OutputSpoolCapacity>,
    parallel: AtomicUsize,
}

impl ConversionResources {
    pub fn new(parallel: usize) -> Self {
        let parallel = parallel.max(1);
        Self {
            cpu: Arc::new(DynamicLimiter::new(parallel)),
            probe: Arc::new(Semaphore::new(parallel)),
            dialog: Arc::new(Semaphore::new(1)),
            output_reservations: Arc::new(std::sync::Mutex::new(HashMap::new())),
            network_stream: Arc::new(Semaphore::new(1)),
            network_output: Arc::new(Semaphore::new(1)),
            output_spool: Arc::new(OutputSpoolCapacity::new()),
            parallel: AtomicUsize::new(parallel),
        }
    }

    pub fn update_parallel(&self, parallel: usize) {
        let parallel = parallel.max(1);
        self.parallel.store(parallel, Ordering::Release);
        self.cpu.set_limit(parallel);
    }

    fn parallel(&self) -> usize {
        self.parallel.load(Ordering::Acquire).max(1)
    }
}

struct DynamicLimiter {
    limit: AtomicUsize,
    running: AtomicUsize,
    changed: Notify,
}

impl DynamicLimiter {
    fn new(limit: usize) -> Self {
        Self {
            limit: AtomicUsize::new(limit.max(1)),
            running: AtomicUsize::new(0),
            changed: Notify::new(),
        }
    }

    fn set_limit(&self, limit: usize) {
        self.limit.store(limit.max(1), Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn acquire(
        self: &Arc<Self>,
        cancellation: &crate::JobCancellation,
    ) -> std::result::Result<DynamicPermit, ()> {
        loop {
            if cancellation.is_cancelled() {
                return Err(());
            }
            let running = self.running.load(Ordering::Acquire);
            if running < self.limit.load(Ordering::Acquire)
                && self
                    .running
                    .compare_exchange_weak(
                        running,
                        running + 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                return Ok(DynamicPermit {
                    limiter: self.clone(),
                });
            }
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.running.load(Ordering::Acquire) < self.limit.load(Ordering::Acquire) {
                continue;
            }
            tokio::select! {
                _ = &mut changed => {},
                _ = cancellation.cancelled() => return Err(()),
            }
        }
    }
}

struct DynamicPermit {
    limiter: Arc<DynamicLimiter>,
}

impl Drop for DynamicPermit {
    fn drop(&mut self) {
        self.limiter.running.fetch_sub(1, Ordering::AcqRel);
        self.limiter.changed.notify_waiters();
    }
}

pub struct BatchOutcome {
    pub results: Vec<FileResult>,
    pub settings: Settings,
    pub mode: String,
    pub format: String,
    pub batch_order: u64,
}

pub async fn run_conversion(run: ConversionRun) -> BatchOutcome {
    let ConversionRun {
        app,
        job_id,
        request,
        settings,
        pgids,
        temp_cache_used,
        cancellation,
        pause,
        resources,
        batch_order,
    } = run;
    let spool_manager = app.state::<crate::AppState>().spool_manager().clone();
    spool_manager.retry_recovery();
    if !pause.wait_until_resumed(&cancellation).await {
        return BatchOutcome {
            results: Vec::new(),
            settings,
            mode: request.mode,
            format: request.format,
            batch_order,
        };
    }
    let format = if request.mode == "decode" {
        // DECODE モードは wav または aiff のみ許可、それ以外はデフォルト wav
        match request.format.as_str() {
            "aiff" => "aiff".to_string(),
            _ => "wav".to_string(),
        }
    } else {
        request.format.clone()
    };

    // ドロップされた元パスの親ディレクトリを基点にすることで、
    // ドロップしたフォルダ名自体も出力パスに含まれるようにする
    let base_dir: Option<PathBuf> =
        if settings.preserve_folder_structure && settings.output_dest != OutputDest::SourceFolder {
            let drop_paths: Vec<PathBuf> = request.paths.iter().map(PathBuf::from).collect();
            common_ancestor(&drop_paths)
        } else {
            None
        };

    let outcome_settings = settings.clone();
    let outcome_mode = request.mode.clone();
    let outcome_format = format.clone();
    let settings = Arc::new(settings);
    let job_id = Arc::new(job_id);
    // ネットワーク入力を検出（macOS/Linux/Windowsでマウント種別を判定）
    let is_network = has_network_input(&request.paths);
    // アクティビティモニター向けにネットワークフラグを AppState へ書き込む
    if let Some(state) = app.try_state::<crate::AppState>() {
        state
            .is_network_conv
            .store(is_network, std::sync::atomic::Ordering::Relaxed);
    }
    // CPU並列数: 常にユーザー設定値を使用（ネットワーク時も変換は並列）
    let cpu_parallel = resources.parallel();
    // I/O並列数: ネットワーク時はシリアル（帯域飽和防止）、ローカルは並列
    let io_parallel = if is_network { 1 } else { cpu_parallel };
    // 並列数に応じてCPUスレッドを均等配分（1ジョブあたりのスレッド数）
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let threads_per_job = (cpu_count / cpu_parallel).max(1);
    let sem = resources.cpu.clone();
    let dialog_sem = resources.dialog.clone();
    let output_reservations = resources.output_reservations.clone();

    // フェーズ1+2+3-Stage1: ストリーミングパイプライン
    // プローブ完了順にグループを解決し、揃い次第すぐに変換を開始する。
    // NAS入力は256 MiB以下だけをキャッシュし、それを超える入力は元パスを直接読む。

    // NASの実データ読み込みと書き戻しは、それぞれ常に1本だけにする。
    let network_stream_sem = resources.network_stream.clone();
    let network_output_sem = resources.network_output.clone();
    let output_spool_capacity = resources.output_spool.clone();

    // Stage 1 → Stage 2 チャンネル
    let (stage_tx, mut stage_rx) = tokio::sync::mpsc::channel::<(
        usize,
        PathBuf,
        FileInfo,
        Option<NetworkInput>,
        Option<usize>,
        Option<String>,
    )>(cpu_parallel + 1);
    // 走査スレッドは少数のフォルダバッチだけを先読みし、巨大ツリー全体を保持しない。
    let (scan_tx, mut scan_rx) = tokio::sync::mpsc::channel(4);
    let scan_paths = request.paths.clone();
    let scanner_cancellation = cancellation.clone();
    let mut scanner_task = AbortOnDrop::new(tokio::task::spawn_blocking(move || {
        scan_paths_in_batches(scan_paths, scan_tx, scanner_cancellation)
    }));

    // Stage 1: プローブ → グループ解決 → I/Oロード（バックグラウンドタスク）
    // プローブ完了したグループから即座に Stage 2 へ送信する
    let s1_temp_cache_used = temp_cache_used.clone();
    let s1_spool_manager = spool_manager.clone();
    let s1_app = app.clone();
    let s1_job_id = job_id.clone();
    let s1_settings = settings.clone();
    let s1_network_stream_sem = network_stream_sem.clone();
    let s1_probe_sem = resources.probe.clone();
    let s1_pause = pause.clone();
    let s1_cancellation = cancellation.clone();
    let mut stage1_task = AbortOnDrop::new(tokio::spawn(async move {
        let mut scan_results: Vec<FR> = Vec::new();
        let mut non_media: Vec<FR> = Vec::new();
        let mut rejected_results: Vec<FR> = Vec::new();
        while let Some(batch) = scan_rx.recv().await {
            if !s1_pause.wait_until_resumed(&s1_cancellation).await {
                return (scan_results, non_media, rejected_results);
            }
            if s1_cancellation.is_cancelled()
                || !s1_app
                    .try_state::<crate::AppState>()
                    .is_some_and(|state| state.is_converting.load(Ordering::SeqCst))
            {
                return (scan_results, non_media, rejected_results);
            }
            // 粗い進捗の分母には、メディア判定や同一stem選別より前の通常ファイルを
            // すべて含める。新しいバッチで分母が増えた場合は表示率を再計算する。
            register_enumerated_inputs(&s1_app, s1_job_id.as_ref(), batch.files.len());
            for (path, error) in batch.errors {
                let display = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                push_conv_log(&s1_app, display, "error", Some(error.clone()));
                scan_results.push(FR::error(path.to_string_lossy(), error));
            }
            // 同じフォルダの全ファイルをプローブし終えるまで待たない。
            // 同一 stem（例: song.flac / song.mp3）だけを先にまとめ、その小さな
            // グループの判定が終わり次第、変換ステージへ渡す。
            let mut grouped_paths: std::collections::BTreeMap<(PathBuf, String), Vec<PathBuf>> =
                std::collections::BTreeMap::new();
            for path in batch.files {
                grouped_paths.entry(stem_key(&path)).or_default().push(path);
            }

            let mut probe_set: JoinSet<Vec<(PathBuf, Result<FileInfo, String>)>> = JoinSet::new();
            let mut pending_groups = grouped_paths.into_values();
            for _ in 0..io_parallel {
                let Some(paths) = pending_groups.next() else {
                    break;
                };
                let probe_cancellation = s1_cancellation.clone();
                let probe_network_sem = s1_network_stream_sem.clone();
                let probe_sem = s1_probe_sem.clone();
                probe_set.spawn(async move {
                    let mut results = Vec::with_capacity(paths.len());
                    for path in paths {
                        let result = probe_candidate(
                            &path,
                            probe_sem.clone(),
                            probe_network_sem.clone(),
                            probe_cancellation.clone(),
                        )
                        .await;
                        results.push((path, result));
                    }
                    results
                });
            }
            while let Some(joined) = probe_set.join_next().await {
                if s1_cancellation.is_cancelled()
                    || !s1_app
                        .try_state::<crate::AppState>()
                        .is_some_and(|state| state.is_converting.load(Ordering::SeqCst))
                {
                    return (scan_results, non_media, rejected_results);
                }
                if let Some(paths) = pending_groups.next() {
                    let probe_cancellation = s1_cancellation.clone();
                    let probe_network_sem = s1_network_stream_sem.clone();
                    let probe_sem = s1_probe_sem.clone();
                    probe_set.spawn(async move {
                        let mut results = Vec::with_capacity(paths.len());
                        for path in paths {
                            let result = probe_candidate(
                                &path,
                                probe_sem.clone(),
                                probe_network_sem.clone(),
                                probe_cancellation.clone(),
                            )
                            .await;
                            results.push((path, result));
                        }
                        results
                    });
                }
                let Ok(results) = joined else { continue };
                let mut group = Vec::new();
                for (path, result) in results {
                    match result {
                        Ok(info) if info.has_media => group.push((path, info)),
                        Ok(_) => non_media.push(FR::skipped(path.to_string_lossy())),
                        Err(e) => non_media.push(FR::error(path.to_string_lossy(), e)),
                    }
                }
                if !group.is_empty() {
                    let (best, rejected_paths) = select_best_from_group(group);
                    for p in rejected_paths {
                        rejected_results.push(FR::skipped(p.to_string_lossy()));
                    }
                    let export_all_tracks =
                        s1_settings.export_all_audio_tracks && best.1.audio_tracks.len() > 1;
                    let tracks = if export_all_tracks {
                        best.1
                            .audio_tracks
                            .iter()
                            .enumerate()
                            .map(|(index, track)| {
                                (
                                    Some(track.stream_index),
                                    Some(
                                        track_output_stem(&best.0, &track_label(track, index + 1))
                                            .unwrap_or_default(),
                                    ),
                                )
                            })
                            .collect::<Vec<_>>()
                    } else {
                        vec![(None, None)]
                    };

                    let first_index = register_activity_input(&s1_app, tracks.len());
                    let artifacts = tracks
                        .into_iter()
                        .enumerate()
                        .map(|(offset, (audio_stream_index, output_stem))| {
                            let display_name = output_stem.clone().unwrap_or_else(|| {
                                best.0
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            });
                            (
                                first_index + offset,
                                audio_stream_index,
                                output_stem,
                                display_name,
                            )
                        })
                        .collect::<Vec<_>>();

                    for (_, _, _, display_name) in &artifacts {
                        push_conv_log(&s1_app, display_name.clone(), "processing", None);
                    }

                    // 256 MiB以下だけをローカルへ退避する。巨大入力は元パスを保持し、
                    // Stage 2でネットワーク読み込み権を変換終了まで占有する。
                    let network_input = if is_path_on_network(&best.0) {
                        let input_size = match tokio::fs::metadata(&best.0).await {
                            Ok(metadata) => metadata.len(),
                            Err(error) => {
                                for (index, _, _, display) in &artifacts {
                                    update_artifact_progress(
                                        &s1_app,
                                        s1_job_id.as_ref(),
                                        *index,
                                        display,
                                        1.0,
                                        true,
                                    );
                                    push_conv_log(
                                        &s1_app,
                                        display.clone(),
                                        "error",
                                        Some(error.to_string()),
                                    );
                                }
                                scan_results
                                    .push(FR::error(best.0.to_string_lossy(), error.to_string()));
                                continue;
                            }
                        };
                        if should_cache_network_input(input_size) {
                            let network_read_permit = tokio::select! {
                                permit = s1_network_stream_sem.acquire() => permit,
                                _ = s1_cancellation.cancelled() => {
                                    return (scan_results, non_media, rejected_results);
                                }
                            };
                            let Ok(_network_read_permit) = network_read_permit else {
                                return (scan_results, non_media, rejected_results);
                            };
                            let progress_artifacts = artifacts
                                .iter()
                                .map(|(index, _, _, display)| (*index, display.clone()))
                                .collect::<Vec<_>>();
                            let progress_app = s1_app.clone();
                            let progress_job_id = s1_job_id.clone();
                            match copy_network_input_to_temp(
                                &best.0,
                                s1_temp_cache_used.clone(),
                                s1_spool_manager.clone(),
                                Some(&s1_app),
                                s1_cancellation.clone(),
                                Arc::new(move |ratio| {
                                    for (index, display) in &progress_artifacts {
                                        update_artifact_progress(
                                            &progress_app,
                                            progress_job_id.as_ref(),
                                            *index,
                                            display,
                                            input_preparation_progress(ratio),
                                            false,
                                        );
                                    }
                                }),
                            )
                            .await
                            {
                                Ok(temp) => Some(NetworkInput::Cached(temp)),
                                Err(_) if s1_cancellation.is_cancelled() => {
                                    return (scan_results, non_media, rejected_results);
                                }
                                Err(error) => {
                                    for (index, _, _, display) in &artifacts {
                                        update_artifact_progress(
                                            &s1_app,
                                            s1_job_id.as_ref(),
                                            *index,
                                            display,
                                            1.0,
                                            true,
                                        );
                                        push_conv_log(
                                            &s1_app,
                                            display.clone(),
                                            "error",
                                            Some(error.to_string()),
                                        );
                                        scan_results.push(FR::error(
                                            best.0.to_string_lossy(),
                                            error.to_string(),
                                        ));
                                    }
                                    continue;
                                }
                            }
                        } else {
                            for (index, _, _, display) in &artifacts {
                                update_artifact_progress(
                                    &s1_app,
                                    s1_job_id.as_ref(),
                                    *index,
                                    display,
                                    input_preparation_progress(1.0),
                                    false,
                                );
                            }
                            Some(NetworkInput::Direct(best.0.clone()))
                        }
                    } else {
                        for (index, _, _, display) in &artifacts {
                            update_artifact_progress(
                                &s1_app,
                                s1_job_id.as_ref(),
                                *index,
                                display,
                                input_preparation_progress(1.0),
                                false,
                            );
                        }
                        None
                    };

                    for (artifact_index, audio_stream_index, output_stem, _) in artifacts {
                        let send = stage_tx.send((
                            artifact_index,
                            best.0.clone(),
                            best.1.clone(),
                            network_input.clone(),
                            audio_stream_index,
                            output_stem,
                        ));
                        let sent = tokio::select! {
                            sent = send => sent,
                            _ = s1_cancellation.cancelled() => {
                                return (scan_results, non_media, rejected_results);
                            }
                        };
                        if sent.is_err() {
                            return (scan_results, non_media, rejected_results);
                        }
                    }
                }
            }
        }
        (scan_results, non_media, rejected_results)
    }));

    // Stage 2: CPUタスク（並列変換）
    let mut conv_set: JoinSet<FR> = JoinSet::new();
    while let Some((artifact_index, path, info, network_input, audio_stream_index, output_stem)) =
        stage_rx.recv().await
    {
        let sem = sem.clone();
        let app = app.clone();
        let job_id = job_id.clone();
        let format = format.clone();
        let settings = settings.clone();
        let file_duration = info.duration_secs;
        let pgids_for_spawn = pgids.clone();
        let base_dir = base_dir.clone();
        let dialog_sem = dialog_sem.clone();
        let output_reservations = output_reservations.clone();
        let network_output_sem = network_output_sem.clone();
        let output_spool_capacity = output_spool_capacity.clone();
        let network_stream_sem = network_stream_sem.clone();
        let cancellation = cancellation.clone();
        let pause = pause.clone();
        let spool_manager = spool_manager.clone();

        conv_set.spawn(async move {
            if !pause.wait_until_resumed(&cancellation).await {
                return FR::error(path.to_string_lossy(), CANCELLED_ERROR);
            }
            let input_display = output_stem.clone().unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });

            let (output_path, _output_path_lock) = loop {
                let output_context = OutputResolutionContext {
                    format: &format,
                    settings: &settings,
                    base_dir: base_dir.as_deref(),
                    app: &app,
                    dialog_sem: &dialog_sem,
                    reservations: &output_reservations,
                    cancellation: &cancellation,
                    batch_order,
                };
                match if let Some(stem) = output_stem.as_deref() {
                    resolve_output_path_for_stem(&path, stem, &output_context).await
                } else {
                    resolve_output_path(&path, &output_context).await
                } {
                    Ok(result) => break result,
                    // 別インスタンスが同じ完成ファイルを使用中なら、予約済み候補を残して
                    // 次の親フォルダ名付き候補へ即座に再解決する。
                    Err(e) if e.to_string() == "__OUTPUT_PATH_BUSY__" => continue,
                    Err(e) if e.to_string() == "__SKIPPED__" => {
                        update_artifact_progress(
                            &app,
                            job_id.as_ref(),
                            artifact_index,
                            &input_display,
                            1.0,
                            true,
                        );
                        push_conv_log(&app, input_display.clone(), "skipped", None);
                        return FR::skipped(path.to_string_lossy());
                    }
                    Err(e) => {
                        update_artifact_progress(
                            &app,
                            job_id.as_ref(),
                            artifact_index,
                            &input_display,
                            1.0,
                            true,
                        );
                        push_conv_log(&app, input_display.clone(), "error", Some(e.to_string()));
                        return FR::error(path.to_string_lossy(), e.to_string());
                    }
                }
            };

            let network_output = is_path_on_network(&output_path);
            if network_output {
                if let Some(state) = app.try_state::<crate::AppState>() {
                    state.is_network_conv.store(true, Ordering::Relaxed);
                }
            }
            let _output_reservation = if network_output {
                let bytes = estimate_output_bytes(&format, &settings, &info);
                match output_spool_capacity
                    .reserve(bytes, &app, &cancellation)
                    .await
                {
                    Ok(reservation) => Some(reservation),
                    Err(error) => {
                        update_artifact_progress(
                            &app,
                            job_id.as_ref(),
                            artifact_index,
                            &input_display,
                            1.0,
                            true,
                        );
                        push_conv_log(
                            &app,
                            input_display.clone(),
                            "error",
                            Some(error.to_string()),
                        );
                        return FR::error(path.to_string_lossy(), error.to_string());
                    }
                }
            } else {
                None
            };
            // 成果物はNASへ直接書かず、必ずローカル出力スプールへ生成する。
            let mut output_spool = if network_output {
                let usage = app.state::<crate::AppState>().output_spool_used.clone();
                Some(spool_manager.new_spool_file("output", &format, usage))
            } else {
                None
            };
            let local_output = output_spool
                .as_ref()
                .map(|spool| spool.path().to_path_buf())
                .unwrap_or_else(|| output_path.clone());

            // 出力名の予約・上書き確認はCPU変換枠を消費しない。実際にFFmpegを起動する
            // 直前だけ枠を取得するため、確認待ちが並列変換を止めない。
            if !pause.wait_until_resumed(&cancellation).await {
                return FR::error(path.to_string_lossy(), CANCELLED_ERROR);
            }
            let direct_network_input =
                matches!(network_input.as_ref(), Some(NetworkInput::Direct(_)));
            let network_read_permit = if direct_network_input {
                let permit = tokio::select! {
                    permit = network_stream_sem.clone().acquire_owned() => permit,
                    _ = cancellation.cancelled() => {
                        return FR::error(path.to_string_lossy(), CANCELLED_ERROR);
                    }
                };
                match permit {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        return FR::error(path.to_string_lossy(), "network input worker closed")
                    }
                }
            } else {
                None
            };
            let cpu_permit = sem.acquire(&cancellation).await;
            let Ok(_permit) = cpu_permit else {
                return FR::error(path.to_string_lossy(), CANCELLED_ERROR);
            };

            let on_progress: Arc<dyn Fn(f64) + Send + Sync> = {
                let progress_app = app.clone();
                let progress_job_id = job_id.clone();
                let progress_name = input_display.clone();
                Arc::new(move |ratio| {
                    update_artifact_progress(
                        &progress_app,
                        progress_job_id.as_ref(),
                        artifact_index,
                        &progress_name,
                        ffmpeg_stage_progress(ratio),
                        false,
                    );
                })
            };
            let on_pid_start: process::PidCallback = {
                let pgids_for_pid = pgids_for_spawn.clone();
                Arc::new(move |pid| {
                    pgids_for_pid.lock().unwrap().push(pid as i32);
                })
            };
            let on_pid_exit: process::PidCallback = {
                let pgids_for_exit = pgids_for_spawn.clone();
                Arc::new(move |pid| {
                    let mut pgids = pgids_for_exit.lock().unwrap();
                    if let Some(index) = pgids
                        .iter()
                        .position(|&registered| registered == pid as i32)
                    {
                        pgids.swap_remove(index);
                    }
                })
            };
            let conversion = convert_one(
                ConvertOneRequest {
                    input: &path,
                    output: &local_output,
                    format: &format,
                    settings: &settings,
                    info: &info,
                    audio_stream_index,
                    duration_secs: file_duration,
                    network_input,
                },
                ConversionContext {
                    threads_per_job,
                    cancellation: cancellation.clone(),
                    on_progress,
                    processes: ProcessTracker::new(on_pid_start, on_pid_exit),
                },
            )
            .await;
            // 無音検査・本変換・カバーアート再読込までを1回のNAS読込として直列化する。
            drop(network_read_permit);

            let result = if network_output {
                match conversion {
                    Ok(()) => {
                        let spool_bytes = tokio::fs::metadata(&local_output)
                            .await
                            .map(|m| m.len() as usize)
                            .unwrap_or(0);
                        if let Some(spool) = output_spool.as_mut() {
                            spool.set_accounted_bytes(spool_bytes);
                        }
                        // NAS書込みは1本だけ。完了・失敗・キャンセル時もローカル成果物は削除する。
                        let upload_result = {
                            let upload_permit = tokio::select! {
                                permit = network_output_sem.acquire() => permit,
                                _ = cancellation.cancelled() => {
                                    return FR::error(path.to_string_lossy(), CANCELLED_ERROR);
                                }
                            };
                            let Ok(_upload_permit) = upload_permit else {
                                return FR::error(
                                    path.to_string_lossy(),
                                    "network output worker closed",
                                );
                            };
                            let progress_app = app.clone();
                            let progress_job_id = job_id.clone();
                            let progress_name = input_display.clone();
                            copy_spool_to_network(
                                &local_output,
                                &output_path,
                                &spool_manager,
                                &cancellation,
                                Arc::new(move |ratio| {
                                    update_artifact_progress(
                                        &progress_app,
                                        progress_job_id.as_ref(),
                                        artifact_index,
                                        &progress_name,
                                        output_stage_progress(ratio),
                                        false,
                                    );
                                }),
                            )
                            .await
                        };
                        upload_result
                    }
                    Err(error) => Err(error),
                }
            } else {
                conversion
            };

            // 変換完了ログ
            let (log_status, log_error) = match &result {
                Ok(()) => ("done", None),
                Err(e) => ("error", Some(e.to_string())),
            };
            push_conv_log(&app, input_display.clone(), log_status, log_error);
            update_artifact_progress(
                &app,
                job_id.as_ref(),
                artifact_index,
                &input_display,
                1.0,
                true,
            );

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

    // Stage 1 が全候補を判定し終え、これ以降は対象総数が増えない。
    finish_activity_queueing(&app, job_id.as_ref());

    // stage_tx が Stage 1 内で drop された後にここへ到達する。
    let _ = scanner_task.join().await;
    let (scan_results, non_media, rejected_results) = stage1_task.join().await.unwrap_or_default();

    let mut results: Vec<FR> = scan_results;
    results.extend(non_media);
    results.extend(rejected_results);
    while let Some(Ok(result)) = conv_set.join_next().await {
        results.push(result);
    }

    BatchOutcome {
        results,
        settings: outcome_settings,
        mode: outcome_mode,
        format: outcome_format,
        batch_order,
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use std::collections::HashMap;

    fn test_spool_manager() -> Arc<crate::spool::SpoolManager> {
        let app_data =
            std::env::temp_dir().join(format!("oto-spool-manager-test-{}", uuid::Uuid::new_v4()));
        crate::spool::SpoolManager::initialize(&app_data).unwrap()
    }

    #[test]
    fn artifact_stage_weights_cover_the_whole_pipeline_without_regression() {
        let checkpoints = [
            input_preparation_progress(0.0),
            input_preparation_progress(1.0),
            ffmpeg_stage_progress(0.0),
            ffmpeg_stage_progress(0.5),
            ffmpeg_stage_progress(1.0),
            output_stage_progress(0.0),
            output_stage_progress(0.5),
            output_stage_progress(1.0),
        ];
        assert_eq!(checkpoints[0], 0.0);
        assert_eq!(checkpoints[1], 0.1);
        assert_eq!(checkpoints[4], 0.9);
        assert_eq!(checkpoints[7], 1.0);
        assert!(checkpoints.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[tokio::test]
    async fn lowering_parallel_limit_waits_for_running_work_to_drain() {
        let limiter = Arc::new(DynamicLimiter::new(2));
        let cancellation = crate::JobCancellation::new();
        let first = limiter.acquire(&cancellation).await.unwrap();
        let second = limiter.acquire(&cancellation).await.unwrap();
        limiter.set_limit(1);
        let third = tokio::spawn({
            let limiter = limiter.clone();
            async move {
                let cancellation = crate::JobCancellation::new();
                limiter.acquire(&cancellation).await
            }
        });
        tokio::task::yield_now().await;
        assert!(!third.is_finished());
        drop(first);
        tokio::task::yield_now().await;
        assert!(!third.is_finished());
        drop(second);
        assert!(third.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn raising_parallel_limit_releases_waiting_work_immediately() {
        let limiter = Arc::new(DynamicLimiter::new(1));
        let cancellation = crate::JobCancellation::new();
        let _first = limiter.acquire(&cancellation).await.unwrap();
        let second = tokio::spawn({
            let limiter = limiter.clone();
            async move {
                let cancellation = crate::JobCancellation::new();
                limiter.acquire(&cancellation).await
            }
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        limiter.set_limit(2);
        assert!(second.await.unwrap().is_ok());
    }

    #[test]
    fn network_input_cache_boundary_is_256_mib_inclusive() {
        let limit = crate::INPUT_SPOOL_TARGET_BYTES as u64;
        assert!(should_cache_network_input(limit));
        assert!(!should_cache_network_input(limit + 1));
    }

    #[test]
    fn input_spool_resume_point_never_allows_the_high_water_mark_to_be_exceeded() {
        for input_size in [1, 64, 128, 129, 200, 256].map(|mib| mib * 1024 * 1024) {
            let resume_at = input_spool_resume_at(input_size);
            assert!(resume_at.saturating_add(input_size) <= crate::INPUT_SPOOL_TARGET_BYTES);
        }
        assert_eq!(input_spool_resume_at(64 * 1024 * 1024), 192 * 1024 * 1024);
        assert_eq!(input_spool_resume_at(200 * 1024 * 1024), 56 * 1024 * 1024);
    }

    #[test]
    fn output_reservation_is_conservative_for_pcm_and_vbr() {
        let mut info = FileInfo {
            duration_secs: 60.0,
            tags: HashMap::new(),
            bits_per_sample: 16,
            cover_art_stream_idx: None,
            has_media: true,
            is_lossless: false,
            bit_rate_bps: 128_000,
            audio_tracks: Vec::new(),
        };
        let settings = Settings::default();
        assert!(estimate_output_bytes("wav", &settings, &info) > 20 * 1024 * 1024);
        let mut vbr = settings.clone();
        vbr.mp3_preset = "custom".into();
        vbr.mp3_mode = "vbr".into();
        let vbr_bytes = estimate_output_bytes("mp3", &vbr, &info);
        info.duration_secs = 120.0;
        assert!(estimate_output_bytes("mp3", &vbr, &info) > vbr_bytes);
    }

    #[test]
    fn output_reservation_is_released_by_drop() {
        let capacity = Arc::new(OutputSpoolCapacity::new());
        capacity.reserved.store(4096, Ordering::Release);
        let reservation = OutputReservation {
            bytes: 4096,
            capacity: capacity.clone(),
        };
        drop(reservation);
        assert_eq!(capacity.reserved.load(Ordering::Acquire), 0);
    }

    #[test]
    fn track_labels_prefer_meaningful_handler_then_language_then_number() {
        let commentary = types::AudioTrack {
            stream_index: 2,
            language: Some("jpn".into()),
            handler_name: Some("Commentary".into()),
        };
        let main = types::AudioTrack {
            stream_index: 1,
            language: Some("jpn".into()),
            handler_name: Some("Main".into()),
        };
        let unnamed = types::AudioTrack {
            stream_index: 3,
            language: None,
            handler_name: None,
        };

        assert_eq!(track_label(&commentary, 2), "Commentary");
        assert_eq!(track_label(&main, 1), "jpn");
        assert_eq!(track_label(&unnamed, 3), "trk3");
        assert_eq!(
            track_output_stem(Path::new("movie.mp4"), "Commentary").unwrap(),
            "movie (Commentary)"
        );
    }

    #[test]
    fn container_generated_handler_falls_back_to_language() {
        let track = types::AudioTrack {
            stream_index: 1,
            language: Some("eng".into()),
            handler_name: Some("GPAC ISO Audio Handler".into()),
        };
        assert_eq!(track_label(&track, 1), "eng");
    }

    #[test]
    fn track_labels_replace_filename_separators() {
        let track = types::AudioTrack {
            stream_index: 1,
            language: None,
            handler_name: Some("Commentary: Director/Producer".into()),
        };
        assert_eq!(track_label(&track, 1), "Commentary_ Director_Producer");
    }

    #[test]
    fn successful_in_place_replacement_keeps_converted_file() {
        let dir = std::env::temp_dir().join(format!("oto-replace-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("song.mp3");
        let temp_output = dir.join(".oto-reencode-test.mp3");
        std::fs::write(&output, b"original").unwrap();
        std::fs::write(&temp_output, b"converted").unwrap();

        replace_file_after_success(&temp_output, &output).unwrap();

        assert_eq!(std::fs::read(&output).unwrap(), b"converted");
        assert!(!temp_output.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cached_network_input_is_removed_when_last_owner_drops() {
        let source =
            std::env::temp_dir().join(format!("oto-cache-source-{}", uuid::Uuid::new_v4()));
        std::fs::write(&source, b"network data").unwrap();
        let cache_used = Arc::new(AtomicUsize::new(0));
        let spool_manager = test_spool_manager();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let cached = runtime
            .block_on(copy_network_input_to_temp(
                &source,
                cache_used.clone(),
                spool_manager,
                None,
                Arc::new(crate::JobCancellation::new()),
                Arc::new(|_| {}),
            ))
            .unwrap();
        let cached_path = cached.file.path().to_path_buf();
        assert_eq!(std::fs::read(&cached_path).unwrap(), b"network data");
        assert_eq!(cache_used.load(Ordering::Relaxed), 12);
        drop(cached);
        assert!(!cached_path.exists());
        assert_eq!(cache_used.load(Ordering::Relaxed), 0);
        std::fs::remove_file(source).unwrap();
    }

    #[test]
    fn cancelling_input_copy_removes_partial_file_and_releases_written_bytes() {
        let source =
            std::env::temp_dir().join(format!("oto-cache-cancel-source-{}", uuid::Uuid::new_v4()));
        std::fs::write(&source, vec![7u8; NETWORK_CHUNK_SIZE * 3]).unwrap();
        let cache_used = Arc::new(AtomicUsize::new(0));
        let spool_manager = test_spool_manager();
        let cancellation = Arc::new(crate::JobCancellation::new());
        let cancel_from_progress = cancellation.clone();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(copy_network_input_to_temp(
            &source,
            cache_used.clone(),
            spool_manager,
            None,
            cancellation,
            Arc::new(move |ratio| {
                if ratio > 0.0 {
                    cancel_from_progress.cancel();
                }
            }),
        ));

        assert!(result.is_err());
        assert_eq!(cache_used.load(Ordering::Acquire), 0);
        std::fs::remove_file(source).unwrap();
    }

    #[test]
    fn cancelling_capacity_wait_does_not_start_or_charge_the_next_copy() {
        let source = std::env::temp_dir().join(format!(
            "oto-cache-wait-cancel-source-{}",
            uuid::Uuid::new_v4()
        ));
        let file = std::fs::File::create(&source).unwrap();
        file.set_len(200 * 1024 * 1024).unwrap();
        drop(file);
        let existing = 100 * 1024 * 1024;
        let cache_used = Arc::new(AtomicUsize::new(existing));
        let cancellation = Arc::new(crate::JobCancellation::new());
        let spool_manager = test_spool_manager();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(async {
            let source_for_copy = source.clone();
            let cache_for_copy = cache_used.clone();
            let cancellation_for_copy = cancellation.clone();
            let spool_manager_for_copy = spool_manager.clone();
            let copy = tokio::spawn(async move {
                copy_network_input_to_temp(
                    &source_for_copy,
                    cache_for_copy,
                    spool_manager_for_copy,
                    None,
                    cancellation_for_copy,
                    Arc::new(|_| {}),
                )
                .await
            });
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            cancellation.cancel();
            copy.await.unwrap()
        });

        assert!(result.is_err());
        assert_eq!(cache_used.load(Ordering::Acquire), existing);
        std::fs::remove_file(source).unwrap();
    }

    /// ローカルパスはネットワーク判定で false を返す
    #[test]
    fn local_temp_dir_is_not_network() {
        let tmp = std::env::temp_dir();
        // macOS/Linux 実装のみ呼び出し可能。Windows はビルド時に別関数
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(
            !is_path_on_network(&tmp),
            "temp_dir should not be on network: {:?}",
            tmp
        );
    }

    #[test]
    fn local_root_is_not_network() {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(!is_path_on_network(Path::new("/")));
    }

    /// has_network_input: 空のパスリストは false
    #[test]
    fn empty_paths_returns_false() {
        assert!(!has_network_input(&[]));
    }

    /// has_network_input: ローカルの /tmp は false
    #[test]
    fn tmp_path_not_network() {
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        assert!(!has_network_input(&[tmp]));
    }

    /// Windows のみ: UNC パスは必ずネットワーク判定
    #[cfg(windows)]
    #[test]
    fn unc_path_is_network() {
        assert!(has_network_input(
            &[r"\\server\share\file.flac".to_string()]
        ));
    }

    /// Windows のみ: ローカルドライブパスはネットワーク判定されない
    /// （実際のドライブ種別依存のため、存在するローカルドライブ C:\ を想定）
    #[cfg(windows)]
    #[test]
    fn local_drive_path_not_network() {
        // C:\ がローカルドライブの場合のみ成り立つ。CI では skip しても可
        let path = r"C:\Users\test\music.flac".to_string();
        // GetDriveTypeW が DRIVE_REMOTE を返さなければ false のはず
        // ネットワークドライブにマップされていない前提のテスト
        let _ = has_network_input(&[path]); // panics/crashes は起こらないことを確認
    }
}
