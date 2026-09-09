use std::path::Path;
use std::sync::OnceLock;

fn resolve_binary(name: &str) -> String {
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
        // CREATE_NO_WINDOW を付けないと `where` のコンソールが一瞬表示される。
        // ffmpeg 起動前に毎回これが見えていた「ちらつき」の原因。
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("where");
        cmd.arg(name).creation_flags(0x08000000);
        if let Ok(output) = cmd.output() {
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

// アプリ生存期間中はパスをキャッシュする。
// 変換のたびに ffmpeg_path() が呼ばれるため、毎回 `where`/`which` を起動すると
// Windows ではちらつきが残るし、Unix でもプロセス起動コストが累積する。
static FFMPEG_PATH: OnceLock<String> = OnceLock::new();
static FFPROBE_PATH: OnceLock<String> = OnceLock::new();

pub fn ffmpeg_path() -> String {
    FFMPEG_PATH.get_or_init(|| resolve_binary("ffmpeg")).clone()
}
pub fn ffprobe_path() -> String {
    FFPROBE_PATH
        .get_or_init(|| resolve_binary("ffprobe"))
        .clone()
}

pub fn friendly_ffmpeg_error(e: &str) -> String {
    let lower = e.to_lowercase();
    if lower.contains("no such file")
        || lower.contains("not found")
        || lower.contains("cannot find the file")
    {
        "FFmpeg が見つかりません。https://ffmpeg.org からインストールしてください。".into()
    } else {
        e.into()
    }
}
