//! Border around the area being recorded, shown during the countdown and while recording.
//!
//! The border is four thin override-redirect windows placed just outside the region, so it is
//! never part of the capture, works without a compositor, and i3 leaves it alone. Edges that
//! would fall off-screen (e.g. for a full-screen recording) are not shown.

use gtk::prelude::*;

use crate::capture::Region;
use crate::config::MarkerConfig;

/// Closes the border when dropped.
pub struct Marker {
    windows: Vec<gtk::Window>,
}

/// A rectangle in device pixels: `(x0, y0, x1, y1)`, end exclusive.
type Rect = (i32, i32, i32, i32);

impl Marker {
    /// Show a border around `region` on a `screen` sized X screen.
    pub fn show(region: Region, screen: (u32, u32), config: &MarkerConfig) -> Self {
        let (red, green, blue) = config.rgb().unwrap_or((0.9, 0.28, 0.3));
        let scale = gtk::gdk::Screen::default()
            .and_then(|screen| screen.display().monitor(0))
            .map(|monitor| monitor.scale_factor())
            .unwrap_or(1)
            .max(1);
        let windows = border_rects(region, screen, config.width as i32)
            .into_iter()
            .map(|(rect, outward)| {
                let window = strip_window(to_logical(rect, outward, scale));
                window.connect_draw(move |_, cr| {
                    cr.set_source_rgb(red, green, blue);
                    let _ = cr.paint();
                    gtk::glib::Propagation::Stop
                });
                window.show_all();
                // Let clicks through to whatever is underneath.
                window.input_shape_combine_region(Some(&gtk::cairo::Region::create()));
                window
            })
            .collect();
        Self { windows }
    }
}

impl Drop for Marker {
    fn drop(&mut self) {
        for window in &self.windows {
            window.close();
        }
    }
}

fn strip_window((x, y, width, height): (i32, i32, i32, i32)) -> gtk::Window {
    let window = gtk::Window::new(gtk::WindowType::Popup);
    window.set_app_paintable(true);
    window.set_accept_focus(false);
    window.move_(x, y);
    window.set_default_size(width, height);
    window.set_size_request(width, height);
    window
}

/// Which side of a strip faces away from the region, so logical rounding never moves a
/// strip into the captured area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outward {
    Up,
    Down,
    Left,
    Right,
}

/// The visible parts of a `thickness` wide border just outside `region`.
fn border_rects(
    region: Region,
    (width, height): (u32, u32),
    thickness: i32,
) -> Vec<(Rect, Outward)> {
    let (x0, y0) = (region.x, region.y);
    let (x1, y1) = (x0 + region.width as i32, y0 + region.height as i32);
    let t = thickness;
    [
        ((x0 - t, y0 - t, x1 + t, y0), Outward::Up),
        ((x0 - t, y1, x1 + t, y1 + t), Outward::Down),
        ((x0 - t, y0, x0, y1), Outward::Left),
        ((x1, y0, x1 + t, y1), Outward::Right),
    ]
    .into_iter()
    .filter_map(|((ax, ay, bx, by), outward)| {
        let clipped = (
            ax.max(0),
            ay.max(0),
            bx.min(width as i32),
            by.min(height as i32),
        );
        (clipped.0 < clipped.2 && clipped.1 < clipped.3).then_some((clipped, outward))
    })
    .collect()
}

/// Convert a device-pixel rect to logical `(x, y, width, height)`, rounding the edge that
/// touches the region away from it.
fn to_logical((x0, y0, x1, y1): Rect, outward: Outward, scale: i32) -> (i32, i32, i32, i32) {
    let floor = |value: i32| value.div_euclid(scale);
    let ceil = |value: i32| (value + scale - 1).div_euclid(scale);
    let (lx0, ly0, lx1, ly1) = match outward {
        Outward::Up => (floor(x0), floor(y0), ceil(x1), floor(y1)),
        Outward::Down => (floor(x0), ceil(y0), ceil(x1), ceil(y1)),
        Outward::Left => (floor(x0), floor(y0), floor(x1), ceil(y1)),
        Outward::Right => (ceil(x0), floor(y0), ceil(x1), ceil(y1)),
    };
    (lx0, ly0, (lx1 - lx0).max(1), (ly1 - ly0).max(1))
}

#[cfg(test)]
mod tests {
    use super::{border_rects, to_logical, Outward};
    use crate::capture::Region;

    #[test]
    fn border_surrounds_region() {
        let rects = border_rects(Region::new(100, 50, 640, 480), (1920, 1080), 3);
        assert_eq!(
            rects,
            vec![
                ((97, 47, 743, 50), Outward::Up),
                ((97, 530, 743, 533), Outward::Down),
                ((97, 50, 100, 530), Outward::Left),
                ((740, 50, 743, 530), Outward::Right),
            ]
        );
    }

    #[test]
    fn full_screen_has_no_border() {
        assert!(border_rects(Region::new(0, 0, 1920, 1080), (1920, 1080), 3).is_empty());
    }

    #[test]
    fn edges_at_screen_border_are_dropped() {
        let rects = border_rects(Region::new(0, 0, 800, 600), (1920, 1080), 3);
        let sides: Vec<_> = rects.iter().map(|(_, outward)| *outward).collect();
        assert_eq!(sides, [Outward::Down, Outward::Right]);
        assert_eq!(rects[0].0, (0, 600, 803, 603));
    }

    #[test]
    fn logical_rounding_stays_outside_region() {
        // Region starts at device y=51; at scale 2 the top strip must end at or above 25.5.
        assert_eq!(
            to_logical((97, 48, 743, 51), Outward::Up, 2),
            (48, 24, 324, 1)
        );
        assert_eq!(
            to_logical((97, 531, 743, 534), Outward::Down, 2),
            (48, 266, 324, 1)
        );
        assert_eq!(
            to_logical((10, 20, 30, 40), Outward::Left, 1),
            (10, 20, 20, 20)
        );
    }
}
