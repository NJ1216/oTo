use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OutputDest {
    SourceFolder,
    Desktop,
    Downloads,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SourceFileAction {
    Keep,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NameConflict {
    ConfirmDialog,
    AutoRename,
    ForceOverwrite,
}

fn default_enabled_formats() -> Vec<String> {
    vec!["mp3".into(), "aac".into(), "flac".into()]
}

fn default_enabled_decode_formats() -> Vec<String> {
    vec!["wav".into(), "aiff".into()]
}

fn default_mp3_channel_mode() -> String {
    "joint_stereo".into()
}
fn default_last_decode_format() -> String {
    "wav".into()
}

fn default_mp3_preset() -> String {
    "192".into()
}
fn default_aac_preset() -> String {
    "128".into()
}
fn default_opus_preset() -> String {
    "128".into()
}
fn default_opus_bitrate() -> u32 {
    128
}
fn default_opus_complexity() -> u32 {
    5
}
fn default_flac_preset() -> String {
    "5".into()
}
fn default_alac_bit_depth() -> u32 {
    16
}

fn default_mp3_mode() -> String {
    "cbr".into()
}
fn default_mp3_bitrate() -> u32 {
    192
}
fn default_mp3_vbr_quality() -> u32 {
    4
}
fn default_aac_mode() -> String {
    "cbr".into()
}
fn default_m4a_bitrate() -> u32 {
    128
}
fn default_aac_vbr_quality() -> u32 {
    4
}
fn default_opus_mode() -> String {
    "vbr".into()
}
fn default_flac_compression() -> u8 {
    5
}
fn default_last_mode() -> String {
    "encode".into()
}
fn default_last_format() -> String {
    "mp3".into()
}
fn default_silence_trim_db() -> f64 {
    -80.0
}
fn default_silence_trim_duration_ms() -> u32 {
    50
}
fn default_clear_log_on_convert() -> bool {
    true
}
fn default_auto_open_activity() -> bool {
    false
}

fn calc_parallel_count() -> usize {
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cpu_count - 1).max(1)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub output_dest: OutputDest,
    pub source_file_action: SourceFileAction,
    pub name_conflict: NameConflict,

    // MP3
    #[serde(default = "default_mp3_preset")]
    pub mp3_preset: String,
    #[serde(default = "default_mp3_mode")]
    pub mp3_mode: String,
    #[serde(default = "default_mp3_bitrate")]
    pub mp3_bitrate: u32,
    #[serde(default = "default_mp3_vbr_quality")]
    pub mp3_vbr_quality: u32,
    #[serde(default)]
    pub mp3_sample_rate: u32,
    #[serde(default = "default_mp3_channel_mode")]
    pub mp3_channel_mode: String,

    // AAC
    #[serde(default = "default_aac_preset")]
    pub aac_preset: String,
    #[serde(default = "default_aac_mode")]
    pub aac_mode: String,
    #[serde(default = "default_m4a_bitrate")]
    pub m4a_bitrate: u32,
    #[serde(default = "default_aac_vbr_quality")]
    pub aac_vbr_quality: u32,
    #[serde(default)]
    pub aac_sample_rate: u32,
    #[serde(default)]
    pub aac_channels: u32,

    // OPUS
    #[serde(default = "default_opus_preset")]
    pub opus_preset: String,
    #[serde(default = "default_opus_mode")]
    pub opus_mode: String,
    #[serde(default = "default_opus_bitrate")]
    pub opus_bitrate: u32,
    #[serde(default = "default_opus_complexity")]
    pub opus_complexity: u32,

    // FLAC
    #[serde(default = "default_flac_preset")]
    pub flac_preset: String,
    #[serde(default = "default_flac_compression")]
    pub flac_compression: u8,

    // ALAC
    #[serde(default)]
    pub alac_preset: String,
    #[serde(default = "default_alac_bit_depth")]
    pub alac_bit_depth: u32,

    /// 起動時に動的計算される。settings.json には保存しない。
    #[serde(skip)]
    pub parallel_count: usize,
    #[serde(default)]
    pub open_in_finder: bool,
    #[serde(default = "default_last_mode")]
    pub last_mode: String,
    #[serde(default = "default_last_format")]
    pub last_format: String,
    #[serde(default = "default_last_decode_format")]
    pub last_decode_format: String,
    pub custom_output_path: Option<String>,
    #[serde(default)]
    pub preserve_folder_structure: bool,
    #[serde(default)]
    pub language: String,
    #[serde(default = "default_enabled_formats")]
    pub enabled_formats: Vec<String>,

    #[serde(default = "default_enabled_decode_formats")]
    pub enabled_decode_formats: Vec<String>,
    // Silence trim
    #[serde(default)]
    pub silence_trim_enabled: bool,
    #[serde(default = "default_silence_trim_db")]
    pub silence_trim_db: f64,
    #[serde(default = "default_silence_trim_duration_ms")]
    pub silence_trim_duration_ms: u32,

    #[serde(default = "default_clear_log_on_convert")]
    pub clear_log_on_convert: bool,
    #[serde(default = "default_auto_open_activity")]
    pub auto_open_activity: bool,
    /// 入力内の全音声ストリームを、それぞれ別ファイルとして出力する。
    #[serde(default)]
    pub export_all_audio_tracks: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output_dest: OutputDest::SourceFolder,
            source_file_action: SourceFileAction::Keep,
            name_conflict: NameConflict::AutoRename,
            mp3_preset: default_mp3_preset(),
            mp3_mode: default_mp3_mode(),
            mp3_bitrate: default_mp3_bitrate(),
            mp3_vbr_quality: default_mp3_vbr_quality(),
            mp3_sample_rate: 0,
            mp3_channel_mode: default_mp3_channel_mode(),
            aac_preset: default_aac_preset(),
            aac_mode: default_aac_mode(),
            m4a_bitrate: default_m4a_bitrate(),
            aac_vbr_quality: default_aac_vbr_quality(),
            aac_sample_rate: 0,
            aac_channels: 0,
            opus_preset: default_opus_preset(),
            opus_mode: default_opus_mode(),
            opus_bitrate: default_opus_bitrate(),
            opus_complexity: default_opus_complexity(),
            flac_preset: default_flac_preset(),
            flac_compression: default_flac_compression(),
            alac_preset: String::new(),
            alac_bit_depth: default_alac_bit_depth(),
            parallel_count: calc_parallel_count(),
            open_in_finder: false,
            last_mode: default_last_mode(),
            last_format: default_last_format(),
            last_decode_format: default_last_decode_format(),
            custom_output_path: None,
            preserve_folder_structure: false,
            language: String::new(),
            enabled_formats: default_enabled_formats(),
            enabled_decode_formats: default_enabled_decode_formats(),
            silence_trim_enabled: false,
            silence_trim_db: default_silence_trim_db(),
            silence_trim_duration_ms: default_silence_trim_duration_ms(),
            clear_log_on_convert: default_clear_log_on_convert(),
            auto_open_activity: default_auto_open_activity(),
            export_all_audio_tracks: false,
        }
    }
}

