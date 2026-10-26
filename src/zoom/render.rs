//! Renders a zoomed copy of a capture: ffmpeg decodes to raw frames, each frame is cropped and
//! scaled here according to the camera, and a second ffmpeg encodes the result together with
//! the capture's audio.
//!
//! The capture is only ever read. Output goes to a `.part` file that is renamed into place
//! once ffmpeg has finished, so a failed or interrupted render never leaves a file that looks
//! complete.

use std::fs;
use std::io::{BufRead, BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};

use anyhow::{bail, Context, Result};

use crate::capture::encoder::{Speed, VideoEncoder};
use crate::capture::options::Quality;
use crate::zoom::camera::{Camera, CameraPlanner};
use crate::zoom::cursor_log::{Click, CursorLog};
use crate::zoom::ZoomSettings;

const BYTES_PER_PIXEL: usize = 3;
/// Fixed-point precision for bilinear weights.
const WEIGHT_BITS: u32 = 8;
const WEIGHT_ONE: u32 = 1 << WEIGHT_BITS;

const RIPPLE_SECS: f64 = 0.45;
const RIPPLE_COLOR: [u8; 3] = [255, 196, 0];
/// Ripple sizes are for a 1080p output and scale with the output height.
const RIPPLE_START_RADIUS: f64 = 10.0;
const RIPPLE_END_RADIUS: f64 = 38.0;
const RIPPLE_THICKNESS: f64 = 4.0;

/// Render `capture` zoomed according to `log` into `output`. `on_progress` receives 0.0..=1.0.
#[allow(clippy::too_many_arguments)]
pub fn render(
    ffmpeg: &str,
    capture: &Path,
    log: &CursorLog,
    output: &Path,
    settings: &ZoomSettings,
    quality: Quality,
    video_encoder: &VideoEncoder,
    mut on_progress: impl FnMut(f32),
) -> Result<()> {
    if paths_match(capture, output) {
        bail!("refusing to render over the capture: {}", output.display());
    }
    let width = log.header.width as usize;
    let height = log.header.height as usize;
    let framerate = log.header.framerate.max(1);
    if width == 0 || height == 0 {
        bail!("cursor log has an empty frame size");
    }

    let partial = partial_path(output);
    let mut decoder = spawn_ffmpeg(ffmpeg, &decoder_args(capture, log), true, false)?;
    let mut encoder = spawn_ffmpeg(
        ffmpeg,
        &encoder_args(capture, log, &partial, output, quality, video_encoder),
        false,
        true,
    )?;
    let decoder_errors = drain_stderr(&mut decoder);
    let encoder_errors = drain_stderr(&mut encoder);

    let result = pump_frames(
        &mut decoder,
        &mut encoder,
        log,
        settings,
        width,
        height,
        framerate,
        &mut on_progress,
    );
    // Close stdin so the encoder finishes the file, then collect both exit statuses.
    drop(encoder.stdin.take());
    let decoder_status = decoder.wait()?;
    let encoder_status = encoder.wait()?;
    let decoder_error = decoder_errors.join().unwrap_or_default();
    let encoder_error = encoder_errors.join().unwrap_or_default();

    let failure = match (result, decoder_status.success(), encoder_status.success()) {
        (Err(err), _, _) if !encoder_status.success() => {
            Some(format!("{err:#}; encoder failed ({encoder_status}): {encoder_error}"))
        }
        (Err(err), _, _) => Some(format!("{err:#}")),
        (Ok(()), false, _) => Some(format!("decoder failed ({decoder_status}): {decoder_error}")),
        (Ok(()), _, false) => Some(format!("encoder failed ({encoder_status}): {encoder_error}")),
        (Ok(()), true, true) => None,
    };
    if let Some(failure) = failure {
        let _ = fs::remove_file(&partial);
        bail!("zoom render failed: {failure}");
    }

    fs::rename(&partial, output).with_context(|| {
        format!(
            "failed to move {} to {}",
            partial.display(),
            output.display()
        )
    })?;
    on_progress(1.0);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn pump_frames(
    decoder: &mut Child,
    encoder: &mut Child,
    log: &CursorLog,
    settings: &ZoomSettings,
    width: usize,
    height: usize,
    framerate: u32,
    on_progress: &mut impl FnMut(f32),
) -> Result<()> {
    let mut input = decoder.stdout.take().context("decoder stdout unavailable")?;
    let mut output = BufWriter::new(encoder.stdin.take().context("encoder stdin unavailable")?);

    let frame_len = width * height * BYTES_PER_PIXEL;
    let mut src = vec![0u8; frame_len];
    let mut dst = vec![0u8; frame_len];
    let mut planner = CameraPlanner::new(log, settings);
    let clicks = planner.clicks().to_vec();
    let expected_frames = (log.duration() * f64::from(framerate)).max(1.0);

    let mut frame = 0u64;
    while read_frame(&mut input, &mut src)? {
        let t = frame as f64 / f64::from(framerate);
        let camera = planner.next(t);
        sample_frame(&src, &mut dst, width, height, camera);
        if settings.click_effect {
            draw_ripples(&mut dst, width, height, camera, &clicks, t + settings.sync_offset);
        }
        output
            .write_all(&dst)
            .context("failed to send a frame to the encoder")?;

        frame += 1;
        if frame % u64::from(framerate) == 0 {
            on_progress((frame as f64 / expected_frames).min(0.99) as f32);
        }
    }
    output.flush().context("failed to send a frame to the encoder")?;
    Ok(())
}

/// Fill `buf` with the next frame. Returns `false` at end of stream.
fn read_frame(input: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => bail!("decoder stopped in the middle of a frame"),
            Ok(read) => filled += read,
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err).context("failed to read a frame from the decoder"),
        }
    }
    Ok(true)
}

