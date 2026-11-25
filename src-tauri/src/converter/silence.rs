use super::binary::ffmpeg_path;
use super::process::{configure_ffmpeg_command, ProcessTracker};
use super::trim::{decide_trim, TrimDecision};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// A scan fails only after this much active (non-paused) time without FFmpeg's `time=` advancing.
const SILENCE_STALL_TIMEOUT: Duration = Duration::from_secs(60);

pub struct SilenceConfig {
    pub db: f64,
    pub min_duration_secs: f64,
    pub total_duration_secs: f64,
    pub audio_stream_index: Option<usize>,
}

pub struct SilenceContext<'a> {
    pub pause: &'a crate::SessionPause,
    pub cancellation: &'a crate::JobCancellation,
    pub processes: &'a ProcessTracker,
    pub on_progress: Arc<dyn Fn(f64) + Send + Sync>,
}

/// Drain all bytes, including invalid UTF-8, while observing CR/LF-delimited
/// FFmpeg stats. Preserve the original stderr for the existing silence parser.
/// `advanced` is signalled on every strict increase of `time=`, whether or not
/// the duration (and so the progress ratio) is known.
async fn read_silence_stderr(
    mut stderr: impl AsyncRead + Unpin,
    duration: f64,
    on_progress: Arc<dyn Fn(f64) + Send + Sync>,
    advanced: tokio::sync::mpsc::UnboundedSender<()>,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut line = Vec::new();
    let mut last_ratio = 0.0_f64;
    let mut last_time = f64::NEG_INFINITY;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stderr.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        for &byte in &buffer[..read] {
            if byte == b'\r' || byte == b'\n' {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_start();
                if text.starts_with("size=")
                    || text.starts_with("Lsize=")
                    || text.starts_with("frame=")
                {
                    if let Some(time) = text
                        .split_once("time=")
                        .and_then(|(_, rest)| rest.split_whitespace().next())
                        .and_then(parse_hms)
                        .filter(|time| time.is_finite())
                    {
                        if time > last_time {
                            last_time = time;
                            let _ = advanced.send(());
                        }
                        if duration.is_finite() && duration > 0.0 {
                            let ratio = (time / duration).clamp(0.0, 1.0);
                            if ratio > last_ratio {
                                last_ratio = ratio;
                                on_progress(ratio);
                            }
                        }
                    }
                }
                line.clear();
            } else {
                line.push(byte);
            }
        }
    }
    Ok(bytes)
}

/// FFmpeg の `time=HH:MM:SS.xx` 文字列を秒に変換する。`N/A` 等は None。
fn parse_hms(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let sec: f64 = parts[2].parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}

/// Parse FFmpeg silence detection output and return all silence regions.
/// 末尾無音 (silence_end が無い未クローズの silence_start) は
/// `unclosed_tail` に最後の silence_start 時刻として返す。
/// 3 番目の戻り値は FFmpeg の進捗 `time=` から得た **実デコード長**。
/// raw AAC / VBR / 動画コンテナ等ではコンテナのメタデータ長と実デコード長が
/// 大きくズレるため、末尾無音判定にはこの実デコード長を優先して用いる。
pub fn parse_silence_regions_full(stderr: &str) -> (Vec<(f64, f64)>, Option<f64>, Option<f64>) {
    let mut all_regions: Vec<(f64, f64)> = Vec::new();
    let mut cur_start: Option<f64> = None;
    let mut decoded_dur: Option<f64> = None;

    // FFmpeg overwrites stats with CR even when stderr is piped. Treat each
    // update as a record so tail detection uses the last decoded duration.
    for line in stderr.split(['\r', '\n']) {
        if let Some(pos) = line.find("silence_start:") {
            if let Ok(t) = line[pos + 14..].trim().parse::<f64>() {
                cur_start = Some(t.max(0.0));
            }
        } else if let Some(pos) = line.find("silence_end:") {
            if let Some(start) = cur_start.take() {
                let s = line[pos + 12..].split('|').next().unwrap_or("").trim();
                if let Ok(end) = s.parse::<f64>() {
                    all_regions.push((start, end));
                }
            }
        }
        // 進捗/統計行（例: `size=N/A time=00:00:07.03 bitrate=...`）から実デコード長を拾う。
        // 複数回出力されるため最後の有効値を採用する。
        if let Some(tpos) = line.find("time=") {
            let tok = line[tpos + 5..].split_whitespace().next().unwrap_or("");
            if let Some(t) = parse_hms(tok) {
                decoded_dur = Some(t);
            }
        }
    }

    (all_regions, cur_start, decoded_dur)
}

