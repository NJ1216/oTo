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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub output_dest: OutputDest,
    pub source_file_action: SourceFileAction,
    pub name_conflict: NameConflict,
    pub mp3_bitrate: u32,
    pub m4a_bitrate: u32,
    pub flac_compression: u8,
    pub parallel_count: usize,
    pub open_in_finder: bool,
    pub last_mode: String,
    pub last_format: String,
    pub custom_output_path: Option<String>,
    #[serde(default)]
    pub language: String,
}

impl Default for Settings {
    fn default() -> Self {
        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            output_dest: OutputDest::SourceFolder,
            source_file_action: SourceFileAction::Keep,
            name_conflict: NameConflict::AutoRename,
            mp3_bitrate: 192,
            m4a_bitrate: 128,
            flac_compression: 5,
            parallel_count: (cpu_count / 2).max(1),
            open_in_finder: false,
            last_mode: "encode".into(),
            last_format: "mp3".into(),
            custom_output_path: None,
            language: String::new(),
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
    let settings: Settings = serde_json::from_str(&data).unwrap_or_default();
    Ok(settings)
}

pub fn save_settings(app: &AppHandle, settings: &Settings) -> Result<()> {
    let path = settings_path(app)?;
    let data = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, data)?;
    Ok(())
}
