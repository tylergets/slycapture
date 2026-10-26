//! Records the pointer while ffmpeg captures, so zoom can be applied afterwards.
//!
//! The log is JSON lines next to the capture: a [`Header`], then one [`Sample`] whenever the
//! pointer moves or a button changes. Coordinates are relative to the recorded region, and
//! times are seconds since the logger started (just before ffmpeg is launched).

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, KeyButMask};

use crate::capture::Region;

const LOG_VERSION: u32 = 1;
/// Fast enough to catch ordinary clicks, which are held for 50ms or more.
const POLL_INTERVAL: Duration = Duration::from_millis(8);
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

pub const BUTTON_LEFT: u8 = 1;
pub const BUTTON_MIDDLE: u8 = 2;
pub const BUTTON_RIGHT: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub version: u32,
    /// Size of the recorded region, which is also the video size.
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub t: f64,
    pub x: i32,
    pub y: i32,
    /// Bitmask of `BUTTON_*`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub buttons: u8,
}

fn is_zero(value: &u8) -> bool {
    *value == 0
}

/// A button press, in region coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Click {
    pub t: f64,
    pub x: f64,
    pub y: f64,
}

/// `foo.capture.mkv` -> `foo.capture.cursor.jsonl`
pub fn sidecar_path(capture: &Path) -> PathBuf {
    capture.with_extension("cursor.jsonl")
}

/// Polls the pointer on a background thread until stopped.
pub struct CursorLogger {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl CursorLogger {
    pub fn start(region: Region, framerate: u32, path: PathBuf) -> Result<Self> {
        let file = File::create(&path)
            .with_context(|| format!("failed to create cursor log: {}", path.display()))?;
        let header = Header {
            version: LOG_VERSION,
            width: region.width,
            height: region.height,
            framerate,
        };

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::spawn(move || log_pointer(BufWriter::new(file), header, region, &stop))
        };
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        match self.thread.take().map(JoinHandle::join) {
            Some(Ok(result)) => result,
            Some(Err(_)) => bail!("cursor logger thread panicked"),
            None => Ok(()),
        }
    }
}

impl Drop for CursorLogger {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn log_pointer(
    mut out: BufWriter<File>,
    header: Header,
    region: Region,
    stop: &AtomicBool,
) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(None).context("cursor logger: X connect failed")?;
    let root = conn.setup().roots[screen_num].root;

    serde_json::to_writer(&mut out, &header)?;
    out.write_all(b"\n")?;

    let started = Instant::now();
    let mut last_flush = started;
    let mut last: Option<(i32, i32, u8)> = None;
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        let pointer = conn.query_pointer(root)?.reply()?;
        let x = i32::from(pointer.root_x) - region.x;
        let y = i32::from(pointer.root_y) - region.y;
        let buttons = buttons_from_mask(pointer.mask);

        // Always end with a sample, so the log's duration is the recording's.
        if stopping || last != Some((x, y, buttons)) {
            let sample = Sample {
                t: started.elapsed().as_secs_f64(),
                x,
                y,
                buttons,
            };
            serde_json::to_writer(&mut out, &sample)?;
            out.write_all(b"\n")?;
            last = Some((x, y, buttons));
        }
        if stopping {
            break;
        }

        // Keep the log useful if slycapture dies mid-recording, like the Matroska capture.
        if last_flush.elapsed() >= FLUSH_INTERVAL {
            out.flush()?;
            last_flush = Instant::now();
        }
        thread::sleep(POLL_INTERVAL);
    }
    out.flush()?;
    Ok(())
}

fn buttons_from_mask(mask: KeyButMask) -> u8 {
    [
        (KeyButMask::BUTTON1, BUTTON_LEFT),
        (KeyButMask::BUTTON2, BUTTON_MIDDLE),
        (KeyButMask::BUTTON3, BUTTON_RIGHT),
    ]
    .into_iter()
    .filter(|(bit, _)| mask.contains(*bit))
    .fold(0, |buttons, (_, button)| buttons | button)
}

/// A loaded cursor log.
#[derive(Debug, Clone)]
pub struct CursorLog {
    pub header: Header,
    samples: Vec<Sample>,
}

