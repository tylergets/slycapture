//! Per-recording setup: output name, video/audio options and (for windows) the window size.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib::{self, ControlFlow};
use gtk::prelude::*;

use crate::capture::audio::{self, AudioMeter};
use crate::capture::options::{sanitize_base_name, Format, Quality, RecordingOptions};
use crate::capture::Target;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizePreset {
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
}

pub const PRESETS: &[SizePreset] = &[
    SizePreset {
        name: "480p",
        width: 854,
        height: 480,
    },
    SizePreset {
        name: "720p",
        width: 1280,
        height: 720,
    },
    SizePreset {
        name: "1080p",
        width: 1920,
        height: 1080,
    },
    SizePreset {
        name: "1440p",
        width: 2560,
        height: 1440,
    },
    SizePreset {
        name: "4K",
        width: 3840,
        height: 2160,
    },
    SizePreset {
        name: "Square",
        width: 1080,
        height: 1080,
    },
    SizePreset {
        name: "Vertical",
        width: 1080,
        height: 1920,
    },
];

const DEFAULT_PRESET: &str = "1080p";
const MIN_DIMENSION: f64 = 64.0;
const DEFAULT_SOURCE: &str = "default";
const METER_REFRESH: Duration = Duration::from_millis(50);
const SIZE_FULL_SCREEN: &str = "full";
const SIZE_CUSTOM: &str = "custom";

/// Presets that fit on a `screen_width` x `screen_height` screen.
pub fn presets_for_screen(screen_width: u32, screen_height: u32) -> Vec<SizePreset> {
    PRESETS
        .iter()
        .copied()
        .filter(|preset| preset.width <= screen_width && preset.height <= screen_height)
        .collect()
}

/// Context the dialog needs besides the previous options.
pub struct SetupContext {
    pub target: Target,
    pub ffmpeg: String,
    pub screen: (u32, u32),
    /// Current size of the window being recorded, for window recordings.
    pub window_size: Option<(u32, u32)>,
    /// Size of the selected region, shown for region recordings.
    pub region_size: Option<(u32, u32)>,
}

