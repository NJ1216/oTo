use std::path::Path;

pub fn resolve_binary(name: &str) -> String {
    // Check PATH first via which/where
    #[cfg(unix)]
    {
        if let Ok(output) = std::process::Command::new("which").arg(name).output() {
            if output.status.success() {
                if let Ok(path) = String::from_utf8(output.stdout) {
                    let path = path.trim().to_string();
                    if !path.is_empty() && Path::new(&path).exists() {
                        return path;
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(output) = std::process::Command::new("where").arg(name).output() {
            if output.status.success() {
                if let Ok(path) = String::from_utf8(output.stdout) {
                    let path = path.lines().next().unwrap_or("").trim().to_string();
                    if !path.is_empty() && Path::new(&path).exists() {
                        return path;
                    }
                }
            }
        }
    }

    // Fallback: common Homebrew / system paths
    for dir in ["/opt/homebrew/bin/", "/usr/local/bin/"] {
        let path = format!("{}{}", dir, name);
        if Path::new(&path).exists() {
            return path;
        }
    }
    name.to_string()
}

pub fn ffmpeg_path() -> String { resolve_binary("ffmpeg") }
pub fn ffprobe_path() -> String { resolve_binary("ffprobe") }

pub fn friendly_ffmpeg_error(e: &str) -> String {
    let lower = e.to_lowercase();
    if lower.contains("no such file") || lower.contains("not found") || lower.contains("cannot find the file") {
        "FFmpeg が見つかりません。https://ffmpeg.org からインストールしてください。".into()
    } else {
        e.into()
    }
}
