use super::binary::{ffprobe_path, friendly_ffmpeg_error};
use super::types::{AudioTrack, FileInfo};
use std::collections::HashMap;
use std::path::Path;

/// FFprobe が入力をメディアとして解析できなかった場合の結果。
///
/// フォルダを丸ごと指定したときは `.DS_Store` やテキストファイルも走査対象になる。
/// それらは変換失敗ではなく、FFmpeg が対応するメディアではないため対象外として扱う。
fn non_media_file_info() -> FileInfo {
    FileInfo {
        duration_secs: 0.0,
        tags: HashMap::new(),
        bits_per_sample: 16,
        cover_art_stream_idx: None,
        has_media: false,
        is_lossless: false,
        bit_rate_bps: 0,
        audio_tracks: Vec::new(),
    }
}

pub async fn probe_file(
    path: &Path,
    cancellation: &crate::JobCancellation,
) -> Result<FileInfo, String> {
    if cancellation.is_cancelled() {
        return Err("conversion cancelled".to_string());
    }
    let ffprobe = ffprobe_path();
    let mut probe_cmd = tokio::process::Command::new(&ffprobe);
    probe_cmd.args([
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ]);
    probe_cmd.arg(path); // Pass as OsStr to support non-UTF-8 filenames
    probe_cmd.kill_on_drop(true);
    #[cfg(windows)]
    probe_cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let out = match tokio::select! {
        output = probe_cmd.output() => output,
        _ = cancellation.cancelled() => return Err("conversion cancelled".to_string()),
    } {
        Ok(output) => output,
        Err(e) => {
            let msg = friendly_ffmpeg_error(&e.to_string());
            return Err(format!("{}: {}", path.display(), msg));
        }
    };

    // FFprobe の非ゼロ終了は「対応しない入力」の通常の結果でもある。
    // ここをエラーにすると `.DS_Store` や `.txt` まで変換失敗として通知されてしまう。
    // FFmpeg/FFprobe 自体を起動できない場合は、上の Err で引き続き通知する。
    if !out.status.success() {
        return Ok(non_media_file_info());
    }
    let json: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(json) => json,
        Err(_) => return Ok(non_media_file_info()),
    };

    let mut duration = 0.0f64;
    let mut tags = HashMap::new();
    let mut bits_per_sample = 16u32;
    let mut cover_art_stream_idx: Option<usize> = None;
    let mut has_media = false;
    let mut is_lossless = false;
    let mut bit_rate_bps = 0u64;
    let mut audio_tracks = Vec::new();

    if let Some(d) = json["format"]["duration"].as_str() {
        duration = d.parse().unwrap_or(0.0);
    }
    if let Some(br) = json["format"]["bit_rate"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
    {
        bit_rate_bps = br;
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
                    let stream_index = stream["index"]
                        .as_u64()
                        .map(|v| v as usize)
                        .unwrap_or(stream_idx);
                    let language = stream["tags"]["language"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned);
                    let handler_name = stream["tags"]["handler_name"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned);
                    audio_tracks.push(AudioTrack {
                        stream_index,
                        language,
                        handler_name,
                    });
                    let codec = stream["codec_name"].as_str().unwrap_or("");
                    is_lossless = matches!(
                        codec,
                        "pcm_s16le"
                            | "pcm_s24le"
                            | "pcm_s32le"
                            | "pcm_s16be"
                            | "pcm_s24be"
                            | "pcm_s32be"
                            | "pcm_f32le"
                            | "pcm_f64le"
                            | "flac"
                            | "alac"
                    );
                    if let Some(stream_tags) = stream["tags"].as_object() {
                        for (k, v) in stream_tags {
                            if let Some(s) = v.as_str() {
                                tags.entry(k.to_lowercase())
                                    .or_insert_with(|| s.to_string());
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
                "video"
                    if stream["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 1
                        && cover_art_stream_idx.is_none() =>
                {
                    let codec = stream["codec_name"].as_str().unwrap_or("");
                    if matches!(codec, "mjpeg" | "png") {
                        cover_art_stream_idx = Some(stream_idx);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(FileInfo {
        duration_secs: duration,
        tags,
        bits_per_sample,
        cover_art_stream_idx,
        has_media,
        is_lossless,
        bit_rate_bps,
        audio_tracks,
    })
}

#[cfg(test)]
mod tests {
    use super::non_media_file_info;

    #[test]
    fn unrecognised_input_is_not_media() {
        let info = non_media_file_info();

        assert!(!info.has_media);
        assert_eq!(info.duration_secs, 0.0);
        assert!(info.tags.is_empty());
    }
}
