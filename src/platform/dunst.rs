//! Pause dunst notifications while recording, so popups don't end up in the video.

use std::process::Command;

/// Restores dunst's previous pause state when dropped.
pub struct DndGuard {
    was_paused: bool,
}

/// Pause dunst if it is running. Returns `None` when dunst can't be reached.
pub fn pause() -> Option<DndGuard> {
    let output = Command::new("dunstctl").arg("is-paused").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let was_paused = String::from_utf8_lossy(&output.stdout).trim() == "true";
    if !was_paused {
        set_paused(true);
    }
    Some(DndGuard { was_paused })
}

impl Drop for DndGuard {
    fn drop(&mut self) {
        if !self.was_paused {
            set_paused(false);
        }
    }
}

fn set_paused(paused: bool) {
    let value = if paused { "true" } else { "false" };
    if let Err(err) = Command::new("dunstctl")
        .args(["set-paused", value])
        .status()
    {
        eprintln!("warning: failed to set dunst paused={value}: {err}");
    }
}
