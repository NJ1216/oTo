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
    vec!["mp3".into(), "m4a".into(), "flac".into()]
}

fn default_mp3_channel_mode() -> String {
    "joint_stereo".into()
}

fn default_last_decode_format() -> String {
    "wav".into()
}

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
    pub mp3_bitrate: u32,
    #[serde(default)]
    pub mp3_sample_rate: u32,
    #[serde(default = "default_mp3_channel_mode")]
    pub mp3_channel_mode: String,
    pub m4a_bitrate: u32,
    pub flac_compression: u8,
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
            mp3_bitrate: 192,
            mp3_sample_rate: 0,
            mp3_channel_mode: default_mp3_channel_mode(),
            m4a_bitrate: 128,
            flac_compression: 5,
            parallel_count: calc_parallel_count(false),
            full_power: false,
            open_in_finder: false,
            last_mode: "encode".into(),
            last_format: "mp3".into(),
            last_decode_format: default_last_decode_format(),
            custom_output_path: None,
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
    let mut settings: Settings = serde_json::from_str(&data).unwrap_or_default();
    settings.parallel_count = calc_parallel_count(settings.full_power);
    Ok(settings)
}

pub fn save_settings(app: &AppHandle, settings: &Settings) -> Result<()> {
    let path = settings_path(app)?;
    let data = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, data)?;
    Ok(())
}