fn decoder_args(capture: &Path, log: &CursorLog) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-i".into(),
        capture.display().to_string(),
        "-map".into(),
        "0:v:0".into(),
        // Constant frame rate so frame N is at N / framerate; the scale is a no-op unless the
        // log and capture disagree, which would otherwise desync every frame read.
        "-vf".into(),
        format!(
            "fps={},scale={}:{}",
            log.header.framerate, log.header.width, log.header.height
        ),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "rgb24".into(),
        "pipe:1".into(),
    ]
}

fn encoder_args(
    capture: &Path,
    log: &CursorLog,
    partial: &Path,
    output: &Path,
    quality: Quality,
    video_encoder: &VideoEncoder,
) -> Vec<String> {
    let video = video_encoder.args(quality, Speed::Balanced);
    let mp4 = output
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"));

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
    ];
    args.extend(video.global);
    args.extend([
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "rgb24".into(),
        "-video_size".into(),
        format!("{}x{}", log.header.width, log.header.height),
        "-framerate".into(),
        log.header.framerate.to_string(),
        "-i".into(),
        "pipe:0".into(),
        "-i".into(),
        capture.display().to_string(),
        "-map".into(),
        "0:v".into(),
        "-map".into(),
        "1:a?".into(),
    ]);
    if let Some(filter) = video.filter {
        args.extend(["-vf".into(), filter.into()]);
    }
    args.extend(video.codec);
    args.extend([
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
    ]);
    if mp4 {
        args.extend(["-movflags".into(), "+faststart".into()]);
    }
    // The `.part` name hides the real extension from ffmpeg, so name the container.
    args.extend([
        "-f".into(),
        if mp4 { "mp4" } else { "matroska" }.into(),
        partial.display().to_string(),
    ]);
    args
}

