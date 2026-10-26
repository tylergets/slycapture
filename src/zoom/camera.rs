//! Turns a cursor log into a smooth virtual camera: where each output frame looks and how far
//! it is zoomed in.
//!
//! Each click opens a zoom window from `lead` seconds before it (we know the future, so the
//! camera can arrive as the click happens) to `hold` seconds after. Close windows are merged so
//! the camera does not bounce out and back in. While zoomed, the camera only pans when the
//! pointer leaves a dead zone in the middle of the view. Zoom and pan are driven by critically
//! damped springs, so motion eases in and out without overshoot.

use crate::zoom::cursor_log::{Click, CursorLog};
use crate::zoom::ZoomSettings;

/// Spring stiffness for zoom in/out; settles in about 0.6s.
const ZOOM_OMEGA: f64 = 7.0;
/// Spring stiffness for panning; a bit softer than zoom so following feels relaxed.
const PAN_OMEGA: f64 = 5.0;

/// The part of the source frame shown in one output frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// Center of the view, in source pixels.
    pub cx: f64,
    pub cy: f64,
    /// 1.0 shows the whole frame; 2.0 shows a quarter of it.
    pub scale: f64,
}

impl Camera {
    /// Top-left corner of the view, in source pixels.
    pub fn origin(&self, width: f64, height: f64) -> (f64, f64) {
        (
            self.cx - width / self.scale / 2.0,
            self.cy - height / self.scale / 2.0,
        )
    }
}

/// Steps the camera forward one output frame at a time.
pub struct CameraPlanner<'a> {
    log: &'a CursorLog,
    settings: ZoomSettings,
    clicks: Vec<Click>,
    windows: Vec<(f64, f64)>,
    width: f64,
    height: f64,
    /// Zoom is animated in log space so zooming in and out feel equally fast.
    log_scale: Spring,
    cx: Spring,
    cy: Spring,
    focus: (f64, f64),
    was_zoomed: bool,
    last_t: Option<f64>,
}

impl<'a> CameraPlanner<'a> {
    pub fn new(log: &'a CursorLog, settings: &ZoomSettings) -> Self {
        let width = f64::from(log.header.width);
        let height = f64::from(log.header.height);
        let clicks = usable_clicks(log, settings);
        let windows = zoom_windows(&clicks, settings);
        Self {
            log,
            settings: settings.clone(),
            clicks,
            windows,
            width,
            height,
            log_scale: Spring::new(0.0, ZOOM_OMEGA),
            cx: Spring::new(width / 2.0, PAN_OMEGA),
            cy: Spring::new(height / 2.0, PAN_OMEGA),
            focus: (width / 2.0, height / 2.0),
            was_zoomed: false,
            last_t: None,
        }
    }

    /// Clicks that get a zoom and a click effect.
    pub fn clicks(&self) -> &[Click] {
        &self.clicks
    }

    /// Camera for the frame at video time `t`. Call with increasing `t`.
    pub fn next(&mut self, t: f64) -> Camera {
        let log_t = t + self.settings.sync_offset;
        let dt = self.last_t.map_or(0.0, |last| (t - last).max(0.0));
        self.last_t = Some(t);

        let zoomed = self
            .windows
            .iter()
            .any(|&(start, end)| (start..end).contains(&log_t));
        let max_scale = self.settings.max_scale;

        if zoomed {
            let aim = self
                .upcoming_click(log_t)
                .or_else(|| self.log.position_at(log_t))
                .unwrap_or(self.focus);
            if self.was_zoomed {
                self.focus = follow_with_dead_zone(
                    self.focus,
                    aim,
                    self.width / max_scale * self.settings.dead_zone / 2.0,
                    self.height / max_scale * self.settings.dead_zone / 2.0,
                );
            } else {
                self.focus = aim;
            }
            self.focus = self.clamp_center(self.focus, max_scale);
        }
        self.was_zoomed = zoomed;

        let target_scale = if zoomed { max_scale } else { 1.0 };
        self.log_scale.step(target_scale.ln(), dt);
        let scale = self.log_scale.value.exp().clamp(1.0, max_scale);

        self.cx.step(self.focus.0, dt);
        self.cy.step(self.focus.1, dt);
        // Keep the springs where the view actually is, so a later zoom starts from what was
        // on screen rather than from a point the clamp hid.
        let (cx, cy) = self.clamp_center((self.cx.value, self.cy.value), scale);
        self.cx.value = cx;
        self.cy.value = cy;

        Camera { cx, cy, scale }
    }

