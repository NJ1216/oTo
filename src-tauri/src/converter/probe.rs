use std::collections::HashMap;
use std::path::Path;
use super::binary::{ffprobe_path, friendly_ffmpeg_error};
use super::types::FileInfo;

pub async fn probe_file(path: &Path) -> Result<FileInfo, String> {
    let ffprobe = ffprobe_path();
    let mut probe_cmd = tokio::process::Command::new(&ffprobe);
    probe_cmd.args([
        "-v", "quiet",
        "-print_format", "json",
        "-show_format",
        "-show_streams",
    ]);
    probe_cmd.arg(path); // Pass as OsStr to support non-UTF-8 filenames
    #[cfg(windows)]
    probe_cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let output = match probe_cmd.output().await {
        Ok(o) => Some(o),
        Err(e) => {
            let msg = friendly_ffmpeg_error(&e.to_string());
            return Err(format!("{}: {}", path.display(), msg));
        }
    };

    let mut duration = 0.0f64;
    let mut tags = HashMap::new();
    let mut bits_per_sample = 16u32;
    let mut cover_art_stream_idx: Option<usize> = None;
    let mut has_media = false;
    let mut is_lossless = false;
    let mut bit_rate_bps = 0u64;

    if let Some(out) = output {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            if let Some(d) = json["format"]["duration"].as_str() {
                duration = d.parse().unwrap_or(0.0);
            }
            if let Some(br) = json["format"]["bit_rate"].as_str()
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
                            let codec = stream["codec_name"].as_str().unwrap_or("");
                            is_lossless = matches!(codec,
                                "pcm_s16le" | "pcm_s24le" | "pcm_s32le" |
                                "pcm_s16be" | "pcm_s24be" | "pcm_s32be" |
                                "pcm_f32le" | "pcm_f64le" | "flac" | "alac"
                            );
                            if let Some(stream_tags) = stream["tags"].as_object() {
                                for (k, v) in stream_tags {
                                    if let Some(s) = v.as_str() {
                                        tags.entry(k.to_lowercase()).or_insert_with(|| s.to_string());
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
    })
}
