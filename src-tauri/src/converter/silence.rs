use super::binary::ffmpeg_path;
use super::process::{configure_ffmpeg_command, ProcessTracker};
use std::path::Path;

const DIRECT_BOUNDARY_SCAN_SECS: f64 = 60.0;
const SILENCE_PROCESS_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Default, PartialEq)]
pub struct DirectSilenceTrim {
    pub start_secs: Option<f64>,
    pub end_secs: Option<f64>,
}

pub struct SilenceConfig {
    pub db: f64,
    pub min_duration_secs: f64,
    pub total_duration_secs: f64,
    pub audio_stream_index: Option<usize>,
}

pub struct SilenceContext<'a> {
    pub cancellation: &'a crate::JobCancellation,
    pub processes: &'a ProcessTracker,
}

#[derive(Default)]
struct ScanWindow {
    seek_secs: Option<f64>,
    duration_secs: Option<f64>,
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

    for line in stderr.lines() {
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

/// テスト・後方互換用の wrapper（regions のみ返す）。
#[cfg(test)]
fn parse_silence_regions(stderr: &str) -> Vec<(f64, f64)> {
    parse_silence_regions_full(stderr).0
}

fn boundary_flags(
    regions: &[(f64, f64)],
    unclosed_tail: Option<f64>,
    decoded_duration: Option<f64>,
    total_duration: f64,
) -> (bool, bool) {
    let tolerance = 0.05;
    let effective_end = decoded_duration.unwrap_or(total_duration);
    let has_start = regions.iter().any(|(start, _)| *start <= tolerance)
        || unclosed_tail.is_some_and(|start| start <= tolerance);
    let has_end = unclosed_tail.is_some()
        || regions
            .iter()
            .any(|(_, end)| (effective_end - *end).abs() <= tolerance);
    (has_start, has_end)
}

fn direct_head_boundary(regions: &[(f64, f64)], scan_duration: f64) -> Option<f64> {
    let tolerance = 0.05;
    regions
        .iter()
        .find(|(start, end)| *start <= tolerance && *end < (scan_duration - tolerance).max(0.0))
        .map(|(_, end)| *end)
}

fn direct_tail_boundary(
    regions: &[(f64, f64)],
    unclosed_tail: Option<f64>,
    decoded_duration: Option<f64>,
    scan_duration: f64,
) -> Option<f64> {
    let tolerance = 0.05;
    if let Some(start) = unclosed_tail {
        return (start > tolerance && start <= scan_duration + tolerance).then_some(start);
    }
    let effective_end = decoded_duration.unwrap_or(scan_duration);
    regions
        .iter()
        .rev()
        .find(|(start, end)| *start > tolerance && (effective_end - *end).abs() <= tolerance)
        .map(|(start, _)| *start)
}

async fn run_silence_detect_process(
    path: &Path,
    config: &SilenceConfig,
    window: ScanWindow,
    context: &SilenceContext<'_>,
) -> Result<(Vec<(f64, f64)>, Option<f64>, Option<f64>), String> {
    if context.cancellation.is_cancelled() {
        return Err("conversion cancelled".to_string());
    }
    let ffmpeg = ffmpeg_path();
    let filter = format!(
        "asetpts=PTS-STARTPTS,silencedetect=noise={}dB:duration={:.4}",
        config.db, config.min_duration_secs
    );
    let mut cmd = tokio::process::Command::new(&ffmpeg);
    if let Some(seek) = window.seek_secs.filter(|seek| *seek > 0.0) {
        cmd.arg("-ss").arg(format!("{seek:.6}"));
    }
    cmd.arg("-i").arg(path);
    if let Some(duration) = window.duration_secs {
        cmd.arg("-t").arg(format!("{duration:.6}"));
    }
    if let Some(stream_index) = config.audio_stream_index {
        cmd.args(["-map", &format!("0:{stream_index}")]);
    }
    cmd.args(["-vn", "-af", &filter, "-f", "null", "-"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    configure_ffmpeg_command(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return Ok((Vec::new(), None, None)),
    };
    let registration = context.processes.register(child.id());
    let stderr_task = child.stderr.take().map(|mut stderr| {
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            use tokio::io::AsyncReadExt;
            let _ = stderr.read_to_end(&mut bytes).await;
            bytes
        })
    });

    enum End {
        Finished,
        Cancelled,
        TimedOut,
    }
    let end = tokio::select! {
        result = child.wait() => {
            let _ = result;
            End::Finished
        }
        _ = context.cancellation.cancelled() => End::Cancelled,
        _ = tokio::time::sleep(std::time::Duration::from_secs(SILENCE_PROCESS_TIMEOUT_SECS)) => End::TimedOut,
    };
    if !matches!(end, End::Finished) {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    drop(registration);
    let stderr = match stderr_task {
        Some(task) => task.await.unwrap_or_default(),
        None => Vec::new(),
    };
    match end {
        End::Cancelled => Err("conversion cancelled".to_string()),
        End::TimedOut => Ok((Vec::new(), None, None)),
        End::Finished => Ok(parse_silence_regions_full(&String::from_utf8_lossy(
            &stderr,
        ))),
    }
}

pub async fn detect_boundary_silence_cancellable(
    path: &Path,
    config: &SilenceConfig,
    context: &SilenceContext<'_>,
) -> Result<(bool, bool), String> {
    let (regions, unclosed, decoded) =
        run_silence_detect_process(path, config, ScanWindow::default(), context).await?;
    Ok(boundary_flags(
        &regions,
        unclosed,
        decoded,
        config.total_duration_secs,
    ))
}

pub async fn detect_direct_boundary_trim(
    path: &Path,
    config: &SilenceConfig,
    context: &SilenceContext<'_>,
) -> Result<DirectSilenceTrim, String> {
    let head_duration = config
        .total_duration_secs
        .clamp(0.0, DIRECT_BOUNDARY_SCAN_SECS);
    let (head_regions, _, _) = run_silence_detect_process(
        path,
        config,
        ScanWindow {
            seek_secs: None,
            duration_secs: Some(head_duration),
        },
        context,
    )
    .await?;
    let start_secs = direct_head_boundary(&head_regions, head_duration);

    let tail_duration = config
        .total_duration_secs
        .clamp(0.0, DIRECT_BOUNDARY_SCAN_SECS);
    let tail_offset = (config.total_duration_secs - tail_duration).max(0.0);
    let (tail_regions, tail_unclosed, tail_decoded) = run_silence_detect_process(
        path,
        config,
        ScanWindow {
            seek_secs: Some(tail_offset),
            duration_secs: Some(tail_duration),
        },
        context,
    )
    .await?;
    let end_secs = direct_tail_boundary(&tail_regions, tail_unclosed, tail_decoded, tail_duration)
        .map(|relative| tail_offset + relative)
        .filter(|end| start_secs.is_none_or(|start| *end > start));

    Ok(DirectSilenceTrim {
        start_secs,
        end_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_returns_empty() {
        assert!(parse_silence_regions("").is_empty());
    }

    #[test]
    fn parse_single_complete_region() {
        let stderr = "[silencedetect] silence_start: 0\n[silencedetect] silence_end: 1.5 | silence_duration: 1.5\n";
        let regions = parse_silence_regions(stderr);
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
        let regions = parse_silence_regions(stderr);
        assert_eq!(regions, vec![(0.0, 0.5), (10.0, 10.8)]);
    }

    #[test]
    fn parse_negative_start_clamped_to_zero() {
        // OPUS のプリスキップ影響で silence_start が負になることがある
        let stderr =
            "[silencedetect] silence_start: -0.1\n[silencedetect] silence_end: 1.0 | ...\n";
        let regions = parse_silence_regions(stderr);
        assert_eq!(regions, vec![(0.0, 1.0)]);
    }

    #[test]
    fn parse_unclosed_start_not_captured() {
        // silence_end がない場合は parse_silence_regions は捕捉しない（detect_boundary_silence が補完）
        let stderr = "[silencedetect] silence_start: 5.0\n";
        assert!(parse_silence_regions(stderr).is_empty());
    }

    #[test]
    fn parse_end_without_preceding_start_is_ignored() {
        let stderr = "[silencedetect] silence_end: 1.0 | silence_duration: 1.0\n";
        assert!(parse_silence_regions(stderr).is_empty());
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
    fn parse_no_time_line_returns_none_decoded() {
        let stderr = "[silencedetect] silence_start: 0\n[silencedetect] silence_end: 1.5 |\n";
        let (_, _, decoded) = parse_silence_regions_full(stderr);
        assert_eq!(decoded, None);
    }

    #[test]
    fn direct_head_requires_a_boundary_within_the_scan_window() {
        assert_eq!(direct_head_boundary(&[(0.0, 12.5)], 60.0), Some(12.5));
        assert_eq!(direct_head_boundary(&[(0.0, 60.0)], 60.0), None);
        assert_eq!(direct_head_boundary(&[], 60.0), None);
    }

    #[test]
    fn direct_tail_does_not_trim_when_the_whole_window_is_silent() {
        assert_eq!(direct_tail_boundary(&[], Some(0.0), Some(60.0), 60.0), None);
        assert_eq!(
            direct_tail_boundary(&[], Some(42.0), Some(60.0), 60.0),
            Some(42.0)
        );
    }
}
