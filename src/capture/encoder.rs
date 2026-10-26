//! Video encoder selection: libx264, or a hardware encoder (NVENC, VAAPI) when one works.
//!
//! Hardware support is probed once per process with a tiny test encode, because an encoder
//! being listed by ffmpeg says nothing about whether the driver can actually open it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::capture::options::Quality;

/// The `recording.encoder` config value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderChoice {
    /// NVENC, then VAAPI, then libx264, whichever works first.
    #[default]
    Auto,
    Software,
    Nvenc,
    Vaapi,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoEncoder {
    Software,
    Nvenc,
    Vaapi { device: PathBuf },
}

/// How much time an encode may spend per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    /// Live capture: must keep up with the frame rate.
    Realtime,
    /// Intermediate renders (zoom).
    Balanced,
    /// The final compression pass.
    Thorough,
}

/// ffmpeg arguments for one video encode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EncoderArgs {
    /// Global options, placed before the inputs.
    pub global: Vec<String>,
    /// Filter appended to the video chain (uploads frames to the GPU).
    pub filter: Option<&'static str>,
    /// `-c:v ...` and its options.
    pub codec: Vec<String>,
}

const VAAPI_UPLOAD: &str = "format=nv12,hwupload";

static RESOLVED: OnceLock<VideoEncoder> = OnceLock::new();

/// The encoder to use for `choice`, probed on first call and cached for the process. An
/// explicitly chosen hardware encoder that does not work falls back to libx264.
pub fn resolve(ffmpeg: &str, choice: EncoderChoice, vaapi_device: Option<&Path>) -> VideoEncoder {
    RESOLVED
        .get_or_init(|| {
            let encoder = detect(ffmpeg, choice, vaapi_device);
            println!("video encoder: {}", encoder.name());
            encoder
        })
        .clone()
}

fn detect(ffmpeg: &str, choice: EncoderChoice, vaapi_device: Option<&Path>) -> VideoEncoder {
    let nvenc = || probe(ffmpeg, &VideoEncoder::Nvenc).then_some(VideoEncoder::Nvenc);
    let vaapi = || {
        vaapi_devices(vaapi_device)
            .into_iter()
            .map(|device| VideoEncoder::Vaapi { device })
            .find(|encoder| probe(ffmpeg, encoder))
    };
    let found = match choice {
        EncoderChoice::Software => return VideoEncoder::Software,
        EncoderChoice::Auto => return nvenc().or_else(vaapi).unwrap_or(VideoEncoder::Software),
        EncoderChoice::Nvenc => nvenc(),
        EncoderChoice::Vaapi => vaapi(),
    };
    found.unwrap_or_else(|| {
        eprintln!("warning: {choice:?} encoding is unavailable, using libx264");
        VideoEncoder::Software
    })
}

/// The configured VAAPI device, or every DRM render node.
fn vaapi_devices(configured: Option<&Path>) -> Vec<PathBuf> {
    if let Some(device) = configured {
        return vec![device.to_path_buf()];
    }
    let mut devices: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
        })
        .collect();
    devices.sort();
    devices
}

