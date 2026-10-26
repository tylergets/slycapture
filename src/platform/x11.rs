use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt, EventMask, GrabMode, GrabStatus, ImageFormat, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

use crate::capture::Region;

/// Glyph index of `XC_crosshair` in the standard X cursor font.
const XC_CROSSHAIR: u16 = 34;
const GRAB_RETRY_TIMEOUT: Duration = Duration::from_secs(1);
const LEFT_BUTTON: u8 = 1;

/// Size of the whole X screen (all monitors).
pub fn screen_size() -> Result<(u32, u32)> {
    let (conn, screen_num) = connect()?;
    let screen = &conn.setup().roots[screen_num];
    Ok((
        u32::from(screen.width_in_pixels),
        u32::from(screen.height_in_pixels),
    ))
}

/// An RGB (8 bits per channel, no padding) snapshot of the whole screen.
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// Snapshot the screen as it is displayed. Uses `GetImage` on the root window like x11grab,
/// so the result matches what gets recorded, including under a compositor.
pub fn screenshot() -> Result<Screenshot> {
    let (conn, screen_num) = connect()?;
    let screen = &conn.setup().roots[screen_num];
    let (width, height) = (screen.width_in_pixels, screen.height_in_pixels);
    let image = conn
        .get_image(ImageFormat::Z_PIXMAP, screen.root, 0, 0, width, height, !0)?
        .reply()?;

    let pixels = usize::from(width) * usize::from(height);
    if image.depth < 24 || image.data.len() < pixels * 4 {
        bail!("unsupported screen format (depth {})", image.depth);
    }
    // 24/32-bit TrueColor is BGRX in memory on little-endian servers.
    let mut rgb = Vec::with_capacity(pixels * 3);
    for bgrx in image.data.chunks_exact(4).take(pixels) {
        rgb.extend_from_slice(&[bgrx[2], bgrx[1], bgrx[0]]);
    }
    Ok(Screenshot {
        width: u32::from(width),
        height: u32::from(height),
        rgb,
    })
}

/// Absolute on-screen geometry of a window.
pub fn window_region(window: Window) -> Result<Region> {
    let (conn, screen_num) = connect()?;
    let root = conn.setup().roots[screen_num].root;
    let geometry = conn.get_geometry(window)?.reply()?;
    let origin = conn.translate_coordinates(window, root, 0, 0)?.reply()?;
    Ok(Region::new(
        i32::from(origin.dst_x),
        i32::from(origin.dst_y),
        u32::from(geometry.width),
        u32::from(geometry.height),
    ))
}

/// Show a crosshair and wait for the user to click a window.
///
/// Returns the client window that was clicked, or `None` if the user cancelled with any key
/// or a non-left mouse button.
pub fn select_window() -> Result<Option<Window>> {
    let (conn, screen_num) = connect()?;
    let root = conn.setup().roots[screen_num].root;

    let font = conn.generate_id()?;
    conn.open_font(font, b"cursor")?;
    let cursor = conn.generate_id()?;
    conn.create_glyph_cursor(
        cursor,
        font,
        font,
        XC_CROSSHAIR,
        XC_CROSSHAIR + 1,
        0,
        0,
        0,
        0xffff,
        0xffff,
        0xffff,
    )?;

    grab_input(&conn, root, cursor)?;
    let selected = wait_for_click(&conn);

    conn.ungrab_pointer(CURRENT_TIME)?;
    conn.ungrab_keyboard(CURRENT_TIME)?;
    conn.free_cursor(cursor)?;
    conn.close_font(font)?;
    conn.flush()?;

    match selected? {
        Some(frame) => Ok(Some(find_client_window(&conn, frame)?.unwrap_or(frame))),
        None => Ok(None),
    }
}

fn connect() -> Result<(RustConnection, usize)> {
    x11rb::connect(None).context("failed to connect to the X server; is DISPLAY set?")
}

/// Grab pointer and keyboard. Retries briefly because the tray menu may still hold a grab
/// right after the user clicks a menu item.
fn grab_input(conn: &RustConnection, root: Window, cursor: u32) -> Result<()> {
    let deadline = Instant::now() + GRAB_RETRY_TIMEOUT;
    loop {
        let pointer = conn
            .grab_pointer(
                false,
                root,
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                cursor,
                CURRENT_TIME,
            )?
            .reply()?
            .status;
        if pointer == GrabStatus::SUCCESS {
            let keyboard = conn
                .grab_keyboard(false, root, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?
                .reply()?
                .status;
            if keyboard == GrabStatus::SUCCESS {
                return Ok(());
            }
            conn.ungrab_pointer(CURRENT_TIME)?;
        }

        if Instant::now() >= deadline {
            bail!("could not grab the pointer/keyboard to select a window");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_click(conn: &RustConnection) -> Result<Option<Window>> {
    let mut pressed: Option<Window> = None;
    loop {
        match conn.wait_for_event()? {
            Event::ButtonPress(event) if event.detail == LEFT_BUTTON => {
                if event.child != NONE {
                    pressed = Some(event.child);
                }
            }
            Event::ButtonPress(_) | Event::KeyPress(_) => return Ok(None),
            // Wait for release so the click is not delivered to the window after ungrabbing.
            Event::ButtonRelease(event) if event.detail == LEFT_BUTTON && pressed.is_some() => {
                return Ok(pressed);
            }
            _ => {}
        }
    }
}

/// Window managers reparent clients into frames; the click lands on the frame. Find the
/// descendant that carries `WM_STATE`, which is the actual client window.
fn find_client_window(conn: &RustConnection, window: Window) -> Result<Option<Window>> {
    let wm_state = conn.intern_atom(false, b"WM_STATE")?.reply()?.atom;
    find_window_with_property(conn, window, wm_state)
}

fn find_window_with_property(
    conn: &RustConnection,
    window: Window,
    property: u32,
) -> Result<Option<Window>> {
    let reply = conn
        .get_property(false, window, property, AtomEnum::ANY, 0, 0)?
        .reply()?;
    if reply.type_ != NONE {
        return Ok(Some(window));
    }

    for child in conn.query_tree(window)?.reply()?.children {
        if let Some(found) = find_window_with_property(conn, child, property)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}
