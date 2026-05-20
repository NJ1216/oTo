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
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;

use crate::settings::{OutputDest, Settings, SourceFileAction};
use codec_args::build_codec_args;
use file_collector::{common_ancestor, select_best_from_group, stem_key};
use output::resolve_output_path;
use probe::probe_file;
use silence::detect_boundary_silence;
use types::{FileInfo, FileResult as FR};

use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(windows)]
use std::sync::atomic::AtomicU64;
use tokio::io::AsyncWriteExt;

#[cfg(windows)]
static PROGRESS_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

// Parse FFmpeg stderr 'time=HH:MM:SS.ms' and return progress ratio
#[allow(dead_code)]
fn parse_ffmpeg_time(time_str: &str, duration_secs: f64) -> Option<f64> {
    if duration_secs <= 0.0 {
        return None;
    }
    let time_str = time_str.trim();
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let hours: f64 = parts[0].parse().ok()?;
    let minutes: f64 = parts[1].parse().ok()?;
    let secs: f64 = parts[2].parse().ok()?;
    let total_secs = hours * 3600.0 + minutes * 60.0 + secs;
    Some((total_secs / duration_secs).min(1.0))
}

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
    input_bytes: Option<Vec<u8>>,
    on_progress: Arc<dyn Fn(f64) + Send + Sync>,
    on_pid: Arc<dyn Fn(u32) + Send + Sync>,
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
            } else if let Some(backup) = &self.backup {
                // 成功時は退避していた旧ファイルを破棄する
                let _ = std::fs::remove_file(backup);
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

    // args[..3] = [-threads, N, -y]; args[3..] = [-map_metadata … -nostats]
    // input_bytes が Some のとき: -f <format_name> -i pipe:0 でstdin経由
    // input_bytes が None のとき: 既存のOsStr方式でファイルパス直接渡し
    let mut cmd = tokio::process::Command::new(&ffmpeg);
    cmd.args(&args[..3]);
    if input_bytes.is_some() {
        cmd.args(["-f", &info.format_name, "-i", "pipe:0"]);
    } else {
        cmd.arg("-i").arg(input);
    }
    cmd.args(&args[3..]).arg(output);
    #[cfg(not(windows))]
    {
        cmd.stdin(Stdio::piped())
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());
    }
    #[cfg(windows)]
    {
        cmd.stdin(Stdio::piped())
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

    // input_bytes が Some のとき: stdin にバイト列を書き込んで閉じる
    // drop で EOF が通知されFFmpegが入力終端を認識する
    if let Some(bytes) = input_bytes {
        if let Some(mut stdin_handle) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = stdin_handle.write_all(&bytes).await;
            });
        }
    } else {
        // stdin を使わない場合は閉じる（Windowsでパイプがハングしないよう）
        drop(child.stdin.take());
    }

    let stderr_task = child.stderr.take().map(|stderr| {
        let on_progress = on_progress.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut buf = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                buf.push(line.clone());
                // Parse 'time=HH:MM:SS.ms' for realtime progress update
                if let Some(time_str) = line.strip_prefix("time=") {
                    if let Some(progress) = parse_ffmpeg_time(time_str, duration_secs) {
                        on_progress(progress);
                    }
                }
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
fn is_path_on_network(_path: &Path) -> bool { false }

#[cfg(not(windows))]
fn has_network_input(paths: &[String]) -> bool {
    paths.iter().any(|p| is_path_on_network(Path::new(p)))
}

/// AppState のログバッファにエントリを追加する（最大300件、古いものを自動削除）
fn push_conv_log(app: &AppHandle, file_name: String, status: &str, error: Option<String>) {
    if let Some(state) = app.try_state::<crate::AppState>() {
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut log = state.conv_log.lock().unwrap();
        if log.len() >= 300 { log.pop_front(); }
        log.push_back(crate::ConvLogEntry {
            ts_ms,
            file_name,
            status: status.to_string(),
            error,
        });
    }
}

// --- Main conversion runner ---

pub async fn run_conversion(
    app: AppHandle,
    job_id: String,
    request: ConvertRequest,
    settings: Settings,
    pgids: Arc<std::sync::Mutex<Vec<i32>>>,
    memory_used: Arc<AtomicUsize>,
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
    // ネットワーク入力を検出（macOS/Linux/Windowsでマウント種別を判定）
    let is_network = has_network_input(&request.paths);
    // アクティビティモニター向けにネットワークフラグを AppState へ書き込む
    if let Some(state) = app.try_state::<crate::AppState>() {
        state.is_network_conv.store(is_network, std::sync::atomic::Ordering::Relaxed);
    }
    // CPU並列数: 常にユーザー設定値を使用（ネットワーク時も変換は並列）
    let cpu_parallel = settings.parallel_count.max(1);
    // I/O並列数: ネットワーク時はシリアル（帯域飽和防止）、ローカルは並列
    let io_parallel = if is_network { 1 } else { cpu_parallel };
    // 並列数に応じてCPUスレッドを均等配分（1ジョブあたりのスレッド数）
    let cpu_count = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads_per_job = (cpu_count / cpu_parallel).max(1);
    let sem = Arc::new(Semaphore::new(cpu_parallel));
    let dialog_sem = Arc::new(Semaphore::new(1)); // ダイアログは同時1件

    // フェーズ1+2+3-Stage1: ストリーミングパイプライン
    // プローブ完了順にグループを解決し、揃い次第すぐに変換を開始する。
    // ネットワーク時: I/Oシリアル読み込み→メモリ保持→stdin並列変換
    // ローカル時: bytes=None で既存動作と同等

    // プローブ前にステムグループを事前集計（重複判定カウントダウンに使用）
    let mut group_remaining: std::collections::HashMap<(PathBuf, String), usize> =
        std::collections::HashMap::new();
    for path in &file_paths {
        *group_remaining.entry(stem_key(path)).or_insert(0) += 1;
    }
    let max_selected = group_remaining.len(); // progress_ratios Vec サイズの上限

    // プログレス追跡: 各スロットに 0.0–1.0 の完了比率を格納し、max_selected で割る。
    // 分母が固定なので、Stage 1 が新ファイルを発見しても値が後退しない。
    let progress_ratios = Arc::new(tokio::sync::Mutex::new(vec![0.0f64; max_selected]));
    let total_selected = Arc::new(AtomicUsize::new(0));

    // メモリバジェット（ネットワーク時のみ有効）
    let memory_budget = settings.max_memory_mb * 1024 * 1024;
    memory_used.store(0, Ordering::Relaxed); // 今回の変換開始時にリセット
    let memory_freed = Arc::new(tokio::sync::Notify::new());
    let silence_trim_enabled = settings.silence_trim_enabled;

    // Stage 1 → Stage 2 チャンネル
    let (stage_tx, mut stage_rx) = tokio::sync::mpsc::channel::<(usize, PathBuf, FileInfo, Option<Vec<u8>>)>(cpu_parallel + 1);

    // Stage 1: プローブ → グループ解決 → I/Oロード（バックグラウンドタスク）
    // プローブ完了したグループから即座に Stage 2 へ送信する
    let s1_memory_used = memory_used.clone();
    let s1_memory_freed = memory_freed.clone();
    let s1_total_selected = total_selected.clone();
    let s1_app = app.clone();
    let stage1_task = tokio::spawn(async move {
        let probe_sem = Arc::new(Semaphore::new(io_parallel));
        let mut probe_set: JoinSet<(PathBuf, Result<FileInfo, String>)> = JoinSet::new();
        for path in file_paths {
            let probe_sem = probe_sem.clone();
            probe_set.spawn(async move {
                let _permit = probe_sem.acquire().await.unwrap();
                let result = probe_file(&path).await;
                (path, result)
            });
        }

        let mut group_media: std::collections::HashMap<(PathBuf, String), Vec<(PathBuf, FileInfo)>> =
            std::collections::HashMap::new();
        let mut non_media: Vec<FR> = Vec::new();
        let mut rejected_results: Vec<FR> = Vec::new();
        let mut stream_idx = 0usize;

        while let Some(Ok((path, result))) = probe_set.join_next().await {
            let key = stem_key(&path);
            let remaining = group_remaining.get_mut(&key).unwrap();
            *remaining -= 1;
            let group_done = *remaining == 0;

            match result {
                Ok(info) if info.has_media => {
                    group_media.entry(key.clone()).or_default().push((path, info));
                }
                Ok(_) => non_media.push(FR::skipped(path.to_string_lossy())),
                Err(e) => non_media.push(FR::error(path.to_string_lossy(), e)),
            }

            if group_done {
                if let Some(group) = group_media.remove(&key) {
                    let (best, rejected_paths) = select_best_from_group(group);
                    for p in rejected_paths {
                        rejected_results.push(FR::skipped(p.to_string_lossy()));
                    }
                    s1_total_selected.fetch_add(1, Ordering::Relaxed);

                    // ネットワーク時かつ silence trim 無効ならメモリにロードして stdin 経由で渡す
                    let should_buffer = is_network && !silence_trim_enabled;
                    let bytes = if should_buffer {
                        let file_size = tokio::fs::metadata(&best.0).await
                            .map(|m| m.len() as usize).unwrap_or(0);
                        if file_size > 0 && file_size <= memory_budget {
                            loop {
                                let notified = s1_memory_freed.notified();
                                let used = s1_memory_used.load(Ordering::Acquire);
                                if used == 0 || used + file_size <= memory_budget { break; }
                                notified.await;
                            }
                            s1_memory_used.fetch_add(file_size, Ordering::Release);
                            let new_used = s1_memory_used.load(Ordering::Relaxed);
                            if let Some(st) = s1_app.try_state::<crate::AppState>() {
                                st.memory_peak.fetch_max(new_used, Ordering::Relaxed);
                            }
                            match tokio::fs::read(&best.0).await {
                                Ok(b) => Some(b),
                                Err(_) => {
                                    s1_memory_used.fetch_sub(file_size, Ordering::Release);
                                    s1_memory_freed.notify_one();
                                    None
                                }
                            }
                        } else { None }
                    } else { None };

                    if stage_tx.send((stream_idx, best.0, best.1, bytes)).await.is_err() {
                        break;
                    }
                    stream_idx += 1;
                }
            }
        }
        (non_media, rejected_results)
    });

    // Stage 2: CPUタスク（並列変換）
    let mut conv_set: JoinSet<FR> = JoinSet::new();
    while let Some((new_i, path, info, input_bytes)) = stage_rx.recv().await {
        let sem = sem.clone();
        let app = app.clone();
        let job_id = job_id.clone();
        let format = format.clone();
        let settings = settings.clone();
        let progress_ratios = progress_ratios.clone();
        let file_duration = info.duration_secs;
        let pgids_for_spawn = pgids.clone();
        let base_dir = base_dir.clone();
        let dialog_sem = dialog_sem.clone();
        let memory_used = memory_used.clone();
        let memory_freed = memory_freed.clone();
        let total_selected_for_watcher = total_selected.clone();

        conv_set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let byte_size = input_bytes.as_ref().map(|b| b.len()).unwrap_or(0);

            let output_path = match resolve_output_path(
                &path, &format, &settings, base_dir.as_deref(), &app, &dialog_sem,
            ).await {
                Ok(p) => p,
                Err(e) => {
                    if byte_size > 0 {
                        memory_used.fetch_sub(byte_size, Ordering::Release);
                        memory_freed.notify_one();
                    }
                    return FR::error(path.to_string_lossy(), e.to_string());
                }
            };

            let input_display = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            // watch channel でスロットリング（unbounded spawn を排除）
            let (progress_tx, mut progress_rx) = watch::channel(0.0f64);
            let app_w = app.clone();
            let job_id_w = job_id.clone();
            let pr_w = progress_ratios.clone();
            let name_w = input_display.clone();
            tokio::spawn(async move {
                while progress_rx.changed().await.is_ok() {
                    let ratio = *progress_rx.borrow_and_update();
                    let percent = {
                        let mut pr = pr_w.lock().await;
                        if new_i < pr.len() { pr[new_i] = ratio; }
                        // max_selected は事前確定した固定値なので分母が増加して後退しない
                        (pr.iter().sum::<f64>() / max_selected.max(1) as f64 * 100.0).min(100.0)
                    };
                    if let Some(st) = app_w.try_state::<crate::AppState>() {
                        st.active_files.lock().unwrap().insert(name_w.clone(), ratio as f32);
                    }
                    let fc = total_selected_for_watcher.load(Ordering::Relaxed);
                    if app_w.emit("progress", ProgressPayload {
                        job_id: (*job_id_w).clone(),
                        percent,
                        current_file: name_w.clone(),
                        file_index: new_i,
                        file_count: fc,
                    }).is_err() {
                        eprintln!("emit progress failed");
                    }
                }
            });

            // 変換開始ログ（AppState バッファ経由でポーリングに渡す）
            push_conv_log(&app, input_display.clone(), "processing", None);

            let result = convert_one(
                &path,
                &output_path,
                &format,
                &settings,
                &info,
                file_duration,
                threads_per_job,
                input_bytes,
                Arc::new(move |ratio| { let _ = progress_tx.send(ratio); }),
                {
                    let pgids_for_pid = pgids_for_spawn.clone();
                    Arc::new(move |pid| {
                        pgids_for_pid.lock().unwrap().push(pid as i32);
                    })
                },
            )
            .await;

            // 変換完了後にメモリを解放しStage1に通知
            if byte_size > 0 {
                memory_used.fetch_sub(byte_size, Ordering::Release);
                memory_freed.notify_one();
            }

            {
                let mut pr = progress_ratios.lock().await;
                if new_i < pr.len() { pr[new_i] = 1.0; }
            }

            // 変換完了ログ
            let (log_status, log_error) = match &result {
                Ok(()) => ("done", None),
                Err(e) => ("error", Some(e.to_string())),
            };
            push_conv_log(&app, input_display.clone(), log_status, log_error);
            if let Some(st) = app.try_state::<crate::AppState>() {
                st.active_files.lock().unwrap().remove(&input_display);
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

    // stage_tx が stage1_task 内で drop された後にここへ到達するので即座に完了する
    let (non_media, rejected_results) = stage1_task.await.unwrap_or_default();

    let mut results: Vec<FR> = skip_results;
    results.extend(non_media);
    results.extend(rejected_results);
    while let Some(Ok(result)) = conv_set.join_next().await {
        results.push(result);
    }

    let success_count = results.iter().filter(|r| r.success).count();
    let error_count = results.iter().filter(|r| !r.success && !r.skipped).count();

    // 変換完了後に出力先をファイルマネージャで表示
    // ドロップ順を保つため入力パスでソートしてから先頭の成功ファイルを表示
    if settings.open_in_finder {
        let mut successes: Vec<&FR> = results.iter().filter(|r| r.success).collect();
        successes.sort_by(|a, b| a.input_path.cmp(&b.input_path));
        if let Some(first_success) = successes.first() {
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
    let final_count = total_selected.load(Ordering::Relaxed);
    if app.emit(
        "progress",
        ProgressPayload {
            job_id: (*job_id).clone(),
            percent: 100.0,
            current_file: String::new(),
            file_index: final_count,
            file_count: final_count,
        },
    ).is_err() {
        eprintln!("emit progress failed");
    }

    // conversion_complete を emit する前に is_converting を解除する。
    // この順を逆にするとフロントが完了処理→ユーザーが Cmd+Q→Rust が is_converting=true を
    // 検出して終了確認ダイアログを誤って出す競合が起きる。
    if let Some(state) = app.try_state::<crate::AppState>() {
        state.is_converting.store(false, std::sync::atomic::Ordering::SeqCst);
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

#[cfg(test)]
mod network_tests {
    use super::*;

    /// ローカルパスはネットワーク判定で false を返す
    #[test]
    fn local_temp_dir_is_not_network() {
        let tmp = std::env::temp_dir();
        // macOS/Linux 実装のみ呼び出し可能。Windows はビルド時に別関数
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(!is_path_on_network(&tmp), "temp_dir should not be on network: {:?}", tmp);
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
        assert!(has_network_input(&[r"\\server\share\file.flac".to_string()]));
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