impl Settings {
    pub fn refresh_runtime_values(&mut self) {
        self.parallel_count = calc_parallel_count();
    }

    /// Validate and clamp all numeric/range fields to safe values.
    /// Called after loading or before saving to prevent malformed settings
    /// from reaching FFmpeg.
    pub fn validate(&mut self) {
        // MP3
        self.mp3_bitrate = self.mp3_bitrate.clamp(32, 320);
        self.mp3_vbr_quality = self.mp3_vbr_quality.clamp(0, 9);
        self.mp3_sample_rate = match self.mp3_sample_rate {
            0 | 44100 | 48000 | 88200 | 96000 => self.mp3_sample_rate,
            _ => 0,
        };
        if !matches!(self.mp3_mode.as_str(), "cbr" | "vbr") {
            self.mp3_mode = "cbr".into();
        }
        if !matches!(
            self.mp3_channel_mode.as_str(),
            "auto" | "joint_stereo" | "stereo" | "mono"
        ) {
            self.mp3_channel_mode = "joint_stereo".into();
        }
        if let Ok(v) = self.mp3_preset.parse::<u32>() {
            if !(128..=320).contains(&v) && v != 0 {
                self.mp3_preset = "192".into();
            }
        } else if self.mp3_preset != "custom" {
            self.mp3_preset = "192".into();
        }

        // AAC
        self.m4a_bitrate = self.m4a_bitrate.clamp(32, 320);
        self.aac_vbr_quality = self.aac_vbr_quality.clamp(1, 5);
        self.aac_sample_rate = match self.aac_sample_rate {
            0 | 44100 | 48000 => self.aac_sample_rate,
            _ => 0,
        };
        self.aac_channels = self.aac_channels.clamp(0, 2);
        if !matches!(self.aac_mode.as_str(), "cbr" | "vbr") {
            self.aac_mode = "cbr".into();
        }
        if let Ok(v) = self.aac_preset.parse::<u32>() {
            if !(64..=256).contains(&v) && v != 0 {
                self.aac_preset = "128".into();
            }
        } else if self.aac_preset != "custom" {
            self.aac_preset = "128".into();
        }

        // OPUS
        self.opus_bitrate = self.opus_bitrate.clamp(16, 320);
        self.opus_complexity = self.opus_complexity.clamp(0, 10);
        if !matches!(self.opus_mode.as_str(), "cbr" | "vbr") {
            self.opus_mode = "vbr".into();
        }
        if let Ok(v) = self.opus_preset.parse::<u32>() {
            if !(16..=320).contains(&v) && v != 0 {
                self.opus_preset = "128".into();
            }
        } else if self.opus_preset != "custom" {
            self.opus_preset = "128".into();
        }

        // FLAC
        self.flac_compression = self.flac_compression.clamp(0, 8);
        if let Ok(v) = self.flac_preset.parse::<u32>() {
            if v > 8 {
                self.flac_preset = "5".into();
            }
        } else if self.flac_preset != "custom" {
            self.flac_preset = "5".into();
        }

        // ALAC
        self.alac_bit_depth = match self.alac_bit_depth {
            16 | 24 => self.alac_bit_depth,
            _ => 16,
        };

        // Silence trim
        self.silence_trim_db = self.silence_trim_db.clamp(-100.0, 0.0);
        self.silence_trim_duration_ms = self.silence_trim_duration_ms.clamp(1, 10000);

        // Enabled formats: ensure at least one
        if self.enabled_formats.is_empty() {
            self.enabled_formats = vec!["mp3".into()];
        }
        if self.enabled_decode_formats.is_empty() {
            self.enabled_decode_formats = vec!["wav".into()];
        }

        // Last mode / format
        if !matches!(self.last_mode.as_str(), "encode" | "decode") {
            self.last_mode = "encode".into();
        }
    }
}

