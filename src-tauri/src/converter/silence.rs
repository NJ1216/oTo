use std::path::Path;
use super::binary::ffmpeg_path;

/// Parse FFmpeg silence detection output and return all silence regions.
/// 末尾無音 (silence_end が無い未クローズの silence_start) は
/// `unclosed_tail` に最後の silence_start 時刻として返す。
pub fn parse_silence_regions_full(stderr: &str) -> (Vec<(f64, f64)>, Option<f64>) {
    let mut all_regions: Vec<(f64, f64)> = Vec::new();
    let mut cur_start: Option<f64> = None;

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
    }

    (all_regions, cur_start)
}

/// テスト・後方互換用の wrapper（regions のみ返す）。
#[cfg(test)]
fn parse_silence_regions(stderr: &str) -> Vec<(f64, f64)> {
    parse_silence_regions_full(stderr).0
}

fn run_silence_detect_raw(path: &Path, db: f64, min_dur_secs: f64) -> (Vec<(f64, f64)>, Option<f64>) {
    let ffmpeg = ffmpeg_path();
    let filter = format!("silencedetect=noise={db}dB:duration={min_dur_secs:.4}");

    let mut cmd = std::process::Command::new(&ffmpeg);
    cmd.args(["-i", &path.to_string_lossy(), "-af", &filter, "-f", "null", "-"])
       .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }

    let out = match cmd.output() {
        Ok(o) => o,
        Err(_) => return (Vec::new(), None),
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    parse_silence_regions_full(&stderr)
}

/// Run FFmpeg silence detection on a file and return parsed regions
/// (末尾未クローズ silence は含めない、互換 API)。
pub fn run_silence_detect(path: &Path, db: f64, min_dur_secs: f64) -> Vec<(f64, f64)> {
    run_silence_detect_raw(path, db, min_dur_secs).0
}

/// Detect silence regions at the beginning and end of the file.
/// Returns (has_start_silence, has_end_silence).
/// 内部で FFmpeg を 1 回だけ実行し、末尾無音 (silence_end が来ないまま EOF) は
/// `parse_silence_regions_full` の `unclosed_tail` から補完する。
pub fn detect_boundary_silence(path: &Path, db: f64, min_dur_secs: f64, total_duration: f64) -> (bool, bool) {
    let (mut all_regions, unclosed_tail) = run_silence_detect_raw(path, db, min_dur_secs);

    if let Some(start) = unclosed_tail {
        all_regions.push((start, total_duration));
    }

    if all_regions.is_empty() {
        return (false, false);
    }

    let tolerance = 0.05; // 50ms tolerance
    let has_start = all_regions.iter().any(|(s, _)| *s <= tolerance);
    let has_end = all_regions.iter().any(|(_, e)| (total_duration - *e).abs() <= tolerance);

    (has_start, has_end)
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
        let stderr = "[silencedetect] silence_start: -0.1\n[silencedetect] silence_end: 1.0 | ...\n";
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
}