async fn run_silence_detect_process(
    path: &Path,
    config: &SilenceConfig,
    context: &SilenceContext<'_>,
    progress_range: (f64, f64),
) -> Result<(Vec<(f64, f64)>, Option<f64>, Option<f64>), String> {
    if !context.pause.wait_until_resumed(context.cancellation).await {
        return Err("conversion cancelled".to_string());
    }
    let (progress_start, progress_end) = progress_range;
    (context.on_progress)(progress_start);
    let ffmpeg = ffmpeg_path();
    let filter = format!(
        "asetpts=PTS-STARTPTS,silencedetect=noise={}dB:duration={:.4}",
        config.db, config.min_duration_secs
    );
    let mut cmd = tokio::process::Command::new(&ffmpeg);
    cmd.arg("-i").arg(path);
    if let Some(stream_index) = config.audio_stream_index {
        cmd.args(["-map", &format!("0:{stream_index}")]);
    }
    cmd.args(["-stats", "-vn", "-af", &filter, "-f", "null", "-"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    configure_ffmpeg_command(&mut cmd);

    execute_silence_command(
        &mut cmd,
        config.total_duration_secs,
        context,
        (progress_start, progress_end),
        SILENCE_STALL_TIMEOUT,
    )
    .await
}

/// Completes once `stall_limit` of active (non-paused) time passes without a
/// signal on `ticks`. Each signal restarts the budget; once the sender is
/// closed a fresh full budget starts and runs out unless the caller is dropped first.
async fn watch_stall(
    pause: &crate::SessionPause,
    stall_limit: Duration,
    mut ticks: tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    loop {
        tokio::select! {
            tick = ticks.recv() => {
                if tick.is_none() {
                    pause.wait_for_active_duration(stall_limit).await;
                    return;
                }
            }
            _ = pause.wait_for_active_duration(stall_limit) => return,
        }
    }
}

async fn execute_silence_command(
    cmd: &mut tokio::process::Command,
    scan_duration: f64,
    context: &SilenceContext<'_>,
    progress_range: (f64, f64),
    stall_limit: Duration,
) -> Result<(Vec<(f64, f64)>, Option<f64>, Option<f64>), String> {
    let (progress_start, progress_end) = progress_range;
    let mut child = cmd
        .spawn()
        .map_err(|error| format!("silence analysis could not start: {error}"))?;
    let registration = context.processes.register(child.id());
    let (advanced, ticks) = tokio::sync::mpsc::unbounded_channel();
    let stderr_task = child.stderr.take().map(|stderr| {
        let on_progress = context.on_progress.clone();
        tokio::spawn(read_silence_stderr(
            stderr,
            scan_duration,
            Arc::new(move |ratio| {
                on_progress(progress_start + ratio * (progress_end - progress_start));
            }),
            advanced,
        ))
    });

    enum End {
        Finished(std::io::Result<std::process::ExitStatus>),
        Cancelled,
        Stalled,
    }
    let end = tokio::select! {
        result = child.wait() => End::Finished(result),
        _ = context.cancellation.cancelled() => End::Cancelled,
        _ = watch_stall(context.pause, stall_limit, ticks) => End::Stalled,
    };
    if !matches!(end, End::Finished(_)) {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    drop(registration);
    let stderr = match stderr_task {
        Some(task) => task
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string())),
        None => Ok(Vec::new()),
    };
    match end {
        End::Cancelled => Err("conversion cancelled".to_string()),
        End::Stalled => Err(format!(
            "silence analysis stalled: no progress for {} seconds of active processing",
            stall_limit.as_secs()
        )),
        End::Finished(result) => {
            let status =
                result.map_err(|error| format!("silence analysis wait failed: {error}"))?;
            let stderr =
                stderr.map_err(|error| format!("silence analysis output failed: {error}"))?;
            let text = String::from_utf8_lossy(&stderr);
            if !status.success() {
                return Err(format!(
                    "silence analysis failed ({status}): {}",
                    text.lines()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or("no diagnostic output")
                ));
            }
            (context.on_progress)(progress_end);
            Ok(parse_silence_regions_full(&text))
        }
    }
}

/// Scan the whole file once and decide how to trim it.
pub async fn analyze_trim(
    path: &Path,
    config: &SilenceConfig,
    context: &SilenceContext<'_>,
) -> Result<TrimDecision, String> {
    let (regions, unclosed_tail, decoded_duration) =
        run_silence_detect_process(path, config, context, (0.0, 1.0)).await?;
    Ok(decide_trim(
        &regions,
        unclosed_tail,
        decoded_duration,
        config.total_duration_secs,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn analysis_progress_arrives_before_eof_and_keeps_all_stderr_bytes() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(256);
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(read_silence_stderr(
            reader,
            10.0,
            Arc::new(move |ratio| {
                send.send(ratio).unwrap();
            }),
            tokio::sync::mpsc::unbounded_channel().0,
        ));
        let first = b"invalid metadata=\xff\nsize=N/A time=00:00:05";
        writer.write_all(first).await.unwrap();
        let second = b".00 bitrate=N/A\rsize=N/A time=N/A\nsize=N/A time=00:00:01.00\r";
        writer.write_all(second).await.unwrap();
        let ratio = tokio::time::timeout(std::time::Duration::from_secs(1), receive.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ratio, 0.5,
            "progress must be delivered while the pipe is still open"
        );
        let last = b"[silencedetect] silence_start: 8\nsize=N/A time=00:00:10.00 bitrate=N/A\n";
        writer.write_all(last).await.unwrap();
        drop(writer);
        let bytes = task.await.unwrap().unwrap();
        assert_eq!(
            bytes,
            [first.as_slice(), second.as_slice(), last.as_slice()].concat()
        );
        assert_eq!(receive.recv().await, Some(1.0));
        assert_eq!(
            receive.recv().await,
            None,
            "invalid/backward timestamps must not regress progress"
        );
    }

    #[tokio::test]
    async fn spawn_failure_is_an_error_without_completed_analysis_progress() {
        let pause = crate::SessionPause::new();
        let cancellation = crate::JobCancellation::new();
        let processes = ProcessTracker::new(
            Arc::new(|_| panic!("no child was spawned")),
            Arc::new(|_| {}),
        );
        let context = SilenceContext {
            pause: &pause,
            cancellation: &cancellation,
            processes: &processes,
            on_progress: Arc::new(|_| panic!("failed scan must not report completion")),
        };
        let missing = std::env::temp_dir().join(format!("missing-ffmpeg-{}", uuid::Uuid::new_v4()));
        let error = execute_silence_command(
            &mut tokio::process::Command::new(missing),
            1.0,
            &context,
            (0.0, 1.0),
            Duration::from_secs(30),
        )
        .await
        .unwrap_err();
        assert!(error.contains("could not start"));
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn stall_without_progress_kills_child_and_reports_stalled() {
        let pause = Arc::new(crate::SessionPause::new());
        let cancellation = crate::JobCancellation::new();
        let processes = ProcessTracker::for_session(pause.clone());
        let context = SilenceContext {
            pause: &pause,
            cancellation: &cancellation,
            processes: &processes,
            on_progress: Arc::new(|_| panic!("stalled scan must not report completion")),
        };
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("30");
        configure_ffmpeg_command(&mut command);
        let result = execute_silence_command(
            &mut command,
            30.0,
            &context,
            (0.0, 1.0),
            SILENCE_STALL_TIMEOUT,
        )
        .await;
        assert!(result.unwrap_err().contains("stalled"));
        assert!(pause.processes.lock().unwrap().is_empty());
    }

    /// Race a fake stderr source against the stall watchdog, as the real scan does.
    async fn scan_fake_stderr<F, Fut>(pause: Arc<crate::SessionPause>, script: F) -> Result<(), ()>
    where
        F: FnOnce(tokio::io::DuplexStream, Arc<crate::SessionPause>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (writer, reader) = tokio::io::duplex(1024);
        let (advanced, ticks) = tokio::sync::mpsc::unbounded_channel();
        let mut task = tokio::spawn(read_silence_stderr(
            reader,
            f64::NAN,
            Arc::new(|_| {}),
            advanced,
        ));
        tokio::spawn(script(writer, pause.clone()));
        tokio::select! {
            _ = watch_stall(&pause, SILENCE_STALL_TIMEOUT, ticks) => Err(()),
            result = &mut task => {
                result.unwrap().unwrap();
                Ok(())
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn steady_progress_never_stalls() {
        use tokio::io::AsyncWriteExt;
        let pause = Arc::new(crate::SessionPause::new());
        let result = scan_fake_stderr(pause, |mut writer, _| async move {
            for step in 1..=20_u32 {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let line = format!(
                    "size=N/A time=00:{:02}:{:02}.00 bitrate=N/A\r",
                    step / 2,
                    (step % 2) * 30
                );
                writer.write_all(line.as_bytes()).await.unwrap();
            }
        })
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn paused_time_does_not_count_toward_stall() {
        use tokio::io::AsyncWriteExt;
        let pause = Arc::new(crate::SessionPause::new());
        let result = scan_fake_stderr(pause, |mut writer, pause| async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            writer
                .write_all(b"size=N/A time=00:00:01.00\r")
                .await
                .unwrap();
            pause.set_paused(true);
            tokio::time::sleep(Duration::from_secs(600)).await;
            pause.set_paused(false);
            tokio::time::sleep(Duration::from_secs(10)).await;
            writer
                .write_all(b"size=N/A time=00:00:02.00\r")
                .await
                .unwrap();
        })
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_time_without_advance_stalls() {
        use tokio::io::AsyncWriteExt;
        let pause = Arc::new(crate::SessionPause::new());
        let result = scan_fake_stderr(pause, |mut writer, _| async move {
            // time= never increases, so repeated stats lines must not keep it alive.
            for _ in 0..10 {
                writer
                    .write_all(b"size=N/A time=00:00:01.00\r")
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        })
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn parse_empty_returns_empty() {
        assert!(parse_silence_regions_full("").0.is_empty());
    }

    #[test]
    fn parse_single_complete_region() {
        let stderr = "[silencedetect] silence_start: 0\n[silencedetect] silence_end: 1.5 | silence_duration: 1.5\n";
        let regions = parse_silence_regions_full(stderr).0;
        assert_eq!(regions, vec![(0.0, 1.5)]);
    }

    #[test]
    fn parse_multiple_regions() {
        let stderr = concat!(
            "[silencedetect] silence_start: 0\n",
            "[silencedetect] silence_end: 0.5 | silence_duration: 0.5\n",
            "[silencedetect] silence_start: 10\n",
            "[silencedetect] silence_end: 10.8 | silence_duration: 0.8\n",
        );
        let regions = parse_silence_regions_full(stderr).0;
        assert_eq!(regions, vec![(0.0, 0.5), (10.0, 10.8)]);
    }

    #[test]
    fn parse_negative_start_clamped_to_zero() {
        // OPUS のプリスキップ影響で silence_start が負になることがある
        let stderr =
            "[silencedetect] silence_start: -0.1\n[silencedetect] silence_end: 1.0 | ...\n";
        let regions = parse_silence_regions_full(stderr).0;
        assert_eq!(regions, vec![(0.0, 1.0)]);
    }

    #[test]
    fn parse_unclosed_start_not_captured() {
        // silence_end がない場合は regions に含めない（decide_trim が unclosed_tail から補完）
        let stderr = "[silencedetect] silence_start: 5.0\n";
        assert!(parse_silence_regions_full(stderr).0.is_empty());
    }

    #[test]
    fn parse_end_without_preceding_start_is_ignored() {
        let stderr = "[silencedetect] silence_end: 1.0 | silence_duration: 1.0\n";
        assert!(parse_silence_regions_full(stderr).0.is_empty());
    }

    #[test]
    fn parse_hms_valid_and_invalid() {
        assert_eq!(parse_hms("00:00:07.03"), Some(7.03));
        assert_eq!(parse_hms("01:02:03.5"), Some(3723.5));
        assert_eq!(parse_hms("N/A"), None);
        assert_eq!(parse_hms("7.03"), None);
    }

    #[test]
    fn parse_extracts_decoded_duration_from_time() {
        // raw AAC ではコンテナ長が誤っていても time= から実デコード長が取れる
        let stderr = concat!(
            "[Parsed_silencedetect_0] silence_start: 4.999977\n",
            "[Parsed_silencedetect_0] silence_end: 7.035646 | silence_duration: 2.01\n",
            "size=N/A time=00:00:07.03 bitrate=N/A speed=2.4e+03x\n",
        );
        let (regions, unclosed, decoded) = parse_silence_regions_full(stderr);
        assert_eq!(regions, vec![(4.999977, 7.035646)]);
        assert_eq!(unclosed, None);
        assert_eq!(decoded, Some(7.03));
    }

    #[test]
    fn carriage_return_stats_use_the_final_duration_for_closed_tail_silence() {
        let stderr = concat!(
            "[silencedetect] silence_start: 8\n",
            "[silencedetect] silence_end: 10 | silence_duration: 2\n",
            "size=N/A time=00:00:09.00 bitrate=N/A\r",
            "size=N/A time=00:00:10.00 bitrate=N/A\r\n",
        );
        let (regions, _, decoded) = parse_silence_regions_full(stderr);
        assert_eq!(decoded, Some(10.0));
        assert_eq!(regions, vec![(8.0, 10.0)]);
    }

    #[test]
    fn parse_no_time_line_returns_none_decoded() {
        let stderr = "[silencedetect] silence_start: 0\n[silencedetect] silence_end: 1.5 |\n";
        let (_, _, decoded) = parse_silence_regions_full(stderr);
        assert_eq!(decoded, None);
    }
}
