use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvertRequest {
    pub paths: Vec<String>,
    pub mode: String,
    pub format: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPayload {
    pub job_id: String,
    pub percent: f64,
    pub current_file: String,
    pub file_index: usize,
    pub file_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileResult {
    pub input_path: String,
    pub output_path: String,
    pub success: bool,
    pub skipped: bool,
    pub error: Option<String>,
    /// Set only on success when trimming was requested but could not be applied.
    pub trim_not_applied: Option<TrimNotice>,
}

/// Why silence trimming was requested but the file was converted untrimmed.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TrimNotice {
    AllSilent,
    AnalysisFailed,
}

impl TrimNotice {
    /// The same camelCase value serde produces, for the plain-string log field.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AllSilent => "allSilent",
            Self::AnalysisFailed => "analysisFailed",
        }
    }
}

impl FileResult {
    pub fn error(input_path: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            input_path: input_path.into(),
            output_path: String::new(),
            success: false,
            skipped: false,
            error: Some(msg.into()),
            trim_not_applied: None,
        }
    }

    pub fn skipped(input_path: impl Into<String>) -> Self {
        Self {
            input_path: input_path.into(),
            output_path: String::new(),
            success: false,
            skipped: true,
            error: None,
            trim_not_applied: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionPayload {
    pub job_id: String,
    pub results: Vec<FileResult>,
    pub success_count: usize,
    pub error_count: usize,
    pub skipped_count: usize,
    /// A session can contain batches captured with different conversion profiles.
    /// The frontend only renders a quality label when this remains false.
    pub mixed_profiles: bool,
}

#[derive(Clone)]
pub struct FileInfo {
    pub duration_secs: f64,
    pub tags: HashMap<String, String>,
    pub bits_per_sample: u32,
    pub cover_art_stream_idx: Option<usize>,
    pub has_media: bool,
    pub is_lossless: bool,
    pub bit_rate_bps: u64,
    /// Largest audio stream's PCM rate at the supported maximum depth (32 bits).
    /// Used for disk reservations; compressed/container bitrate is not a bound.
    pub max_pcm_bit_rate_bps: u64,
    pub audio_tracks: Vec<AudioTrack>,
}

/// ffprobe が返す入力音声ストリームの、個別出力に必要な情報。
#[derive(Debug, Clone)]
pub struct AudioTrack {
    /// 入力全体における FFmpeg stream index (`-map 0:<index>` 用)。
    pub stream_index: usize,
    pub language: Option<String>,
    pub handler_name: Option<String>,
}

#[derive(Debug)]
pub enum OverwriteChoice {
    Overwrite,
    Rename,
    Skip,
    CancelAll,
}
