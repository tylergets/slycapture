use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};

/// Sample rate of the mono s16le stream used for metering.
pub const METER_SAMPLE_RATE: u32 = 16_000;
/// Samples per level update (50ms).
const METER_CHUNK_SAMPLES: usize = (METER_SAMPLE_RATE / 20) as usize;
/// Levels at or below this many dBFS read as silence.
const METER_FLOOR_DB: f32 = -60.0;

/// The most recent audio level, 0.0 (silent) to 1.0 (full scale), shared across threads.
#[derive(Debug, Clone, Default)]
pub struct Level(Arc<AtomicU32>);

impl Level {
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn set(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSource {
    pub name: String,
    pub description: String,
    /// Monitor sources capture an output device rather than an input.
    pub is_monitor: bool,
}

/// Input sources known to PulseAudio/PipeWire, via `pactl`.
pub fn list_sources() -> Vec<AudioSource> {
    let Ok(output) = Command::new("pactl")
        .args(["-f", "json", "list", "sources"])
        .output()
    else {
        return Vec::new();
    };
    let Ok(sources) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) else {
        return Vec::new();
    };
    sources
        .iter()
        .filter_map(|source| {
            let name = source.get("name")?.as_str()?.to_string();
            let description = source
                .get("description")
                .and_then(|value| value.as_str())
                .unwrap_or(&name)
                .to_string();
            Some(AudioSource {
                is_monitor: name.ends_with(".monitor"),
                name,
                description,
            })
        })
        .collect()
}

/// The monitor source of the default output, i.e. "what you hear".
pub fn default_monitor_source() -> String {
    Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|sink| sink.trim().to_string())
        .filter(|sink| !sink.is_empty())
        .map(|sink| format!("{sink}.monitor"))
        .unwrap_or_else(|| "@DEFAULT_MONITOR@".to_string())
}

/// Live level meter for a source, used by the setup dialog. Stops when dropped.
pub struct AudioMeter {
    child: Child,
    level: Level,
}

impl AudioMeter {
    pub fn start(ffmpeg: &str, source: &str) -> Result<Self> {
        let mut child = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "pulse",
                "-i",
                source,
            ])
            .args(meter_output_args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to launch {ffmpeg} for audio metering"))?;
        let stdout = child.stdout.take().context("ffmpeg stdout unavailable")?;
        Ok(Self {
            child,
            level: spawn_level_reader(stdout),
        })
    }

    pub fn level(&self) -> f32 {
        self.level.get()
    }
}

impl Drop for AudioMeter {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// ffmpeg output options for the raw mono stream consumed by [`spawn_level_reader`].
pub fn meter_output_args() -> Vec<String> {
    vec![
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        METER_SAMPLE_RATE.to_string(),
        "-f".into(),
        "s16le".into(),
        "pipe:1".into(),
    ]
}

/// Read s16le mono samples from `stdout` until EOF, publishing the peak level of each chunk.
pub fn spawn_level_reader(mut stdout: ChildStdout) -> Level {
    let level = Level::default();
    let publish = level.clone();
    thread::spawn(move || {
        let mut buffer = vec![0u8; METER_CHUNK_SAMPLES * 2];
        while stdout.read_exact(&mut buffer).is_ok() {
            publish.set(peak_level(&buffer));
        }
        publish.set(0.0);
    });
    level
}

/// Peak of a chunk of s16le samples on a log scale from 0.0 (<= -60 dBFS) to 1.0 (0 dBFS).
fn peak_level(samples: &[u8]) -> f32 {
    let peak = samples
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]).unsigned_abs())
        .max()
        .unwrap_or(0);
    if peak == 0 {
        return 0.0;
    }
    let db = 20.0 * (f32::from(peak) / f32::from(i16::MAX)).log10();
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::peak_level;

    fn samples(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn silence_is_zero() {
        assert_eq!(peak_level(&samples(&[0, 0, 0])), 0.0);
    }

    #[test]
    fn full_scale_is_one() {
        assert_eq!(peak_level(&samples(&[0, i16::MIN, 5])), 1.0);
    }

    #[test]
    fn half_scale_is_about_minus_six_db() {
        let level = peak_level(&samples(&[i16::MAX / 2]));
        assert!((level - 0.9).abs() < 0.01, "{level}");
    }
}
