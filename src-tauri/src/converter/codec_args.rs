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
    // FFmpeg のビルトイン `aac` エンコーダは `-vbr` フラグが効かない
    // (libfdk_aac 専用)。設定上 VBR が選ばれていても CBR(ビットレート指定) に
    // フォールバックする。VBR Quality は m4a_bitrate にマップせず無視する。
    let mut args = vec!["-c:a".into(), "aac".into()];
    if settings.aac_preset == "custom" {
        args.extend(["-b:a".into(), format!("{}k", settings.m4a_bitrate)]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use std::collections::HashMap;

    fn default_info() -> super::super::types::FileInfo {
        super::super::types::FileInfo {
            duration_secs: 100.0,
            tags: HashMap::new(),
            bits_per_sample: 16,
            cover_art_stream_idx: None,
            has_media: true,
            is_lossless: false,
            bit_rate_bps: 192_000,
            format_name: String::new(),
        }
    }

    #[test]
    fn mp3_preset_192_produces_cbr_bitrate() {
        let s = Settings::default(); // mp3_preset = "192"
        let args = build_codec_args("mp3", &s, &default_info());
        assert_eq!(args[0], "-c:a");
        assert_eq!(args[1], "libmp3lame");
        assert!(args.contains(&"-b:a".to_string()));
        assert!(args.contains(&"192k".to_string()));
    }

    #[test]
    fn mp3_custom_vbr_uses_quality_flag() {
        let mut s = Settings::default();
        s.mp3_preset = "custom".into();
        s.mp3_mode = "vbr".into();
        s.mp3_vbr_quality = 2;
        let args = build_codec_args("mp3", &s, &default_info());
        assert!(args.contains(&"-q:a".to_string()));
        assert!(args.contains(&"2".to_string()));
        assert!(!args.contains(&"-b:a".to_string()));
    }

    #[test]
    fn mp3_custom_cbr_uses_bitrate_flag() {
        let mut s = Settings::default();
        s.mp3_preset = "custom".into();
        s.mp3_mode = "cbr".into();
        s.mp3_bitrate = 256;
        let args = build_codec_args("mp3", &s, &default_info());
        assert!(args.contains(&"-b:a".to_string()));
        assert!(args.contains(&"256k".to_string()));
    }

    #[test]
    fn aac_preset_128_produces_cbr_bitrate() {
        let s = Settings::default(); // aac_preset = "128"
        let args = build_codec_args("aac", &s, &default_info());
        assert_eq!(args[0], "-c:a");
        assert_eq!(args[1], "aac");
        assert!(args.contains(&"128k".to_string()));
    }

    #[test]
    fn aac_custom_vbr_falls_back_to_cbr() {
        // ビルトイン aac は -vbr 非対応なので、VBR 選択時も CBR にフォールバックする
        let mut s = Settings::default();
        s.aac_preset = "custom".into();
        s.aac_mode = "vbr".into();
        s.aac_vbr_quality = 3;
        s.m4a_bitrate = 160;
        let args = build_codec_args("aac", &s, &default_info());
        assert!(!args.contains(&"-vbr".to_string()));
        assert!(args.contains(&"-b:a".to_string()));
        assert!(args.contains(&"160k".to_string()));
    }

    #[test]
    fn opus_preset_produces_bitrate() {
        let s = Settings::default(); // opus_preset = "128"
        let args = build_codec_args("opus", &s, &default_info());
        assert_eq!(args[0], "-c:a");
        assert_eq!(args[1], "libopus");
        assert!(args.contains(&"128k".to_string()));
    }

    #[test]
    fn opus_custom_cbr_adds_vbr_off() {
        let mut s = Settings::default();
        s.opus_preset = "custom".into();
        s.opus_mode = "cbr".into();
        s.opus_bitrate = 96;
        let args = build_codec_args("opus", &s, &default_info());
        assert!(args.contains(&"-vbr".to_string()));
        assert!(args.contains(&"off".to_string()));
    }

    #[test]
    fn flac_default_compression_is_5() {
        let s = Settings::default(); // flac_preset = "5"
        let args = build_codec_args("flac", &s, &default_info());
        assert_eq!(args[0], "-c:a");
        assert_eq!(args[1], "flac");
        assert!(args.contains(&"-compression_level".to_string()));
        assert!(args.contains(&"5".to_string()));
    }

    #[test]
    fn alac_16bit_default_no_sample_fmt() {
        let s = Settings::default(); // alac_bit_depth = 16
        let args = build_codec_args("alac", &s, &default_info());
        assert_eq!(args[0], "-c:a");
        assert_eq!(args[1], "alac");
        assert!(!args.contains(&"-sample_fmt".to_string()));
    }

    #[test]
    fn alac_custom_24bit_adds_s32p() {
        let mut s = Settings::default();
        s.alac_preset = "custom".into();
        s.alac_bit_depth = 24;
        let args = build_codec_args("alac", &s, &default_info());
        assert!(args.contains(&"-sample_fmt".to_string()));
        assert!(args.contains(&"s32p".to_string()));
    }

    #[test]
    fn wav_16bit_uses_pcm_s16le() {
        let args = build_codec_args("wav", &Settings::default(), &default_info());
        assert_eq!(args[0], "-c:a");
        assert_eq!(args[1], "pcm_s16le");
    }

    #[test]
    fn wav_24bit_uses_pcm_s24le() {
        let mut info = default_info();
        info.bits_per_sample = 24;
        let args = build_codec_args("wav", &Settings::default(), &info);
        assert_eq!(args[1], "pcm_s24le");
    }

    #[test]
    fn wav_32bit_uses_pcm_s32le() {
        let mut info = default_info();
        info.bits_per_sample = 32;
        let args = build_codec_args("wav", &Settings::default(), &info);
        assert_eq!(args[1], "pcm_s32le");
    }

    #[test]
    fn aiff_16bit_uses_pcm_s16be() {
        let args = build_codec_args("aiff", &Settings::default(), &default_info());
        assert_eq!(args[0], "-c:a");
        assert_eq!(args[1], "pcm_s16be");
    }

    #[test]
    fn aiff_24bit_uses_pcm_s24be() {
        let mut info = default_info();
        info.bits_per_sample = 24;
        let args = build_codec_args("aiff", &Settings::default(), &info);
        assert_eq!(args[1], "pcm_s24be");
    }

    #[test]
    fn unknown_format_returns_empty() {
        let args = build_codec_args("ogg", &Settings::default(), &default_info());
        assert!(args.is_empty());
    }
}
