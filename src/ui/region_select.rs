//! Full-screen overlay for dragging out a recording region. Draws a frozen snapshot of the
//! screen with everything outside the selection dimmed.
//!
//! A snapshot (rather than a translucent window) behaves the same with or without a
//! compositor, and is taken the same way x11grab reads the screen.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::capture::Region;
use crate::platform::x11;

const DIM_ALPHA: f64 = 0.5;
const MIN_SIZE: f64 = 8.0;
const GRAB_ATTEMPTS: u32 = 20;
const GRAB_RETRY: Duration = Duration::from_millis(50);
/// Lets the compositor remove the overlay before anything starts capturing the screen.
const CLOSE_SETTLE: Duration = Duration::from_millis(150);

#[derive(Default)]
struct Drag {
    start: Option<(f64, f64)>,
    current: (f64, f64),
}

impl Drag {
    fn rect(&self) -> Option<(f64, f64, f64, f64)> {
        let (x0, y0) = self.start?;
        let (x1, y1) = self.current;
        Some((x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs()))
    }
}

type Done = Box<dyn FnOnce(Option<Region>)>;

/// Show the overlay and call `on_done` with the selected region, or `None` if cancelled
/// (Escape or right-click).
pub fn select_region(on_done: impl FnOnce(Option<Region>) + 'static) {
    let snapshot = match x11::screenshot() {
        Ok(shot) => gtk::gdk_pixbuf::Pixbuf::from_bytes(
            &glib::Bytes::from_owned(shot.rgb),
            gtk::gdk_pixbuf::Colorspace::Rgb,
            false,
            8,
            shot.width as i32,
            shot.height as i32,
            shot.width as i32 * 3,
        ),
        Err(err) => {
            eprintln!("error: failed to snapshot the screen: {err:#}");
            on_done(None);
            return;
        }
    };
    // The snapshot is in device pixels; the window is sized in logical pixels.
    let scale = gdk::Screen::default()
        .and_then(|screen| screen.display().monitor(0))
        .map(|monitor| monitor.scale_factor())
        .unwrap_or(1)
        .max(1);
    let (width, height) = (snapshot.width() / scale, snapshot.height() / scale);

    let window = gtk::Window::new(gtk::WindowType::Popup);
    window.set_app_paintable(true);
    window.move_(0, 0);
    window.set_default_size(width, height);
    window.set_size_request(width, height);

    let drag = Rc::new(RefCell::new(Drag::default()));
    let done: Rc<RefCell<Option<Done>>> = Rc::new(RefCell::new(Some(Box::new(on_done))));

    {
        let drag = drag.clone();
        window.connect_draw(move |widget, cr| {
            draw(widget, cr, &snapshot, &drag.borrow());
            glib::Propagation::Stop
        });
    }

    window.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::POINTER_MOTION_MASK
            | gdk::EventMask::KEY_PRESS_MASK,
    );

    {
        let drag = drag.clone();
        let done = done.clone();
        window.connect_button_press_event(move |window, event| {
            if event.button() == 1 {
                let position = event.position();
                *drag.borrow_mut() = Drag {
                    start: Some(position),
                    current: position,
                };
                window.queue_draw();
            } else {
                finish(window, &done, None);
            }
            glib::Propagation::Stop
        });
    }

    {
        let drag = drag.clone();
        window.connect_motion_notify_event(move |window, event| {
            drag.borrow_mut().current = event.position();
            window.queue_draw();
            glib::Propagation::Stop
        });
    }

    {
        let drag = drag.clone();
        let done = done.clone();
        window.connect_button_release_event(move |window, event| {
            if event.button() != 1 {
                return glib::Propagation::Stop;
            }
            drag.borrow_mut().current = event.position();
            let rect = drag.borrow().rect();
            match rect {
                Some((x, y, w, h)) if w >= MIN_SIZE && h >= MIN_SIZE => {
                    // Event coordinates are logical pixels; recording needs device pixels.
                    let scale = f64::from(window.scale_factor());
                    let region = Region::new(
                        (x * scale).round() as i32,
                        (y * scale).round() as i32,
                        (w * scale).round() as u32,
                        (h * scale).round() as u32,
                    );
                    finish(window, &done, Some(region));
                }
                // A click without a drag: start over.
                _ => {
                    *drag.borrow_mut() = Drag::default();
                    window.queue_draw();
                }
            }
            glib::Propagation::Stop
        });
    }

    {
        let done = done.clone();
        window.connect_key_press_event(move |window, event| {
            if event.keyval() == gdk::keys::constants::Escape {
                finish(window, &done, None);
            }
            glib::Propagation::Stop
        });
    }

    window.show_all();
    grab_input(window, done, GRAB_ATTEMPTS);
}

