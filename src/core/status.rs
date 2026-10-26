use serde::{Deserialize, Serialize};

use crate::capture::Target;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Daemon not reachable (only produced by clients).
    Offline,
    #[default]
    Idle,
    /// Picking a window or region, or in the setup dialog.
    Preparing,
    Countdown,
    Recording,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobStatus {
    pub name: String,
    pub progress: f32,
}

/// Snapshot of the daemon, served over the control socket.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Status {
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default)]
    pub elapsed_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub countdown: Option<u32>,
    /// Microphone peak level, 0.0..=1.0, while recording with a microphone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mic_level: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Post-processing jobs still running.
    #[serde(default)]
    pub processing: Vec<JobStatus>,
}

impl Status {
    pub fn offline() -> Self {
        Self {
            state: State::Offline,
            ..Self::default()
        }
    }

    /// One line for terminals and simple bars.
    pub fn format_plain(&self) -> String {
        let mut parts = Vec::new();
        match self.state {
            State::Offline => parts.push("offline".to_string()),
            State::Idle => parts.push("idle".to_string()),
            State::Preparing => parts.push("preparing".to_string()),
            State::Countdown => parts.push(format!("starting in {}s", self.countdown.unwrap_or(0))),
            State::Recording => {
                parts.push(format!("recording {}", format_elapsed(self.elapsed_secs)));
                if let (Some(width), Some(height)) = (self.width, self.height) {
                    parts.push(format!("{width}x{height}"));
                }
                if let Some(level) = self.mic_level {
                    parts.push(format!("mic {}", level_meter(level)));
                }
            }
        }
        parts.extend(self.processing.iter().map(format_job));
        parts.join(" | ")
    }

    /// Polybar-formatted line; empty when there is nothing to show (hides the module).
    pub fn format_polybar(&self) -> String {
        const RED: &str = "%{F#e5484d}";
        const RESET: &str = "%{F-}";
        let mut parts = Vec::new();
        match self.state {
            State::Offline | State::Idle | State::Preparing => {}
            State::Countdown => parts.push(format!(
                "{RED}●{RESET} REC in {}",
                self.countdown.unwrap_or(0)
            )),
            State::Recording => {
                let mut part = format!("{RED}● REC{RESET} {}", format_elapsed(self.elapsed_secs));
                if let Some(level) = self.mic_level {
                    part.push_str(&format!(" mic {}", level_meter(level)));
                }
                parts.push(part);
            }
        }
        parts.extend(self.processing.iter().map(format_job));
        parts.join("  ")
    }
}

fn format_job(job: &JobStatus) -> String {
    format!("processing {:.0}%", job.progress * 100.0)
}

pub fn format_elapsed(secs: u64) -> String {
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// A single block character showing `level` (0.0..=1.0).
pub fn level_meter(level: f32) -> char {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let index = (level.clamp(0.0, 1.0) * (BLOCKS.len() - 1) as f32).round() as usize;
    BLOCKS[index]
}

#[cfg(test)]
mod tests {
    use super::{format_elapsed, level_meter, JobStatus, State, Status};

    #[test]
    fn formats_elapsed() {
        assert_eq!(format_elapsed(5), "00:05");
        assert_eq!(format_elapsed(125), "02:05");
        assert_eq!(format_elapsed(3725), "1:02:05");
    }

    #[test]
    fn level_meter_spans_blocks() {
        assert_eq!(level_meter(0.0), '▁');
        assert_eq!(level_meter(1.0), '█');
        assert_eq!(level_meter(7.0), '█');
    }

    #[test]
    fn polybar_hides_when_idle() {
        assert_eq!(Status::default().format_polybar(), "");
    }

    #[test]
    fn formats_recording() {
        let status = Status {
            state: State::Recording,
            width: Some(1920),
            height: Some(1080),
            elapsed_secs: 61,
            mic_level: Some(1.0),
            processing: vec![JobStatus {
                name: "a.mp4".into(),
                progress: 0.5,
            }],
            ..Status::default()
        };
        assert_eq!(
            status.format_plain(),
            "recording 01:01 | 1920x1080 | mic █ | processing 50%"
        );
        assert_eq!(
            status.format_polybar(),
            "%{F#e5484d}● REC%{F-} 01:01 mic █  processing 50%"
        );
    }

    #[test]
    fn status_round_trips_as_json() {
        let status = Status {
            state: State::Countdown,
            countdown: Some(3),
            ..Status::default()
        };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), status);
    }
}