/// Show the setup dialog. Returns `None` if it is cancelled.
pub fn run(context: &SetupContext, initial: &RecordingOptions) -> Option<RecordingOptions> {
    let title = match context.target {
        Target::Screen => "Record Entire Screen",
        Target::Window => "Record Window",
        Target::Region => "Record Region",
    };
    let dialog = gtk::Dialog::with_buttons(
        Some(&format!("slycapture: {title}")),
        None::<&gtk::Window>,
        gtk::DialogFlags::MODAL,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Record", gtk::ResponseType::Accept),
        ],
    );
    dialog.set_default_response(gtk::ResponseType::Accept);
    dialog.set_position(gtk::WindowPosition::Center);
    dialog.set_keep_above(true);
    dialog.set_resizable(false);

    let grid = gtk::Grid::new();
    grid.set_row_spacing(8);
    grid.set_column_spacing(12);
    grid.set_margin(14);
    dialog.content_area().pack_start(&grid, true, true, 0);
    let mut row = 0;
    let mut add_row = |label: &str, widget: &gtk::Widget| {
        let label = gtk::Label::new(Some(label));
        label.set_xalign(1.0);
        grid.attach(&label, 0, row, 1, 1);
        grid.attach(widget, 1, row, 1, 1);
        row += 1;
    };

    // Output name.
    let name_entry = gtk::Entry::new();
    name_entry.set_text(&initial.base_name);
    name_entry.set_activates_default(true);
    name_entry.set_width_chars(28);
    add_row("Name", name_entry.upcast_ref());

    // Size (window recordings) or a read-only size (region recordings).
    let size_widgets = (context.target == Target::Window)
        .then(|| size_row(context, initial))
        .inspect(|widgets| add_row("Window size", widgets.row.upcast_ref()));
    if let (Target::Region, Some((width, height))) = (context.target, context.region_size) {
        let label = gtk::Label::new(Some(&format!("{width}x{height}")));
        label.set_xalign(0.0);
        add_row("Region", label.upcast_ref());
    }

    // Video.
    let framerate = gtk::SpinButton::with_range(1.0, 240.0, 1.0);
    framerate.set_value(f64::from(initial.framerate));
    add_row("Frame rate", framerate.upcast_ref());

    let format = gtk::ComboBoxText::new();
    for value in Format::ALL {
        format.append(Some(value.extension()), value.extension());
    }
    format.set_active_id(Some(initial.format.extension()));
    add_row("Format", format.upcast_ref());

    let quality = gtk::ComboBoxText::new();
    for (index, value) in Quality::ALL.iter().enumerate() {
        quality.append(Some(&index.to_string()), value.label());
    }
    let quality_index = Quality::ALL
        .iter()
        .position(|q| *q == initial.quality)
        .unwrap_or(0);
    quality.set_active_id(Some(&quality_index.to_string()));
    add_row("Quality", quality.upcast_ref());

    let compress = gtk::CheckButton::with_label("Compress after recording");
    compress.set_active(initial.compress);
    compress.set_tooltip_text(Some(
        "Capture losslessly, then re-encode at the chosen quality when recording stops",
    ));
    add_row("", compress.upcast_ref());

    let keep_capture = gtk::CheckButton::with_label("Keep original .capture.mkv");
    keep_capture.set_active(initial.keep_capture);
    add_row("", keep_capture.upcast_ref());

    let draw_mouse = gtk::CheckButton::with_label("Show mouse cursor");
    draw_mouse.set_active(initial.draw_mouse);
    add_row("", draw_mouse.upcast_ref());

    let countdown = gtk::SpinButton::with_range(0.0, 10.0, 1.0);
    countdown.set_value(f64::from(initial.countdown));
    countdown.set_tooltip_text(Some("Seconds to wait before recording starts"));
    add_row("Countdown", countdown.upcast_ref());

    let zoom = gtk::CheckButton::with_label("Zoom in on clicks and follow the mouse");
    zoom.set_active(initial.zoom);
    zoom.set_tooltip_text(Some(
        "Log the pointer while recording, then render a zoomed version (re-encodes the recording)",
    ));
    add_row("Zoom", zoom.upcast_ref());

    // Intro title card.
    let intro = gtk::CheckButton::with_label("Add title card intro");
    intro.set_active(initial.intro);
    intro.set_tooltip_text(Some(
        "Prepend a black title card that fades in (re-encodes the recording)",
    ));
    add_row("Intro", intro.upcast_ref());

    let intro_title = gtk::Entry::new();
    intro_title.set_text(&initial.intro_title);
    intro_title.set_placeholder_text(Some("Title"));
    intro_title.set_activates_default(true);
    add_row("Intro title", intro_title.upcast_ref());

    let intro_logo = gtk::FileChooserButton::new("Intro logo", gtk::FileChooserAction::Open);
    let images = gtk::FileFilter::new();
    images.set_name(Some("Images"));
    images.add_mime_type("image/png");
    images.add_mime_type("image/jpeg");
    images.add_mime_type("image/bmp");
    images.add_mime_type("image/webp");
    intro_logo.add_filter(images);
    if let Some(logo) = &initial.intro_logo {
        intro_logo.set_filename(logo);
    }
    let clear_logo = gtk::Button::from_icon_name(Some("edit-clear"), gtk::IconSize::Button);
    clear_logo.set_tooltip_text(Some("No logo"));
    {
        let intro_logo = intro_logo.clone();
        clear_logo.connect_clicked(move |_| intro_logo.unselect_all());
    }
    let logo_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    logo_row.pack_start(&intro_logo, true, true, 0);
    logo_row.pack_start(&clear_logo, false, false, 0);
    add_row("Intro logo", logo_row.upcast_ref());

    let set_intro_sensitive = {
        let intro_title = intro_title.clone();
        let logo_row = logo_row.clone();
        move |intro: &gtk::CheckButton| {
            intro_title.set_sensitive(intro.is_active());
            logo_row.set_sensitive(intro.is_active());
        }
    };
    set_intro_sensitive(&intro);
    intro.connect_toggled(set_intro_sensitive);

    // Audio.
    let mic_enabled = gtk::CheckButton::with_label("Record microphone");
    mic_enabled.set_active(initial.microphone.is_some());
    add_row("Microphone", mic_enabled.upcast_ref());

    let mic_source = gtk::ComboBoxText::new();
    mic_source.append(Some(DEFAULT_SOURCE), "Default input");
    let mut sources = audio::list_sources();
    sources.sort_by_key(|source| source.is_monitor);
    for source in &sources {
        let label = if source.is_monitor {
            format!("{} (output monitor)", source.description)
        } else {
            source.description.clone()
        };
        mic_source.append(Some(&source.name), &label);
    }
    let initial_source = initial.microphone.as_deref().unwrap_or(DEFAULT_SOURCE);
    if !mic_source.set_active_id(Some(initial_source)) {
        mic_source.set_active_id(Some(DEFAULT_SOURCE));
    }
    add_row("Source", mic_source.upcast_ref());

    let meter = gtk::LevelBar::for_interval(0.0, 1.0);
    meter.set_size_request(-1, 10);
    meter.set_tooltip_text(Some("Microphone level"));
    add_row("Level", meter.upcast_ref());

    let system_audio = gtk::CheckButton::with_label("Record system audio (what you hear)");
    system_audio.set_active(initial.system_audio);
    add_row("System audio", system_audio.upcast_ref());

    let preview = gtk::Label::new(None);
    preview.set_xalign(0.0);
    preview.style_context().add_class("dim-label");
    add_row("File", preview.upcast_ref());

    // Live preview of the output file name.
    let update_preview = {
        let name_entry = name_entry.clone();
        let format = format.clone();
        let preview = preview.clone();
        move || {
            let extension = format
                .active_id()
                .map(|id| id.to_string())
                .unwrap_or_default();
            preview.set_text(&format!(
                "{}-<timestamp>.{extension}",
                sanitize_base_name(&name_entry.text())
            ));
        }
    };
    update_preview();
    {
        let update_preview = update_preview.clone();
        name_entry.connect_changed(move |_| update_preview());
    }
    format.connect_changed(move |_| update_preview());

    // Microphone meter: runs while the microphone is enabled, follows the selected source.
    let audio_meter: Rc<RefCell<Option<AudioMeter>>> = Rc::new(RefCell::new(None));
    let restart_meter = {
        let audio_meter = audio_meter.clone();
        let mic_enabled = mic_enabled.clone();
        let mic_source = mic_source.clone();
        let meter = meter.clone();
        let ffmpeg = context.ffmpeg.clone();
        move || {
            audio_meter.borrow_mut().take();
            let enabled = mic_enabled.is_active();
            mic_source.set_sensitive(enabled);
            meter.set_sensitive(enabled);
            meter.set_value(0.0);
            if !enabled {
                return;
            }
            let source = mic_source
                .active_id()
                .map(|id| id.to_string())
                .unwrap_or_else(|| DEFAULT_SOURCE.to_string());
            match AudioMeter::start(&ffmpeg, &source) {
                Ok(started) => *audio_meter.borrow_mut() = Some(started),
                Err(err) => eprintln!("warning: microphone meter unavailable: {err:#}"),
            }
        }
    };
    restart_meter();
    {
        let restart_meter = restart_meter.clone();
        mic_enabled.connect_toggled(move |_| restart_meter());
    }
    mic_source.connect_changed(move |_| restart_meter());
    let meter_source = {
        let audio_meter = audio_meter.clone();
        let meter = meter.clone();
        glib::timeout_add_local(METER_REFRESH, move || {
            if let Some(audio_meter) = audio_meter.borrow().as_ref() {
                meter.set_value(f64::from(audio_meter.level()));
            }
            ControlFlow::Continue
        })
    };

    dialog.show_all();
    let response = dialog.run();
    meter_source.remove();
    audio_meter.borrow_mut().take();

    let options = (response == gtk::ResponseType::Accept).then(|| RecordingOptions {
        base_name: sanitize_base_name(&name_entry.text()),
        framerate: framerate.value_as_int().max(1) as u32,
        draw_mouse: draw_mouse.is_active(),
        format: match format.active_id().as_deref() {
            Some("mp4") => Format::Mp4,
            _ => Format::Mkv,
        },
        quality: quality
            .active_id()
            .and_then(|id| id.parse::<usize>().ok())
            .and_then(|index| Quality::ALL.get(index).copied())
            .unwrap_or_default(),
        countdown: countdown.value_as_int().max(0) as u32,
        microphone: mic_enabled.is_active().then(|| {
            mic_source
                .active_id()
                .map(|id| id.to_string())
                .unwrap_or_else(|| DEFAULT_SOURCE.to_string())
        }),
        system_audio: system_audio.is_active(),
        compress: compress.is_active(),
        keep_capture: keep_capture.is_active(),
        window_size: size_widgets
            .as_ref()
            .map(|widgets| widgets.selected(context.screen))
            .or(initial.window_size),
        intro: intro.is_active(),
        intro_title: intro_title.text().trim().to_string(),
        intro_logo: intro_logo.filename(),
        zoom: zoom.is_active(),
    });

    dialog.close();
    options
}