impl CursorLog {
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open cursor log: {}", path.display()))?;
        Self::parse(BufReader::new(file))
            .with_context(|| format!("failed to parse cursor log: {}", path.display()))
    }

    pub fn parse(reader: impl BufRead) -> Result<Self> {
        let mut lines = reader.lines();
        let header: Header = match lines.next() {
            Some(line) => serde_json::from_str(&line?)?,
            None => bail!("cursor log is empty"),
        };
        if header.version != LOG_VERSION {
            bail!("unsupported cursor log version {}", header.version);
        }

        let mut samples = Vec::new();
        for line in lines {
            let line = line?;
            // A crash can leave a truncated last line; everything before it is still good.
            match serde_json::from_str::<Sample>(&line) {
                Ok(sample) => samples.push(sample),
                Err(_) => break,
            }
        }
        Ok(Self { header, samples })
    }

    pub fn from_samples(header: Header, samples: Vec<Sample>) -> Self {
        Self { header, samples }
    }

    /// Time of the last sample.
    pub fn duration(&self) -> f64 {
        self.samples.last().map_or(0.0, |sample| sample.t)
    }

    /// Every press of any button.
    pub fn clicks(&self) -> Vec<Click> {
        let mut previous = 0;
        let mut clicks = Vec::new();
        for sample in &self.samples {
            if sample.buttons & !previous != 0 {
                clicks.push(Click {
                    t: sample.t,
                    x: f64::from(sample.x),
                    y: f64::from(sample.y),
                });
            }
            previous = sample.buttons;
        }
        clicks
    }

    /// Pointer position at time `t`, interpolated between samples. Samples are only written
    /// on change, so the pointer holds still from one sample until shortly before the next.
    pub fn position_at(&self, t: f64) -> Option<(f64, f64)> {
        let next = self.samples.partition_point(|sample| sample.t <= t);
        let before = next.checked_sub(1).map(|index| self.samples[index]);
        let after = self.samples.get(next).copied();
        Some(match (before, after) {
            (Some(a), Some(b)) => {
                // Treat the gap as "still, then moved during the last poll interval".
                let move_start = (b.t - POLL_INTERVAL.as_secs_f64()).max(a.t);
                let span = b.t - move_start;
                let f = if span > 0.0 && t > move_start {
                    (t - move_start) / span
                } else {
                    0.0
                };
                (
                    lerp(f64::from(a.x), f64::from(b.x), f),
                    lerp(f64::from(a.y), f64::from(b.y), f),
                )
            }
            (Some(a), None) => (f64::from(a.x), f64::from(a.y)),
            (None, Some(b)) => (f64::from(b.x), f64::from(b.y)),
            (None, None) => return None,
        })
    }
}

fn lerp(a: f64, b: f64, f: f64) -> f64 {
    a + (b - a) * f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(lines: &str) -> CursorLog {
        CursorLog::parse(lines.as_bytes()).expect("log should parse")
    }

    const HEADER: &str = r#"{"version":1,"width":100,"height":50,"framerate":30}"#;

    #[test]
    fn sidecar_sits_next_to_capture() {
        assert_eq!(
            sidecar_path(Path::new("/v/demo.capture.mkv")),
            Path::new("/v/demo.capture.cursor.jsonl")
        );
    }

    #[test]
    fn detects_presses_not_holds() {
        let log = log(&format!(
            "{HEADER}\n\
             {{\"t\":0.0,\"x\":1,\"y\":1}}\n\
             {{\"t\":1.0,\"x\":2,\"y\":2,\"buttons\":1}}\n\
             {{\"t\":1.1,\"x\":3,\"y\":3,\"buttons\":1}}\n\
             {{\"t\":1.2,\"x\":3,\"y\":3,\"buttons\":5}}\n\
             {{\"t\":1.3,\"x\":3,\"y\":3}}\n"
        ));
        let clicks = log.clicks();
        assert_eq!(clicks.len(), 2);
        assert_eq!((clicks[0].t, clicks[0].x), (1.0, 2.0));
        assert_eq!(clicks[1].t, 1.2);
    }

    #[test]
    fn tolerates_truncated_last_line() {
        let log = log(&format!("{HEADER}\n{{\"t\":0.0,\"x\":1,\"y\":1}}\n{{\"t\":0.5,\"x\""));
        assert_eq!(log.duration(), 0.0);
    }

    #[test]
    fn interpolates_position() {
        let log = log(&format!(
            "{HEADER}\n{{\"t\":0.0,\"x\":0,\"y\":0}}\n{{\"t\":1.0,\"x\":80,\"y\":40}}\n"
        ));
        assert_eq!(log.position_at(-1.0), Some((0.0, 0.0)));
        assert_eq!(log.position_at(0.5), Some((0.0, 0.0)));
        assert_eq!(log.position_at(0.996), Some((40.0, 20.0)));
        assert_eq!(log.position_at(5.0), Some((80.0, 40.0)));
    }

    #[test]
    fn maps_button_mask() {
        assert_eq!(buttons_from_mask(KeyButMask::BUTTON1), BUTTON_LEFT);
        assert_eq!(
            buttons_from_mask(KeyButMask::BUTTON3 | KeyButMask::SHIFT),
            BUTTON_RIGHT
        );
    }
}