fn draw(
    widget: &gtk::Window,
    cr: &gtk::cairo::Context,
    snapshot: &gtk::gdk_pixbuf::Pixbuf,
    drag: &Drag,
) {
    let (width, height) = (
        f64::from(widget.allocated_width()),
        f64::from(widget.allocated_height()),
    );

    let scale = f64::from(widget.scale_factor());
    cr.save().ok();
    cr.scale(1.0 / scale, 1.0 / scale);
    cr.set_source_pixbuf(snapshot, 0.0, 0.0);
    let _ = cr.paint();
    cr.restore().ok();

    // Dim everything except the selection (even-odd leaves the inner rectangle untouched).
    cr.set_operator(gtk::cairo::Operator::Over);
    cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
    cr.rectangle(0.0, 0.0, width, height);
    if let Some((x, y, w, h)) = drag.rect() {
        cr.rectangle(x, y, w, h);
    }
    cr.set_source_rgba(0.0, 0.0, 0.0, DIM_ALPHA);
    let _ = cr.fill();

    let Some((x, y, w, h)) = drag.rect() else {
        return;
    };

    // Outline just outside the selection, so it is never part of the recording.
    cr.set_source_rgba(0.9, 0.28, 0.3, 1.0);
    cr.set_line_width(2.0);
    cr.rectangle(x - 1.0, y - 1.0, w + 2.0, h + 2.0);
    let _ = cr.stroke();

    let label = format!("{}x{}", (w * scale).round(), (h * scale).round());
    cr.set_font_size(14.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    let label_y = if y > 24.0 { y - 8.0 } else { y + h + 20.0 };
    cr.move_to(x, label_y);
    let _ = cr.show_text(&label);
}

/// Grab pointer and keyboard so clicks land on the overlay. Retries while the tray menu still
/// holds its own grab.
fn grab_input(window: gtk::Window, done: Rc<RefCell<Option<Done>>>, attempts_left: u32) {
    let display = window.display();
    let (Some(seat), Some(gdk_window)) = (display.default_seat(), window.window()) else {
        finish(&window, &done, None);
        return;
    };
    let cursor = gdk::Cursor::for_display(&display, gdk::CursorType::Crosshair);
    let status = seat.grab(
        &gdk_window,
        gdk::SeatCapabilities::ALL,
        true,
        cursor.as_ref(),
        None,
        None,
    );
    if status == gdk::GrabStatus::Success {
        return;
    }
    if attempts_left == 0 {
        eprintln!("error: could not grab input for region selection ({status:?})");
        finish(&window, &done, None);
        return;
    }
    glib::timeout_add_local_once(GRAB_RETRY, move || {
        grab_input(window, done, attempts_left - 1)
    });
}

fn finish(window: &gtk::Window, done: &Rc<RefCell<Option<Done>>>, region: Option<Region>) {
    let Some(done) = done.borrow_mut().take() else {
        return;
    };
    if let Some(seat) = window.display().default_seat() {
        seat.ungrab();
    }
    window.close();
    glib::timeout_add_local_once(CLOSE_SETTLE, move || done(region));
}
