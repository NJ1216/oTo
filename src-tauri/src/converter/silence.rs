use std::path::Path;
use super::binary::ffmpeg_path;

/// Parse FFmpeg silence detection output and return all silence regions.
pub fn parse_silence_regions(stderr: &str) -> Vec<(f64, f64)> {
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

    all_regions
}

/// Run FFmpeg silence detection on a file and return parsed regions.
pub fn run_silence_detect(path: &Path, db: f64, min_dur_secs: f64) -> Vec<(f64, f64)> {
    let ffmpeg = ffmpeg_path();
    let filter = format!("silencedetect=noise={db}dB:duration={min_dur_secs:.4}");

    let mut cmd = std::process::Command::new(&ffmpeg);
    cmd.args(["-i", &path.to_string_lossy(), "-af", &filter, "-f", "null", "-"])
       .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }

    let out = match cmd.output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    parse_silence_regions(&stderr)
}

/// Detect silence regions at the beginning and end of the file.
/// Returns (has_start_silence, has_end_silence).
pub fn detect_boundary_silence(path: &Path, db: f64, min_dur_secs: f64, total_duration: f64) -> (bool, bool) {
    let mut all_regions = run_silence_detect(path, db, min_dur_secs);

    // Handle silence that extends to the end of the file
    // (run_silence_detect doesn't capture unclosed starts, so we re-parse)
    let ffmpeg = ffmpeg_path();
    let filter = format!("silencedetect=noise={db}dB:duration={min_dur_secs:.4}");
    let mut cmd = std::process::Command::new(&ffmpeg);
    cmd.args(["-i", &path.to_string_lossy(), "-af", &filter, "-f", "null", "-"])
       .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }
    if let Ok(out) = cmd.output() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let mut cur_start: Option<f64> = None;
        for line in stderr.lines() {
            if let Some(pos) = line.find("silence_start:") {
                if let Ok(t) = line[pos + 14..].trim().parse::<f64>() {
                    cur_start = Some(t.max(0.0));
                }
            } else if let Some(_pos) = line.find("silence_end:") {
                cur_start.take();
            }
        }
        if let Some(start) = cur_start {
            all_regions.push((start, total_duration));
        }
    }

    if all_regions.is_empty() {
        return (false, false);
    }

    let tolerance = 0.05; // 50ms tolerance
    let has_start = all_regions.iter().any(|(s, _)| *s <= tolerance);
    let has_end = all_regions.iter().any(|(_, e)| (total_duration - *e).abs() <= tolerance);

    (has_start, has_end)
}