/// Encode a fraction of a second of a test pattern.
fn probe(ffmpeg: &str, encoder: &VideoEncoder) -> bool {
    let args = encoder.args(Quality::High, Speed::Realtime);
    let mut command = Command::new(ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(&args.global)
        .args(["-f", "lavfi", "-i", "color=s=256x256:r=30:d=0.2"]);
    if let Some(filter) = args.filter {
        command.args(["-vf", filter]);
    }
    command
        .args(&args.codec)
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

impl VideoEncoder {
    pub fn name(&self) -> String {
        match self {
            VideoEncoder::Software => "libx264".to_string(),
            VideoEncoder::Nvenc => "NVENC".to_string(),
            VideoEncoder::Vaapi { device } => format!("VAAPI ({})", device.display()),
        }
    }

    pub fn is_hardware(&self) -> bool {
        *self != VideoEncoder::Software
    }

    /// Arguments to encode at `quality`. VAAPI cannot encode losslessly, so lossless encodes
    /// use libx264 instead.
    pub fn args(&self, quality: Quality, speed: Speed) -> EncoderArgs {
        let lossless = quality == Quality::Lossless;
        match self {
            VideoEncoder::Vaapi { .. } if lossless => VideoEncoder::Software.args(quality, speed),
            VideoEncoder::Software => {
                let preset = match speed {
                    Speed::Realtime => "ultrafast",
                    Speed::Balanced => "medium",
                    Speed::Thorough => "slow",
                };
                let (flag, value) = if lossless {
                    ("-qp", "0".to_string())
                } else {
                    ("-crf", quality.crf().to_string())
                };
                EncoderArgs {
                    codec: strings(&[
                        "-c:v", "libx264", "-preset", preset, flag, &value, "-pix_fmt", "yuv420p",
                    ]),
                    ..EncoderArgs::default()
                }
            }
            VideoEncoder::Nvenc => {
                let preset = match speed {
                    Speed::Realtime => "p1",
                    Speed::Balanced => "p4",
                    Speed::Thorough => "p6",
                };
                let mut codec = strings(&["-c:v", "h264_nvenc", "-preset", preset]);
                if lossless {
                    codec.extend(strings(&["-tune", "lossless"]));
                } else {
                    let cq = hardware_qp(quality).to_string();
                    codec.extend(strings(&[
                        "-tune", "hq", "-rc", "vbr", "-cq", &cq, "-b:v", "0",
                    ]));
                }
                codec.extend(strings(&["-pix_fmt", "yuv420p"]));
                EncoderArgs {
                    codec,
                    ..EncoderArgs::default()
                }
            }
            VideoEncoder::Vaapi { device } => {
                let qp = hardware_qp(quality).to_string();
                EncoderArgs {
                    global: vec!["-vaapi_device".into(), device.display().to_string()],
                    filter: Some(VAAPI_UPLOAD),
                    codec: strings(&["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", &qp]),
                }
            }
        }
    }
}

/// Constant quantizer for hardware encoders, which need a slightly lower value than x264's
/// CRF for similar quality.
fn hardware_qp(quality: Quality) -> u8 {
    match quality {
        Quality::Low => 30,
        Quality::Medium => 25,
        Quality::High => 21,
        Quality::Lossless => 0,
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::{Speed, VideoEncoder};
    use crate::capture::options::Quality;
    use std::path::PathBuf;

    #[test]
    fn software_presets_follow_speed() {
        let args = VideoEncoder::Software.args(Quality::Medium, Speed::Thorough);
        assert_eq!(
            args.codec.join(" "),
            "-c:v libx264 -preset slow -crf 23 -pix_fmt yuv420p"
        );
        let args = VideoEncoder::Software.args(Quality::Lossless, Speed::Realtime);
        assert!(args.codec.join(" ").contains("-preset ultrafast -qp 0"));
        assert!(args.global.is_empty() && args.filter.is_none());
    }

    #[test]
    fn nvenc_quality_and_lossless() {
        let args = VideoEncoder::Nvenc
            .args(Quality::High, Speed::Thorough)
            .codec
            .join(" ");
        assert!(args.contains("h264_nvenc -preset p6 -tune hq -rc vbr -cq 21"));
        let args = VideoEncoder::Nvenc
            .args(Quality::Lossless, Speed::Realtime)
            .codec
            .join(" ");
        assert!(args.contains("-preset p1 -tune lossless"));
    }

    #[test]
    fn vaapi_uploads_and_falls_back_for_lossless() {
        let vaapi = VideoEncoder::Vaapi {
            device: PathBuf::from("/dev/dri/renderD128"),
        };
        let args = vaapi.args(Quality::Low, Speed::Balanced);
        assert_eq!(args.global, ["-vaapi_device", "/dev/dri/renderD128"]);
        assert_eq!(args.filter, Some("format=nv12,hwupload"));
        assert!(args
            .codec
            .join(" ")
            .contains("h264_vaapi -rc_mode CQP -qp 30"));
        let lossless = vaapi.args(Quality::Lossless, Speed::Realtime);
        assert!(lossless.codec.join(" ").contains("libx264"));
        assert!(lossless.global.is_empty());
    }
}