    /// While waiting for a click, aim where it will happen rather than at the moving pointer.
    fn upcoming_click(&self, t: f64) -> Option<(f64, f64)> {
        let next = self.clicks.partition_point(|click| click.t < t);
        self.clicks
            .get(next)
            .filter(|click| click.t - t <= self.settings.lead)
            .map(|click| (click.x, click.y))
    }

    /// Keep a view of `scale` inside the frame.
    fn clamp_center(&self, (cx, cy): (f64, f64), scale: f64) -> (f64, f64) {
        let half_w = self.width / scale / 2.0;
        let half_h = self.height / scale / 2.0;
        (
            cx.clamp(half_w, self.width - half_w),
            cy.clamp(half_h, self.height - half_h),
        )
    }
}

/// Clicks inside the frame, minus the ones at the very end, which are almost always the user
/// reaching for the tray or a hotkey to stop recording.
fn usable_clicks(log: &CursorLog, settings: &ZoomSettings) -> Vec<Click> {
    let width = f64::from(log.header.width);
    let height = f64::from(log.header.height);
    let cutoff = log.duration() - settings.ignore_last;
    log.clicks()
        .into_iter()
        .filter(|click| click.t < cutoff)
        .filter(|click| (0.0..width).contains(&click.x) && (0.0..height).contains(&click.y))
        .collect()
}

/// Time ranges to be zoomed in, merged where they nearly touch.
fn zoom_windows(clicks: &[Click], settings: &ZoomSettings) -> Vec<(f64, f64)> {
    let mut windows: Vec<(f64, f64)> = Vec::new();
    for click in clicks {
        let start = click.t - settings.lead;
        let end = click.t + settings.hold;
        match windows.last_mut() {
            Some(last) if start - last.1 <= settings.merge_gap => last.1 = last.1.max(end),
            _ => windows.push((start, end)),
        }
    }
    windows
}

/// Move `focus` just enough that `aim` is within `half_w`/`half_h` of it.
fn follow_with_dead_zone(
    focus: (f64, f64),
    aim: (f64, f64),
    half_w: f64,
    half_h: f64,
) -> (f64, f64) {
    let follow = |focus: f64, aim: f64, half: f64| {
        if aim > focus + half {
            aim - half
        } else if aim < focus - half {
            aim + half
        } else {
            focus
        }
    };
    (
        follow(focus.0, aim.0, half_w),
        follow(focus.1, aim.1, half_h),
    )
}

/// A critically damped spring, stepped with its exact solution so any frame rate is stable.
#[derive(Debug, Clone, Copy)]
struct Spring {
    value: f64,
    velocity: f64,
    omega: f64,
}

impl Spring {
    fn new(value: f64, omega: f64) -> Self {
        Self {
            value,
            velocity: 0.0,
            omega,
        }
    }

