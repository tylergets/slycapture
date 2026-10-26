use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What is being recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Screen,
    Window,
    Region,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// Matroska survives an unclean stop; the safe default.
    #[default]
    Mkv,
    Mp4,
}

impl Format {
    pub const ALL: [Format; 2] = [Format::Mkv, Format::Mp4];

    pub fn extension(self) -> &'static str {
        match self {
            Format::Mkv => "mkv",
            Format::Mp4 => "mp4",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Low,
    Medium,
    #[default]
    High,
    Lossless,
}

impl Quality {
    pub const ALL: [Quality; 4] = [
        Quality::Low,
        Quality::Medium,
        Quality::High,
        Quality::Lossless,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Low => "Low (small files)",
            Quality::Medium => "Medium",
            Quality::High => "High",
            Quality::Lossless => "Lossless (huge files)",
        }
    }

    /// x264 constant rate factor.
    pub fn crf(self) -> u8 {
        match self {
            Quality::Low => 28,
            Quality::Medium => 23,
            Quality::High => 18,
            Quality::Lossless => 0,
        }
    }
}

/// Everything chosen in the setup dialog for one recording. Saved between runs by
/// [`crate::core::saved_options`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingOptions {
    /// File name prefix; a timestamp and extension are appended.
    pub base_name: String,
    pub framerate: u32,
    pub draw_mouse: bool,
    pub format: Format,
    pub quality: Quality,
    /// Seconds to wait before recording starts.
    pub countdown: u32,
    /// PulseAudio/PipeWire source to record as the microphone, if enabled.
    pub microphone: Option<String>,
    /// Denoise and level the microphone while recording.
    pub mic_cleanup: bool,
    /// Record what is playing through the default output.
    pub system_audio: bool,
    /// Re-encode the capture with a slow preset at `quality` after recording.
    /// When off, the capture is only remuxed into `format`.
    pub compress: bool,
    /// Keep the crash-safe `.capture.mkv` after post-processing succeeds.
    pub keep_capture: bool,
    /// Window size for window recordings.
    pub window_size: Option<(u32, u32)>,
    /// Prepend a title card when post-processing. Forces a re-encode.
    pub intro: bool,
    pub intro_title: String,
    /// Image shown on the title card, above the title.
    pub intro_logo: Option<PathBuf>,
    /// Zoom in on clicks and follow the pointer, applied when post-processing.
    pub zoom: bool,
}

impl RecordingOptions {
    pub fn has_audio(&self) -> bool {
        self.microphone.is_some() || self.system_audio
    }
}

/// Make `base_name` safe to use as a file name prefix.
pub fn sanitize_base_name(base_name: &str) -> String {
    let cleaned: String = base_name
        .trim()
        .chars()
        .map(|c| {
            if c == '/' || c == '\0' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.');
    if cleaned.is_empty() {
        "slycapture".to_string()
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_base_name;

    #[test]
    fn sanitizes_base_name() {
        assert_eq!(sanitize_base_name("  demo  "), "demo");
        assert_eq!(sanitize_base_name("a/b"), "a_b");
        assert_eq!(sanitize_base_name("../x"), "_x");
        assert_eq!(sanitize_base_name(""), "slycapture");
    }
}
