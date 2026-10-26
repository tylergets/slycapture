# slycapture

`slycapture` is an X11 screen recorder that lives in your system tray, built for i3.

## Features

- Tray applet (libappindicator, same as slyboard): red icon while recording;
  **middle-click stops** a recording (if your tray host forwards secondary activation)
- **Record Entire Screen**
- **Record Window**: click a window, pick a size (1080p, 720p, 4K, square, vertical, custom…),
  and slycapture moves it to its own i3 workspace as a borderless floating window of exactly
  that size. When recording ends the window goes back where it was.
- **Record Region**: drag out a rectangle on a dimmed snapshot of the screen
- Per-recording setup dialog: file name, frame rate, format (mkv/mp4), quality, cursor,
  countdown, microphone (with source picker and live level meter), system audio, intro card
- Crash-safe capture: recordings are written as `<name>-<timestamp>.capture.mkv` first, then
  compressed (or remuxed) into the final file in the background with progress in the menu
- Pauses dunst notifications while recording (restored afterwards)
- CLI control and live status for bars (polybar, eww, …)

## Requirements

- X11 session (i3 for window recording)
- `ffmpeg` with x11grab and PulseAudio support (`ffmpeg-full` in nixpkgs), `pactl`,
  `notify-send`; optional: `dunstctl`, `xdg-open`
- An AppIndicator/StatusNotifierItem tray host (eww `systray`, waybar, KDE, …)

The Nix package wraps the binary with all runtime tools on `PATH`.

## Quick Start (Nix)

```bash
nix develop
cargo run -- run
```

No config file is required; defaults are used if none is found.

## CLI

```bash
slycapture run                      # run the applet (default command)
slycapture start [screen|window|region] [--now]   # --now skips the setup dialog
slycapture stop                     # stop recording / cancel countdown
slycapture toggle [screen|window|region] [--now]
slycapture status [--format plain|json|polybar] [--follow]
slycapture validate-config
```

`status --follow` prints a line on every change and keeps retrying while the applet isn't
running (it reports `offline`), which makes it suitable as a bar module:

```ini
; polybar
[module/slycapture]
type = custom/script
exec = slycapture status --follow --format polybar
tail = true
click-left = slycapture toggle region
```

For eww, use `deflisten` with `--format json`; the JSON has `state`
(`offline|idle|preparing|countdown|recording`), `elapsed_secs`, `mic_level` (0–1),
`width`, `height`, `countdown`, `output` and `processing` (`[{name, progress}]`).

## Configuration

Config is YAML. Discovery order: `./slycapture.yaml`, then
`~/.config/slycapture/config.yaml`; override with `--config PATH`. All keys are optional.
`recording`, `audio`, `intro` and `zoom.enabled` values are the defaults pre-filled in the setup dialog.

```yaml
recording:
  output_dir: ~/Videos/slycapture   # default: XDG videos dir + /slycapture
  base_name: slycapture
  framerate: 30
  draw_mouse: true
  format: mkv          # mkv | mp4
  quality: high        # low | medium | high | lossless
  countdown: 0         # seconds, 0–60
  compress: true       # capture losslessly, then re-encode at `quality`
  keep_capture: true   # keep the .capture.mkv after post-processing
  ffmpeg: ffmpeg
audio:
  microphone: false
  microphone_source: null   # PulseAudio source name; null = default input
  system_audio: false       # record the default output's monitor
window:
  workspace: slycapture     # i3 workspace used for window recordings
  restore: true             # put the window back when recording ends
notifications:
  pause_dunst: true
intro:
  enabled: false
  title: ""
  logo: null                # image path, supports ~/
  duration: 3.0             # seconds, 1–10
  font: Sans
zoom:                       # zoom in on clicks and follow the pointer
  enabled: false
  max_scale: 1.8            # 1.0–4.0
  hold: 1.5                 # seconds to stay zoomed after a click
  lead: 0.35                # seconds to start zooming before a click
  merge_gap: 1.0            # clicks closer than this share one zoom
  dead_zone: 0.5            # fraction of the view the pointer moves in before panning
  click_effect: true        # ripple where each click happened
  ignore_last: 2.0          # ignore clicks this close to the end (the stop click)
  sync_offset: 0.0          # seconds; raise if zooms start late
```

With zoom on, the pointer is logged to `<name>.capture.cursor.jsonl` next to the capture
while recording, and a zoomed copy is saved as `<name>.zoom.<ext>` beside the normal file.

## Runtime Paths

- Control socket / instance lock: `${XDG_RUNTIME_DIR}/slycapture-<user>.sock`

## Home Manager Module

This flake exports `homeManagerModules.default`:

```nix
{
  imports = [inputs.slycapture.homeManagerModules.default];
  programs.slycapture = {
    enable = true;
    settings = {
      recording.format = "mp4";
      audio.microphone = true;
    };
  };
}
```

## Development

```bash
nix develop
cargo test
nix build
```
