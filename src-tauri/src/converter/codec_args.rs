use crate::settings::Settings;
use super::types::FileInfo;

fn build_mp3_args(settings: &Settings) -> Vec<String> {
    let mut args = vec!["-c:a".into(), "libmp3lame".into()];
    if settings.mp3_preset == "custom" {
        if settings.mp3_mode == "vbr" {
            args.extend(["-q:a".into(), settings.mp3_vbr_quality.to_string()]);
        } else {
            args.extend(["-b:a".into(), format!("{}k", settings.mp3_bitrate)]);
        }
        if settings.mp3_sample_rate > 0 {
            args.extend(["-ar".into(), settings.mp3_sample_rate.to_string()]);
        }
        match settings.mp3_channel_mode.as_str() {
            "mono"   => args.extend(["-ac".into(), "1".into()]),
            "stereo" => args.extend(["-ac".into(), "2".into()]),
            _ => {}
        }
    } else {
        let bitrate: u32 = settings.mp3_preset.parse().unwrap_or(192);
        args.extend(["-b:a".into(), format!("{}k", bitrate)]);
    }
    args
}

fn build_aac_args(settings: &Settings) -> Vec<String> {
    let mut args = vec!["-c:a".into(), "aac".into()];
    if settings.aac_preset == "custom" {
        if settings.aac_mode == "vbr" {
            args.extend(["-vbr".into(), settings.aac_vbr_quality.to_string()]);
        } else {
            args.extend(["-b:a".into(), format!("{}k", settings.m4a_bitrate)]);
        }
        if settings.aac_sample_rate > 0 {
            args.extend(["-ar".into(), settings.aac_sample_rate.to_string()]);
        }
        match settings.aac_channels {
            1 => args.extend(["-ac".into(), "1".into()]),
            2 => args.extend(["-ac".into(), "2".into()]),
            _ => {}
        }
    } else {
        let bitrate: u32 = settings.aac_preset.parse().unwrap_or(128);
        args.extend(["-b:a".into(), format!("{}k", bitrate)]);
    }
    args
}

fn build_opus_args(settings: &Settings) -> Vec<String> {
    let mut args = vec!["-c:a".into(), "libopus".into()];
    if settings.opus_preset == "custom" {
        if settings.opus_mode == "cbr" {
            args.extend(["-vbr".into(), "off".into()]);
        }
        args.extend(["-b:a".into(), format!("{}k", settings.opus_bitrate)]);
        args.extend(["-compression_level".into(), settings.opus_complexity.to_string()]);
    } else {
        let bitrate: u32 = settings.opus_preset.parse().unwrap_or(128);
        args.extend(["-b:a".into(), format!("{}k", bitrate)]);
    }
    args
}

fn build_flac_args(settings: &Settings) -> Vec<String> {
    let level: u8 = if settings.flac_preset == "custom" {
        settings.flac_compression
    } else {
        settings.flac_preset.parse().unwrap_or(5)
    };
    vec!["-c:a".into(), "flac".into(), "-compression_level".into(), level.to_string()]
}

fn build_alac_args(settings: &Settings) -> Vec<String> {
    let mut args = vec!["-c:a".into(), "alac".into()];
    if settings.alac_preset == "custom" && settings.alac_bit_depth == 24 {
        args.extend(["-sample_fmt".into(), "s32p".into()]);
    }
    args
}

fn build_wav_args(info: &FileInfo) -> Vec<String> {
    let pcm_codec = match info.bits_per_sample {
        24 => "pcm_s24le",
        32 => "pcm_s32le",
        _ => "pcm_s16le",
    };
    vec!["-c:a".into(), pcm_codec.into()]
}

fn build_aiff_args(info: &FileInfo) -> Vec<String> {
    let pcm_codec = match info.bits_per_sample {
        24 => "pcm_s24be",
        32 => "pcm_s32be",
        _ => "pcm_s16be",
    };
    vec!["-c:a".into(), pcm_codec.into()]
}

pub fn build_codec_args(format: &str, settings: &Settings, info: &FileInfo) -> Vec<String> {
    match format {
        "mp3"  => build_mp3_args(settings),
        "aac"  => build_aac_args(settings),
        "opus" => build_opus_args(settings),
        "flac" => build_flac_args(settings),
        "alac" => build_alac_args(settings),
        "wav"  => build_wav_args(info),
        "aiff" => build_aiff_args(info),
        _      => vec![],
    }
}
