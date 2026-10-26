//! Automatic cursor zoom, applied after recording.
//!
//! While recording, [`cursor_log::CursorLogger`] writes the pointer position and clicks next to
//! the capture. Afterwards, [`camera::CameraPlanner`] turns that into a smooth virtual camera
//! and [`render`] crops and scales every frame of the capture into a new file. The capture and
//! the log are never modified, so a zoom can always be re-rendered with different settings.

pub mod camera;
pub mod cursor_log;
pub mod render;

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::Deserialize;

/// The `zoom:` config section.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZoomSettings {
    /// Render a zoomed copy after each recording.
    pub enabled: bool,
    /// How far to zoom in on a click. Above ~1.5 text gets soft unless the screen is HiDPI.
    pub max_scale: f64,
    /// Seconds to stay zoomed in after a click.
    pub hold: f64,
    /// Seconds before a click to start zooming in, so the camera arrives on time.
    pub lead: f64,
    /// Clicks closer together than this (seconds) share one zoom instead of zooming out and in.
    pub merge_gap: f64,
    /// Fraction of the zoomed view the pointer can move in before the camera follows it.
    pub dead_zone: f64,
    /// Draw a ripple where each click happened.
    pub click_effect: bool,
    /// Ignore clicks this many seconds before the end (stopping the recording from the tray).
    pub ignore_last: f64,
    /// Seconds added to video time to find the matching cursor time. Increase it if zooms
    /// start too late.
    pub sync_offset: f64,
}

impl Default for ZoomSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            max_scale: 1.8,
            hold: 1.5,
            lead: 0.35,
            merge_gap: 1.0,
            dead_zone: 0.5,
            click_effect: true,
            ignore_last: 2.0,
            sync_offset: 0.0,
        }
    }
}

impl ZoomSettings {
    pub fn validate(&self) -> Result<()> {
        if !(1.0..=4.0).contains(&self.max_scale) {
            bail!("zoom.max_scale must be between 1.0 and 4.0");
        }
        if !(0.0..=1.0).contains(&self.dead_zone) {
            bail!("zoom.dead_zone must be between 0.0 and 1.0");
        }
        for (name, value) in [
            ("hold", self.hold),
            ("lead", self.lead),
            ("merge_gap", self.merge_gap),
            ("ignore_last", self.ignore_last),
        ] {
            if !value.is_finite() || value < 0.0 {
                bail!("zoom.{name} must be zero or more");
            }
        }
        if !self.sync_offset.is_finite() {
            bail!("zoom.sync_offset must be a number");
        }
        Ok(())
    }
}

/// `foo.mp4` -> `foo.zoom.mp4`
pub fn zoomed_path(output: &Path) -> PathBuf {
    let extension = output
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mkv".into());
    output.with_extension(format!("zoom.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::{zoomed_path, ZoomSettings};
    use std::path::Path;

    #[test]
    fn defaults_are_valid() {
        ZoomSettings::default().validate().expect("defaults should be valid");
    }

    #[test]
    fn rejects_zoom_out() {
        let settings = ZoomSettings {
            max_scale: 0.5,
            ..ZoomSettings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn names_zoomed_output() {
        assert_eq!(
            zoomed_path(Path::new("/v/demo-1.mp4")),
            Path::new("/v/demo-1.zoom.mp4")
        );
    }
}