fn settings_path(app: &AppHandle) -> Result<PathBuf> {
    let config_dir = app.path().app_config_dir()?;
    std::fs::create_dir_all(&config_dir)?;
    Ok(config_dir.join("settings.json"))
}

pub fn load_settings(app: &AppHandle) -> Result<Settings> {
    let path = settings_path(app)?;
    if !path.exists() {
        let mut s = Settings::default();
        s.validate();
        return Ok(s);
    }
    let data = std::fs::read_to_string(&path)?;
    let mut settings: Settings = serde_json::from_str(&data).unwrap_or_else(|e| {
        eprintln!("Failed to parse settings.json: {e}");
        Settings::default()
    });
    settings.validate();
    settings.refresh_runtime_values();
    Ok(settings)
}

pub fn save_settings(app: &AppHandle, settings: &Settings) -> Result<()> {
    let path = settings_path(app)?;
    let mut validated = settings.clone();
    validated.validate();
    let data = serde_json::to_string_pretty(&validated)?;
    std::fs::write(&path, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_memory_limit_is_ignored_and_not_reserialized() {
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        value["maxMemoryMb"] = serde_json::json!(8192);
        let settings: Settings = serde_json::from_value(value).unwrap();
        let saved = serde_json::to_value(settings).unwrap();
        assert!(saved.get("maxMemoryMb").is_none());
    }
}
