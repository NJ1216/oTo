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

fn default_mp3_channel_mode() -> String { "joint_stereo".into() }
fn default_last_decode_format() -> String { "wav".into() }

fn default_mp3_preset() -> String  { "192".into() }
fn default_aac_preset() -> String  { "128".into() }
fn default_opus_preset() -> String { "128".into() }
fn default_opus_bitrate() -> u32   { 128 }
fn default_opus_complexity() -> u32 { 5 }
fn default_flac_preset() -> String { "5".into() }
fn default_alac_bit_depth() -> u32 { 16 }

fn default_mp3_mode() -> String     { "cbr".into() }
fn default_mp3_bitrate() -> u32     { 192 }
fn default_mp3_vbr_quality() -> u32 { 4 }
fn default_aac_mode() -> String     { "cbr".into() }
fn default_m4a_bitrate() -> u32     { 128 }
fn default_aac_vbr_quality() -> u32 { 4 }
fn default_opus_mode() -> String    { "vbr".into() }

fn calc_parallel_count(full_power: bool) -> usize {
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    if full_power { cpu_count } else { (cpu_count - 1).max(1) }
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
    pub flac_compression: u8,

    // ALAC
    #[serde(default)]
    pub alac_preset: String,
    #[serde(default = "default_alac_bit_depth")]
    pub alac_bit_depth: u32,

    /// full_power から動的計算される。settings.json には保存しない。
    #[serde(skip)]
    pub parallel_count: usize,
    #[serde(default)]
    pub full_power: bool,
    pub open_in_finder: bool,
    pub last_mode: String,
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
            flac_compression: 5,
            alac_preset: String::new(),
            alac_bit_depth: default_alac_bit_depth(),
            parallel_count: calc_parallel_count(false),
            full_power: false,
            open_in_finder: false,
            last_mode: "encode".into(),
            last_format: "mp3".into(),
            last_decode_format: default_last_decode_format(),
            custom_output_path: None,
            preserve_folder_structure: false,
            language: String::new(),
            enabled_formats: default_enabled_formats(),
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
        return Ok(Settings::default());
    }
    let data = std::fs::read_to_string(&path)?;
    let mut settings: Settings = serde_json::from_str(&data).unwrap_or_else(|e| {
        eprintln!("Failed to parse settings.json: {e}");
        Settings::default()
    });
    settings.parallel_count = calc_parallel_count(settings.full_power);
    Ok(settings)
}

pub fn save_settings(app: &AppHandle, settings: &Settings) -> Result<()> {
    let path = settings_path(app)?;
    let data = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, data)?;
    Ok(())
}