    fn step(&mut self, target: f64, dt: f64) {
        let offset = self.value - target;
        let decay = (-self.omega * dt).exp();
        let k = self.velocity + self.omega * offset;
        self.value = target + (offset + k * dt) * decay;
        self.velocity = (self.velocity - self.omega * k * dt) * decay;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zoom::cursor_log::{Header, Sample, BUTTON_LEFT};

    fn log(samples: Vec<Sample>) -> CursorLog {
        CursorLog::from_samples(
            Header {
                version: 1,
                width: 1920,
                height: 1080,
                framerate: 30,
            },
            samples,
        )
    }

    fn sample(t: f64, x: i32, y: i32, buttons: u8) -> Sample {
        Sample { t, x, y, buttons }
    }

    fn settings() -> ZoomSettings {
        ZoomSettings {
            ignore_last: 0.0,
            ..ZoomSettings::default()
        }
    }

    fn run(log: &CursorLog, settings: &ZoomSettings, until: f64) -> Vec<(f64, Camera)> {
        let mut planner = CameraPlanner::new(log, settings);
        (0..=(until * 30.0) as usize)
            .map(|frame| {
                let t = frame as f64 / 30.0;
                (t, planner.next(t))
            })
            .collect()
    }

    #[test]
    fn spring_settles_without_overshoot() {
        let mut spring = Spring::new(0.0, ZOOM_OMEGA);
        let mut previous = 0.0;
        for _ in 0..60 {
            spring.step(1.0, 1.0 / 30.0);
            assert!(spring.value >= previous && spring.value <= 1.0);
            previous = spring.value;
        }
        assert!((spring.value - 1.0).abs() < 1e-3);
    }

    #[test]
    fn merges_nearby_clicks() {
        let click = |t| Click { t, x: 0.0, y: 0.0 };
        let settings = ZoomSettings::default();
        let windows = zoom_windows(&[click(1.0), click(2.0), click(10.0)], &settings);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0], (1.0 - settings.lead, 2.0 + settings.hold));
    }

    #[test]
    fn dead_zone_moves_focus_minimally() {
        assert_eq!(
            follow_with_dead_zone((100.0, 100.0), (120.0, 90.0), 50.0, 50.0),
            (100.0, 100.0)
        );
        assert_eq!(
            follow_with_dead_zone((100.0, 100.0), (200.0, 0.0), 50.0, 50.0),
            (150.0, 50.0)
        );
    }

    #[test]
    fn stays_wide_without_clicks() {
        let log = log(vec![sample(0.0, 10, 10, 0), sample(3.0, 1500, 900, 0)]);
        for (_, camera) in run(&log, &settings(), 4.0) {
            assert_eq!(camera.scale, 1.0);
            assert_eq!((camera.cx, camera.cy), (960.0, 540.0));
        }
    }

    #[test]
    fn zooms_toward_click_then_back_out() {
        let log = log(vec![
            sample(0.0, 600, 400, 0),
            sample(2.0, 600, 400, BUTTON_LEFT),
            sample(2.1, 600, 400, 0),
            sample(8.0, 600, 400, 0),
        ]);
        let settings = settings();
        let frames = run(&log, &settings, 8.0);
        let at = |t: f64| frames.iter().find(|(ft, _)| *ft >= t).unwrap().1;

        assert_eq!(at(1.0).scale, 1.0);
        let zoomed = at(2.0 + settings.hold - 0.1);
        assert!((zoomed.scale - settings.max_scale).abs() < 0.01);
        assert!((zoomed.cx - 600.0).abs() < 5.0 && (zoomed.cy - 400.0).abs() < 5.0);
        assert!((at(8.0).scale - 1.0).abs() < 0.01);
    }

    #[test]
    fn never_shows_outside_the_frame() {
        let log = log(vec![
            sample(0.0, 0, 0, 0),
            sample(1.0, 0, 0, BUTTON_LEFT),
            sample(1.1, 1919, 1079, BUTTON_LEFT),
            sample(1.2, 1919, 1079, 0),
        ]);
        for (_, camera) in run(&log, &settings(), 5.0) {
            let (x, y) = camera.origin(1920.0, 1080.0);
            assert!(x >= -1e-9 && y >= -1e-9);
            assert!(x + 1920.0 / camera.scale <= 1920.0 + 1e-9);
            assert!(y + 1080.0 / camera.scale <= 1080.0 + 1e-9);
        }
    }

    #[test]
    fn ignores_clicks_right_before_stop() {
        let log = log(vec![
            sample(0.0, 600, 400, 0),
            sample(9.0, 1900, 5, BUTTON_LEFT),
            sample(10.0, 1900, 5, 0),
        ]);
        let settings = ZoomSettings::default();
        assert!(CameraPlanner::new(&log, &settings).clicks().is_empty());
    }
}
