use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::capture::audio::{self, Level};
use crate::capture::options::RecordingOptions;
use crate::capture::Region;

const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const STDERR_TAIL_LINES: usize = 5;
const INPUT_QUEUE_SIZE: &str = "1024";

/// An in-progress ffmpeg x11grab recording. Always written as Matroska, which stays playable
/// if slycapture or ffmpeg dies mid-recording; `capture::transcode` produces the final file.
pub struct Recording {
    child: Child,
    output: PathBuf,
    region: Region,
    started: Instant,
    mic_level: Option<Level>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

/// Resolved audio inputs for one recording.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AudioInputs {
    microphone: Option<String>,
    system: Option<String>,
}

impl Recording {
    pub fn start(
        ffmpeg: &str,
        display: &str,
        region: Region,
        options: &RecordingOptions,
        output: PathBuf,
    ) -> Result<Self> {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create output directory: {}", parent.display())
            })?;
        }

        let inputs = AudioInputs {
            microphone: options.microphone.clone(),
            system: options.system_audio.then(audio::default_monitor_source),
        };
        let metered = inputs.microphone.is_some();

        let mut child = Command::new(ffmpeg)
            .args(ffmpeg_args(display, region, options, &inputs, &output))
            .stdin(Stdio::piped())
            .stdout(if metered {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to launch {ffmpeg}"))?;

        let mic_level = child.stdout.take().map(audio::spawn_level_reader);
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("ffmpeg: {line}");
                    let mut tail = tail.lock().unwrap();
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }

        Ok(Self {
            child,
            output,
            region,
            started: Instant::now(),
            mic_level,
            stderr_tail,
        })
    }

    pub fn output(&self) -> &Path {
        &self.output
    }

    pub fn region(&self) -> Region {
        self.region
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Microphone level, if the microphone is being recorded.
    pub fn mic_level(&self) -> Option<Level> {
        self.mic_level.clone()
    }

    /// Returns the exit status if ffmpeg has already exited (for example, on error).
    pub fn try_exit_status(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// The last few lines ffmpeg wrote to stderr, for error messages.
    pub fn error_output(&self) -> String {
        let tail = self.stderr_tail.lock().unwrap();
        tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// Ask ffmpeg to finish the file cleanly, killing it if it does not exit in time.
    pub fn stop(mut self) -> Result<PathBuf> {
        if let Some(mut stdin) = self.child.stdin.take() {
            // `q` on stdin makes ffmpeg flush and finalize the container.
            let _ = stdin.write_all(b"q");
        }

        let deadline = Instant::now() + STOP_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break None;
            }
            thread::sleep(Duration::from_millis(50));
        };

        if !self.output.exists() {
            let details = self.error_output();
            match status {
                Some(status) => bail!("ffmpeg failed ({status}): {details}"),
                None => bail!("ffmpeg did not stop in time and wrote no file: {details}"),
            }
        }
        Ok(self.output)
    }
}

