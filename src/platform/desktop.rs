//! Desktop integration for finished recordings: open, reveal in the file manager, copy to the
//! clipboard and move to the trash.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use gtk::gio::prelude::*;
use gtk::{gdk, gio, glib};

const FILE_MANAGER_TIMEOUT: Duration = Duration::from_secs(3);
/// Clipboard target info values.
const TARGET_URI_LIST: u32 = 0;
const TARGET_GNOME_FILES: u32 = 1;
const TARGET_TEXT: u32 = 2;

pub fn open(path: &Path) -> Result<()> {
    Command::new("xdg-open")
        .arg(path)
        .spawn()
        .context("failed to launch xdg-open")?;
    Ok(())
}

/// Reveal `path` selected in the file manager (`org.freedesktop.FileManager1`), or open its
/// folder if no file manager implements that interface.
pub fn show_in_folder(path: &Path) -> Result<()> {
    let uri = gio::File::for_path(path).uri().to_string();
    let shown = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).and_then(|bus| {
        bus.call_sync(
            Some("org.freedesktop.FileManager1"),
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1",
            "ShowItems",
            Some(&(vec![uri], "").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            FILE_MANAGER_TIMEOUT.as_millis() as i32,
            gio::Cancellable::NONE,
        )
    });
    match shown {
        Ok(_) => Ok(()),
        Err(_) => open(path.parent().unwrap_or(path)),
    }
}

/// Put `path` on the clipboard as a file (for file managers, browsers and chat apps) and as
/// plain text (for terminals and editors). Must be called on the GTK thread; the clipboard is
/// served for as long as slycapture runs.
pub fn copy_to_clipboard(path: &Path) -> Result<()> {
    let display = gdk::Display::default().context("no display for the clipboard")?;
    let clipboard = gtk::Clipboard::default(&display).context("no clipboard")?;
    let uri = gio::File::for_path(path).uri().to_string();
    let text = path.display().to_string();
    let targets = [
        gtk::TargetEntry::new("text/uri-list", gtk::TargetFlags::empty(), TARGET_URI_LIST),
        gtk::TargetEntry::new(
            "x-special/gnome-copied-files",
            gtk::TargetFlags::empty(),
            TARGET_GNOME_FILES,
        ),
        gtk::TargetEntry::new("UTF8_STRING", gtk::TargetFlags::empty(), TARGET_TEXT),
        gtk::TargetEntry::new(
            "text/plain;charset=utf-8",
            gtk::TargetFlags::empty(),
            TARGET_TEXT,
        ),
        gtk::TargetEntry::new("text/plain", gtk::TargetFlags::empty(), TARGET_TEXT),
    ];
    let served = clipboard.set_with_data(&targets, move |_, selection, info| match info {
        TARGET_URI_LIST => {
            selection.set_uris(&[uri.as_str()]);
        }
        TARGET_GNOME_FILES => {
            selection.set(&selection.target(), 8, format!("copy\n{uri}").as_bytes());
        }
        _ => {
            selection.set_text(&text);
        }
    });
    if !served {
        anyhow::bail!("failed to take the clipboard");
    }
    Ok(())
}

/// Move every existing path in `paths` to the trash. Returns the paths that were trashed.
pub fn trash(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut trashed = Vec::new();
    for path in paths.iter().filter(|path| path.exists()) {
        gio::File::for_path(path)
            .trash(gio::Cancellable::NONE)
            .map_err(|err: glib::Error| anyhow::anyhow!("{err}"))
            .with_context(|| format!("failed to move {} to the trash", path.display()))?;
        trashed.push(path.clone());
    }
    Ok(trashed)
}

/// `1.4 GB`, `820 MB`, `12 KB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(12_345), "12.3 KB");
        assert_eq!(human_size(820_000_000), "820 MB");
        assert_eq!(human_size(1_400_000_000), "1.4 GB");
    }
}
