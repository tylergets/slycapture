use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::capture::options::{Format, Quality, RecordingOptions};
use crate::zoom::ZoomSettings;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub recording: RecordingConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub intro: IntroConfig,
    #[serde(default)]
    pub zoom: ZoomSettings,
}

/// Title card prepended to recordings. `enabled`, `title` and `logo` are defaults for the
/// setup dialog.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntroConfig {
    pub enabled: bool,
    pub title: String,
    /// Image shown above the title. Supports a leading `~/`.
    pub logo: Option<PathBuf>,
    /// Length of the title card in seconds, including its fades.
    pub duration: f64,
    /// fontconfig pattern for the title.
    pub font: String,
}

impl Default for IntroConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            title: String::new(),
            logo: None,
            duration: 3.0,
            font: "Sans".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    /// Pause dunst while recording (restored afterwards) so popups stay out of the video.
    pub pause_dunst: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { pause_dunst: true }
    }
}

/// Recording defaults. Everything except `output_dir` and `ffmpeg` can be changed per
/// recording in the setup dialog.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingConfig {
    /// Directory recordings are written to. Supports a leading `~/`.
    pub output_dir: Option<PathBuf>,
    pub base_name: String,
    pub framerate: u32,
    pub draw_mouse: bool,
    pub format: Format,
    pub quality: Quality,
    pub countdown: u32,
    pub compress: bool,
    pub keep_capture: bool,
    pub ffmpeg: String,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            output_dir: None,
            base_name: "slycapture".to_string(),
            framerate: 30,
            draw_mouse: true,
            format: Format::default(),
            quality: Quality::default(),
            countdown: 0,
            compress: true,
            keep_capture: true,
            ffmpeg: "ffmpeg".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub microphone: bool,
    /// PulseAudio/PipeWire source name; `None` uses the default source.
    pub microphone_source: Option<String>,
    pub system_audio: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// i3 workspace a window is moved to when recording a single window.
    pub workspace: String,
    /// Put the window back (workspace, tiling/floating, border) when recording ends.
    pub restore: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            workspace: "slycapture".to_string(),
            restore: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// `None` when no config file was found and defaults are in use.
    pub path: Option<PathBuf>,
    pub config: AppConfig,
}

impl AppConfig {
    pub fn load(config_path_override: Option<PathBuf>) -> Result<LoadedConfig> {
        let path = match config_path_override {
            Some(path) => path,
            None => match discover_config_path()? {
                Some(path) => path,
                None => {
                    return Ok(LoadedConfig {
                        path: None,
                        config: AppConfig::default(),
                    })
                }
            },
        };

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        let config = Self::parse(&raw)
            .with_context(|| format!("failed to parse YAML config: {}", path.display()))?;

        Ok(LoadedConfig {
            path: Some(path),
            config,
        })
    }

    pub fn parse(raw: &str) -> Result<Self> {
        // An empty file is a valid "all defaults" config.
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(serde_yaml::from_str(raw)?)
    }

    pub fn validate(&self) -> Result<()> {
        self.recording.validate()?;
        self.window.validate()?;
        self.intro.validate()?;
        self.zoom.validate()?;
        Ok(())
    }
}

impl RecordingConfig {
    fn validate(&self) -> Result<()> {
        if !(1..=240).contains(&self.framerate) {
            bail!("recording.framerate must be between 1 and 240");
        }
        if self.ffmpeg.trim().is_empty() {
            bail!("recording.ffmpeg cannot be empty");
        }
        if self.countdown > 60 {
            bail!("recording.countdown must be at most 60 seconds");
        }
        Ok(())
    }

    pub fn resolved_output_dir(&self) -> Result<PathBuf> {
        let home = dirs::home_dir().context("unable to resolve home directory")?;
        Ok(match &self.output_dir {
            Some(dir) => expand_home(dir, &home),
            None => dirs::video_dir()
                .unwrap_or_else(|| home.join("Videos"))
                .join("slycapture"),
        })
    }
}

