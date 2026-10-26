//! Remembers the last setup-dialog choices across restarts, in
//! `$XDG_STATE_HOME/slycapture/last-options.json`.
//!
//! Saved values are laid over the config defaults key by key, so options added in later
//! versions (missing from an older file) keep their config default.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::capture::RecordingOptions;

const FILE_NAME: &str = "last-options.json";

fn path() -> Option<PathBuf> {
    dirs::state_dir().map(|dir| dir.join("slycapture").join(FILE_NAME))
}

/// `defaults` with the saved options applied, or `defaults` if nothing usable was saved.
pub fn load(defaults: RecordingOptions) -> RecordingOptions {
    let Some(raw) = path().and_then(|path| fs::read_to_string(path).ok()) else {
        return defaults;
    };
    match serde_json::from_str(&raw) {
        Ok(saved) => merge(defaults, saved),
        Err(err) => {
            eprintln!("warning: ignoring unreadable {FILE_NAME}: {err}");
            defaults
        }
    }
}

pub fn save(options: &RecordingOptions) -> Result<()> {
    let path = path().context("no state directory for saving options")?;
    let dir = path.parent().context("invalid state path")?;
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    // Write then rename, so a crash never leaves a truncated file.
    let partial = path.with_extension("json.part");
    fs::write(&partial, serde_json::to_vec_pretty(options)?)
        .with_context(|| format!("failed to write {}", partial.display()))?;
    fs::rename(&partial, &path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn merge(defaults: RecordingOptions, saved: Value) -> RecordingOptions {
    let (Ok(Value::Object(mut merged)), Value::Object(saved)) =
        (serde_json::to_value(&defaults), saved)
    else {
        return defaults;
    };
    for (key, value) in saved {
        if merged.contains_key(&key) {
            merged.insert(key, value);
        }
    }
    serde_json::from_value(Value::Object(merged)).unwrap_or(defaults)
}

#[cfg(test)]
mod tests {
    use super::merge;
    use crate::capture::options::Format;
    use crate::config::AppConfig;
    use serde_json::json;

    #[test]
    fn saved_values_override_defaults() {
        let defaults = AppConfig::default().default_options();
        let merged = merge(
            defaults.clone(),
            json!({"format": "mp4", "framerate": 60, "microphone": "usb-mic", "unknown": 1}),
        );
        assert_eq!(merged.format, Format::Mp4);
        assert_eq!(merged.framerate, 60);
        assert_eq!(merged.microphone.as_deref(), Some("usb-mic"));
        // Keys missing from the file keep their defaults.
        assert_eq!(merged.base_name, defaults.base_name);
    }

    #[test]
    fn invalid_values_fall_back_to_defaults() {
        let defaults = AppConfig::default().default_options();
        assert_eq!(merge(defaults.clone(), json!({"format": "avi"})), defaults);
        assert_eq!(merge(defaults.clone(), json!([1, 2])), defaults);
    }

    #[test]
    fn round_trips() {
        let mut options = AppConfig::default().default_options();
        options.window_size = Some((1280, 720));
        options.mic_cleanup = true;
        let saved = serde_json::to_value(&options).unwrap();
        assert_eq!(
            merge(AppConfig::default().default_options(), saved),
            options
        );
    }
}