fn spawn_ffmpeg(ffmpeg: &str, args: &[String], read_stdout: bool, write_stdin: bool) -> Result<Child> {
    Command::new(ffmpeg)
        .args(args)
        .stdin(if write_stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(if read_stdout { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to launch {ffmpeg}"))
}

/// Read stderr on a thread so ffmpeg never blocks on it; returns the last line.
fn drain_stderr(child: &mut Child) -> JoinHandle<String> {
    let stderr = child.stderr.take();
    thread::spawn(move || {
        let mut last = String::new();
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("ffmpeg: {line}");
                last = line;
            }
        }
        last
    })
}

/// `foo.zoom.mp4` -> `foo.zoom.mp4.part`
fn partial_path(output: &Path) -> PathBuf {
    let mut name = output.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

fn paths_match(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// One bilinear tap: the lower source index and the weight of the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tap {
    index: usize,
    next: usize,
    weight: u32,
}

/// Where each output pixel along one axis samples from, for a view starting at `origin`.
fn taps(len: usize, origin: f64, scale: f64) -> Vec<Tap> {
    let last = len - 1;
    (0..len)
        .map(|out| {
            let source = (origin + (out as f64 + 0.5) / scale - 0.5).clamp(0.0, last as f64);
            let index = source.floor() as usize;
            Tap {
                index,
                next: (index + 1).min(last),
                weight: ((source - index as f64) * f64::from(WEIGHT_ONE)).round() as u32,
            }
        })
        .collect()
}

/// Crop `src` to the camera's view and scale it up to fill `dst`. Both are packed RGB.
pub fn sample_frame(src: &[u8], dst: &mut [u8], width: usize, height: usize, camera: Camera) {
    let (x0, y0) = camera.origin(width as f64, height as f64);
    let columns = taps(width, x0, camera.scale);
    let rows = taps(height, y0, camera.scale);
    let stride = width * BYTES_PER_PIXEL;

    let threads = thread::available_parallelism().map_or(1, usize::from);
    let rows_per_chunk = height.div_ceil(threads).max(1);
    thread::scope(|scope| {
        for (chunk_index, chunk) in dst.chunks_mut(rows_per_chunk * stride).enumerate() {
            let (columns, rows) = (&columns, &rows);
            scope.spawn(move || {
                for (row_offset, out_row) in chunk.chunks_exact_mut(stride).enumerate() {
                    let row = rows[chunk_index * rows_per_chunk + row_offset];
                    let top = &src[row.index * stride..][..stride];
                    let bottom = &src[row.next * stride..][..stride];
                    sample_row(top, bottom, row.weight, columns, out_row);
                }
            });
        }
    });
}

fn sample_row(top: &[u8], bottom: &[u8], fy: u32, columns: &[Tap], out: &mut [u8]) {
    for (column, pixel) in columns.iter().zip(out.chunks_exact_mut(BYTES_PER_PIXEL)) {
        let left = column.index * BYTES_PER_PIXEL;
        let right = column.next * BYTES_PER_PIXEL;
        let fx = column.weight;
        for channel in 0..BYTES_PER_PIXEL {
            let blend = |row: &[u8]| {
                u32::from(row[left + channel]) * (WEIGHT_ONE - fx)
                    + u32::from(row[right + channel]) * fx
            };
            let value = blend(top) * (WEIGHT_ONE - fy) + blend(bottom) * fy;
            pixel[channel] = ((value + (1 << (2 * WEIGHT_BITS - 1))) >> (2 * WEIGHT_BITS)) as u8;
        }
    }
}

/// Draw an expanding, fading ring at each click that happened in the last `RIPPLE_SECS`.
fn draw_ripples(dst: &mut [u8], width: usize, height: usize, camera: Camera, clicks: &[Click], t: f64) {
    let (x0, y0) = camera.origin(width as f64, height as f64);
    let size = height as f64 / 1080.0;
    for click in clicks {
        let age = t - click.t;
        if !(0.0..RIPPLE_SECS).contains(&age) {
            continue;
        }
        let progress = age / RIPPLE_SECS;
        let ease_out = 1.0 - (1.0 - progress).powi(3);
        draw_ring(
            dst,
            width,
            height,
            ((click.x - x0) * camera.scale, (click.y - y0) * camera.scale),
            (RIPPLE_START_RADIUS + (RIPPLE_END_RADIUS - RIPPLE_START_RADIUS) * ease_out) * size,
            RIPPLE_THICKNESS * size,
            0.85 * (1.0 - progress),
        );
    }
}

/// Alpha-blend an anti-aliased ring, with a faint fill inside it.
fn draw_ring(
    dst: &mut [u8],
    width: usize,
    height: usize,
    (cx, cy): (f64, f64),
    radius: f64,
    thickness: f64,
    alpha: f64,
) {
    let reach = radius + thickness;
    let x_range = (cx - reach).floor().max(0.0) as usize..((cx + reach).ceil().max(0.0) as usize).min(width);
    let y_range = (cy - reach).floor().max(0.0) as usize..((cy + reach).ceil().max(0.0) as usize).min(height);
    for y in y_range {
        for x in x_range.clone() {
            let distance = (x as f64 + 0.5 - cx).hypot(y as f64 + 0.5 - cy);
            let ring = (thickness / 2.0 + 0.5 - (distance - radius).abs()).clamp(0.0, 1.0);
            let fill = (radius - distance + 0.5).clamp(0.0, 1.0) * 0.25;
            let coverage = ring.max(fill) * alpha;
            if coverage <= 0.0 {
                continue;
            }
            let pixel = &mut dst[(y * width + x) * BYTES_PER_PIXEL..][..BYTES_PER_PIXEL];
            for (channel, color) in pixel.iter_mut().zip(RIPPLE_COLOR) {
                *channel = (f64::from(*channel) * (1.0 - coverage) + f64::from(color) * coverage)
                    .round() as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: usize, height: usize) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).flat_map(move |x| [x as u8, y as u8, 7]))
            .collect()
    }

    #[test]
    fn unzoomed_frame_is_unchanged() {
        let src = gradient(64, 36);
        let mut dst = vec![0; src.len()];
        let camera = Camera {
            cx: 32.0,
            cy: 18.0,
            scale: 1.0,
        };
        sample_frame(&src, &mut dst, 64, 36, camera);
        assert_eq!(src, dst);
    }

    #[test]
    fn zoom_shows_the_middle() {
        let src = gradient(64, 36);
        let mut dst = vec![0; src.len()];
        let camera = Camera {
            cx: 32.0,
            cy: 18.0,
            scale: 2.0,
        };
        sample_frame(&src, &mut dst, 64, 36, camera);
        // The view spans source x 16..48, so the output's left edge is near x=16
        // and its right edge near x=47.
        let first = &dst[..3];
        let last_in_row = &dst[(64 - 1) * 3..][..3];
        assert!((15..=17).contains(&first[0]), "{first:?}");
        assert!((46..=48).contains(&last_in_row[0]), "{last_in_row:?}");
        assert!(dst.chunks(3).all(|pixel| pixel[2] == 7));
    }

    #[test]
    fn taps_stay_in_bounds() {
        for tap in taps(10, -3.0, 1.5).into_iter().chain(taps(10, 8.0, 1.5)) {
            assert!(tap.index < 10 && tap.next < 10 && tap.weight <= WEIGHT_ONE);
        }
    }

    #[test]
    fn ripple_only_draws_near_click() {
        let mut dst = vec![0u8; 200 * 200 * 3];
        let camera = Camera {
            cx: 100.0,
            cy: 100.0,
            scale: 1.0,
        };
        let click = Click {
            t: 1.0,
            x: 100.0,
            y: 100.0,
        };
        draw_ripples(&mut dst, 200, 200, camera, &[click], 1.1);
        let touched = |x: usize, y: usize| dst[(y * 200 + x) * 3] != 0;
        assert!(touched(100, 100));
        assert!(!touched(0, 0) && !touched(199, 199));

        let mut later = vec![0u8; 200 * 200 * 3];
        draw_ripples(&mut later, 200, 200, camera, &[click], 2.0);
        assert!(later.iter().all(|&value| value == 0));
    }

    #[test]
    fn encodes_to_partial_file_with_explicit_container() {
        let log = CursorLog::parse(
            r#"{"version":1,"width":1280,"height":720,"framerate":30}"#.as_bytes(),
        )
        .unwrap();
        let output = Path::new("/v/a.zoom.mp4");
        let args = encoder_args(
            Path::new("/v/a.capture.mkv"),
            &log,
            &partial_path(output),
            output,
            Quality::High,
            &VideoEncoder::Software,
        )
        .join(" ");
        assert!(args.contains("-video_size 1280x720 -framerate 30 -i pipe:0"));
        assert!(args.contains("-map 1:a?"));
        assert!(args.contains("-crf 18"));
        assert!(args.ends_with("-movflags +faststart -f mp4 /v/a.zoom.mp4.part"));
    }

    #[test]
    fn refuses_to_overwrite_capture() {
        let log = CursorLog::parse(
            r#"{"version":1,"width":2,"height":2,"framerate":30}"#.as_bytes(),
        )
        .unwrap();
        let capture = Path::new("/nonexistent/a.capture.mkv");
        let result = render(
            "ffmpeg",
            capture,
            &log,
            capture,
            &ZoomSettings::default(),
            Quality::High,
            &VideoEncoder::Software,
            |_| {},
        );
        assert!(result.is_err());
    }
}