impl AppConfig {
    /// Initial setup-dialog choices.
    pub fn default_options(&self) -> RecordingOptions {
        RecordingOptions {
            base_name: self.recording.base_name.clone(),
            framerate: self.recording.framerate,
            draw_mouse: self.recording.draw_mouse,
            format: self.recording.format,
            quality: self.recording.quality,
            countdown: self.recording.countdown,
            microphone: self.audio.microphone.then(|| {
                self.audio
                    .microphone_source
                    .clone()
                    .unwrap_or_else(|| "default".to_string())
            }),
            system_audio: self.audio.system_audio,
            compress: self.recording.compress,
            keep_capture: self.recording.keep_capture,
            window_size: None,
            intro: self.intro.enabled,
            intro_title: self.intro.title.clone(),
            intro_logo: self.intro.logo.as_ref().map(|logo| {
                dirs::home_dir()
                    .map(|home| expand_home(logo, &home))
                    .unwrap_or_else(|| logo.clone())
            }),
            zoom: self.zoom.enabled,
        }
    }
}

impl IntroConfig {
    fn validate(&self) -> Result<()> {
        if !(1.0..=10.0).contains(&self.duration) {
            bail!("intro.duration must be between 1 and 10 seconds");
        }
        if self.font.trim().is_empty() {
            bail!("intro.font cannot be empty");
        }
        Ok(())
    }
}

impl WindowConfig {
    fn validate(&self) -> Result<()> {
        if self.workspace.trim().is_empty() {
            bail!("window.workspace cannot be empty");
        }
        Ok(())
    }
}

fn expand_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

fn discover_config_path() -> Result<Option<PathBuf>> {
    let cwd_file = std::env::current_dir()?.join("slycapture.yaml");
    if cwd_file.exists() {
        return Ok(Some(cwd_file));
    }

    if let Some(config_dir) = dirs::config_dir() {
        let home_config = config_dir.join("slycapture").join("config.yaml");
        if home_config.exists() {
            return Ok(Some(home_config));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{expand_home, AppConfig};
    use crate::capture::options::{Format, Quality};
    use std::path::Path;

    #[test]
    fn validate_accepts_empty_config() {
        let cfg = AppConfig::parse("").expect("empty config should parse");
        cfg.validate().expect("empty config should be valid");
        assert_eq!(cfg.recording.framerate, 30);
        assert_eq!(cfg.window.workspace, "slycapture");
    }

    #[test]
    fn parses_full_config() {
        let cfg = AppConfig::parse(
            "recording:\n  output_dir: ~/clips\n  framerate: 60\n  draw_mouse: false\n  format: mp4\n  quality: medium\naudio:\n  microphone: true\nwindow:\n  workspace: rec\n",
        )
        .expect("config should parse");
        cfg.validate().expect("config should be valid");
        assert_eq!(cfg.recording.framerate, 60);
        assert!(!cfg.recording.draw_mouse);
        assert_eq!(cfg.window.workspace, "rec");
        let options = cfg.default_options();
        assert_eq!(options.format, Format::Mp4);
        assert_eq!(options.quality, Quality::Medium);
        assert_eq!(options.microphone.as_deref(), Some("default"));
    }

    #[test]
    fn parses_intro() {
        let cfg = AppConfig::parse(
            "intro:\n  enabled: true\n  title: Demo\n  logo: /logo.png\n  duration: 4\n",
        )
        .expect("config should parse");
        cfg.validate().expect("config should be valid");
        assert_eq!(cfg.intro.duration, 4.0);
        let options = cfg.default_options();
        assert!(options.intro);
        assert_eq!(options.intro_title, "Demo");
        assert_eq!(options.intro_logo.as_deref(), Some(Path::new("/logo.png")));
    }

    #[test]
    fn rejects_long_intro() {
        let cfg = AppConfig::parse("intro:\n  duration: 30\n").expect("config should parse");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(AppConfig::parse("clipboard:\n  backend: gtk\n").is_err());
    }

    #[test]
    fn rejects_zero_framerate() {
        let cfg = AppConfig::parse("recording:\n  framerate: 0\n").expect("config should parse");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn expands_leading_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(expand_home(Path::new("~/clips"), home), home.join("clips"));
        assert_eq!(expand_home(Path::new("/abs"), home), Path::new("/abs"));
    }
}