struct SizeWidgets {
    row: gtk::Box,
    combo: gtk::ComboBoxText,
    width: gtk::SpinButton,
    height: gtk::SpinButton,
}

impl SizeWidgets {
    fn selected(&self, screen: (u32, u32)) -> (u32, u32) {
        match self.combo.active_id().as_deref() {
            Some(SIZE_CUSTOM) => (
                self.width.value_as_int() as u32,
                self.height.value_as_int() as u32,
            ),
            Some(SIZE_FULL_SCREEN) | None => screen,
            Some(name) => PRESETS
                .iter()
                .find(|preset| preset.name == name)
                .map(|preset| (preset.width, preset.height))
                .unwrap_or(screen),
        }
    }
}

fn size_row(context: &SetupContext, initial: &RecordingOptions) -> SizeWidgets {
    let (screen_width, screen_height) = context.screen;
    let combo = gtk::ComboBoxText::new();
    combo.append(
        Some(SIZE_FULL_SCREEN),
        &format!("Full screen ({screen_width}x{screen_height})"),
    );
    let presets = presets_for_screen(screen_width, screen_height);
    for preset in &presets {
        combo.append(
            Some(preset.name),
            &format!("{} ({}x{})", preset.name, preset.width, preset.height),
        );
    }
    combo.append(Some(SIZE_CUSTOM), "Custom");

    let (start_width, start_height) = initial
        .window_size
        .or(context.window_size)
        .unwrap_or((1280, 720));
    let width = dimension_spin(screen_width, start_width);
    let height = dimension_spin(screen_height, start_height);

    // Select the previous size's preset if there is one, else 1080p, else custom.
    let initial_id = match initial.window_size {
        Some(size) if size == context.screen => SIZE_FULL_SCREEN,
        Some((w, h)) => presets
            .iter()
            .find(|preset| (preset.width, preset.height) == (w, h))
            .map(|preset| preset.name)
            .unwrap_or(SIZE_CUSTOM),
        None if presets.iter().any(|preset| preset.name == DEFAULT_PRESET) => DEFAULT_PRESET,
        None => SIZE_FULL_SCREEN,
    };
    combo.set_active_id(Some(initial_id));

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.pack_start(&combo, false, false, 0);
    row.pack_start(&width, false, false, 0);
    row.pack_start(&gtk::Label::new(Some("x")), false, false, 0);
    row.pack_start(&height, false, false, 0);

    let set_custom_sensitive = {
        let width = width.clone();
        let height = height.clone();
        move |combo: &gtk::ComboBoxText| {
            let custom = combo.active_id().as_deref() == Some(SIZE_CUSTOM);
            width.set_sensitive(custom);
            height.set_sensitive(custom);
        }
    };
    set_custom_sensitive(&combo);
    combo.connect_changed(set_custom_sensitive);

    SizeWidgets {
        row,
        combo,
        width,
        height,
    }
}

fn dimension_spin(max: u32, value: u32) -> gtk::SpinButton {
    let spin = gtk::SpinButton::with_range(MIN_DIMENSION, f64::from(max), 2.0);
    spin.set_value(f64::from(value.min(max)));
    spin.set_numeric(true);
    spin
}

#[cfg(test)]
mod tests {
    use super::presets_for_screen;

    #[test]
    fn filters_presets_larger_than_screen() {
        let names: Vec<_> = presets_for_screen(1920, 1080)
            .iter()
            .map(|preset| preset.name)
            .collect();
        assert_eq!(names, vec!["480p", "720p", "1080p", "Square"]);
    }

    #[test]
    fn includes_everything_on_large_screen() {
        assert_eq!(presets_for_screen(7680, 4320).len(), super::PRESETS.len());
    }
}