fn ffmpeg_args(
    display: &str,
    region: Region,
    options: &RecordingOptions,
    inputs: &AudioInputs,
    output: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostats".into(),
        "-y".into(),
        "-thread_queue_size".into(),
        INPUT_QUEUE_SIZE.into(),
        "-f".into(),
        "x11grab".into(),
        "-framerate".into(),
        options.framerate.to_string(),
        "-draw_mouse".into(),
        u8::from(options.draw_mouse).to_string(),
        "-video_size".into(),
        format!("{}x{}", region.width, region.height),
        "-i".into(),
        format!("{display}+{},{}", region.x, region.y),
    ];

    // Input 0 is video; audio inputs follow in this order.
    let audio_sources: Vec<&String> = [&inputs.microphone, &inputs.system]
        .into_iter()
        .flatten()
        .collect();
    for source in &audio_sources {
        args.extend([
            "-thread_queue_size".into(),
            INPUT_QUEUE_SIZE.into(),
            "-f".into(),
            "pulse".into(),
            "-i".into(),
            (*source).clone(),
        ]);
    }

    args.extend(["-map".into(), "0:v".into()]);
    match audio_sources.len() {
        0 => {}
        1 => args.extend(["-map".into(), "1:a".into()]),
        _ => args.extend([
            "-filter_complex".into(),
            "[1:a][2:a]amix=inputs=2:duration=longest:normalize=0[aout]".into(),
            "-map".into(),
            "[aout]".into(),
        ]),
    }

    // When a compression pass follows, capture losslessly so quality is only lost once.
    let (video_quality, video_quality_value, audio_codec) = if options.compress {
        ("-qp", "0".to_string(), "pcm_s16le")
    } else {
        ("-crf", options.quality.crf().to_string(), "aac")
    };
    args.extend([
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "ultrafast".into(),
        video_quality.into(),
        video_quality_value,
        "-pix_fmt".into(),
        "yuv420p".into(),
    ]);
    if !audio_sources.is_empty() {
        args.extend(["-c:a".into(), audio_codec.into()]);
    }
    args.push(output.display().to_string());

    // Second output: the microphone as raw samples on stdout for the level meter.
    if inputs.microphone.is_some() {
        args.extend(["-map".into(), "1:a".into()]);
        args.extend(audio::meter_output_args());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::{ffmpeg_args, AudioInputs};
    use crate::capture::options::{Format, Quality, RecordingOptions};
    use crate::capture::Region;
    use std::path::Path;

    fn options() -> RecordingOptions {
        RecordingOptions {
            base_name: "test".into(),
            framerate: 30,
            draw_mouse: true,
            format: Format::Mkv,
            quality: Quality::High,
            countdown: 0,
            microphone: None,
            system_audio: false,
            compress: false,
            keep_capture: true,
            window_size: None,
            intro: false,
            intro_title: String::new(),
            intro_logo: None,
            zoom: false,
        }
    }

    fn args(inputs: AudioInputs) -> String {
        ffmpeg_args(
            ":0",
            Region::new(100, 50, 1920, 1080),
            &options(),
            &inputs,
            Path::new("/tmp/out.mkv"),
        )
        .join(" ")
    }

    #[test]
    fn video_only() {
        let args = args(AudioInputs {
            microphone: None,
            system: None,
        });
        assert!(args.contains("-f x11grab -framerate 30 -draw_mouse 1"));
        assert!(args.contains("-video_size 1920x1080 -i :0+100,50"));
        assert!(args.contains("-crf 18"));
        assert!(!args.contains("pulse"));
        assert!(args.ends_with("/tmp/out.mkv"));
    }

    #[test]
    fn microphone_is_recorded_and_metered() {
        let args = args(AudioInputs {
            microphone: Some("mic".into()),
            system: None,
        });
        assert!(args.contains("-f pulse -i mic -map 0:v -map 1:a"));
        assert!(args.contains("-c:a aac"));
    }

    #[test]
    fn captures_losslessly_when_compressing() {
        let mut options = options();
        options.compress = true;
        let args = ffmpeg_args(
            ":0",
            Region::new(0, 0, 640, 480),
            &options,
            &AudioInputs {
                microphone: Some("mic".into()),
                system: None,
            },
            Path::new("/tmp/out.mkv"),
        )
        .join(" ");
        assert!(args.contains("-preset ultrafast -qp 0"));
        assert!(args.contains("-c:a pcm_s16le"));
        assert!(args.ends_with("/tmp/out.mkv -map 1:a -ac 1 -ar 16000 -f s16le pipe:1"));
    }

    #[test]
    fn microphone_and_system_audio_are_mixed() {
        let args = args(AudioInputs {
            microphone: Some("mic".into()),
            system: Some("sink.monitor".into()),
        });
        assert!(args.contains("-i mic -thread_queue_size 1024 -f pulse -i sink.monitor"));
        assert!(args.contains("amix=inputs=2"));
        assert!(args.contains("-map [aout]"));
    }
}
