use std::cell::{Cell, RefCell};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use gtk::glib::{self, ControlFlow, SourceId};

use crate::capture::encoder::{self, VideoEncoder};
use crate::capture::options::{sanitize_base_name, Format};
use crate::capture::transcode::{IntroSettings, JobState, TranscodeJob, TranscodeSettings};
use crate::capture::{Recording, RecordingOptions, Region, Target};
use crate::config::AppConfig;
use crate::core::control::{self, Action, ActionRequest};
use crate::core::saved_options;
use crate::core::status::{format_elapsed, JobStatus, State, Status};
use crate::platform::applet::{
    self, notify, notify_with_actions, Applet, IconState, MenuEntry, TraySpec,
};
use crate::platform::desktop;
use crate::platform::dunst::{self, DndGuard};
use crate::platform::i3::{self, WindowPlacement};
use crate::platform::x11;
use crate::ui::marker::Marker;
use crate::ui::region_select;
use crate::ui::setup_dialog::{self, SetupContext};
use crate::zoom::cursor_log::{self, CursorLogger};
use crate::zoom::zoomed_path;

const APP_ID: &str = "slycapture";
/// Time given to i3 to apply floating/resize before reading the window geometry.
const WINDOW_SETTLE_DELAY: Duration = Duration::from_millis(400);
/// Menu, status and job refresh rate.
const TICK_INTERVAL: Duration = Duration::from_millis(250);
const ACTION_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long the "Recording saved" notification (with its actions) stays up.
const SAVED_NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(config: AppConfig, control_listener: UnixListener) -> Result<()> {
    if std::env::var_os("DISPLAY").is_none() {
        bail!("DISPLAY is not set; slycapture requires an X11 session");
    }
    gtk::init()?;

    let (tray_tx, tray_rx) = mpsc::channel();
    let applet = Applet::new(APP_ID, tray_spec(&Status::default()), tray_tx)?;
    // Probe hardware encoders in the background so the first recording doesn't wait.
    {
        let (ffmpeg, choice, device) = (
            config.recording.ffmpeg.clone(),
            config.recording.encoder,
            config.recording.vaapi_device.clone(),
        );
        std::thread::spawn(move || encoder::resolve(&ffmpeg, choice, device.as_deref()));
    }
    let app = App::new(config, applet);

    let (actions_tx, actions_rx) = mpsc::channel();
    control::serve(control_listener, app.shared_status.clone(), actions_tx);
    app.listen_for_actions(actions_rx, tray_rx);

    app.tick();
    let weak = Rc::downgrade(&app);
    glib::timeout_add_local(TICK_INTERVAL, move || match weak.upgrade() {
        Some(app) => {
            app.tick();
            ControlFlow::Continue
        }
        None => ControlFlow::Break,
    });

    gtk::main();
    Ok(())
}

/// Tray menu and click actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayAction {
    Start(Target),
    Stop,
    OpenFolder,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Selecting a window/region or in the setup dialog.
    Preparing,
    Countdown(u32),
    Recording,
}

struct ActiveRecording {
    recording: Recording,
    target: Target,
    options: RecordingOptions,
    final_output: PathBuf,
    /// Captured frame size.
    size: (u32, u32),
    /// Pointer log for zooming, while recording with zoom on.
    cursor_logger: Option<CursorLogger>,
    /// Restores dunst when dropped.
    _dnd: Option<DndGuard>,
    /// Where a recorded window came from, restored when recording ends.
    placement: Option<WindowPlacement>,
}

struct PendingStart {
    target: Target,
    region: Region,
    options: RecordingOptions,
    placement: Option<WindowPlacement>,
}

struct App {
    config: AppConfig,
    applet: RefCell<Applet<TrayAction>>,
    phase: Cell<Phase>,
    recording: RefCell<Option<ActiveRecording>>,
    countdown: RefCell<Option<(SourceId, PendingStart)>>,
    jobs: RefCell<Vec<TranscodeJob>>,
    /// Border around the area being recorded, during the countdown and recording.
    marker: RefCell<Option<Marker>>,
    /// Options from the last setup dialog (saved across restarts), offered next time.
    last_options: RefCell<RecordingOptions>,
    shared_status: Arc<Mutex<Status>>,
}

impl App {
    fn new(config: AppConfig, applet: Applet<TrayAction>) -> Rc<Self> {
        Rc::new(Self {
            last_options: RefCell::new(saved_options::load(config.default_options())),
            config,
            applet: RefCell::new(applet),
            phase: Cell::new(Phase::Idle),
            recording: RefCell::new(None),
            countdown: RefCell::new(None),
            jobs: RefCell::new(Vec::new()),
            marker: RefCell::new(None),
            shared_status: Arc::new(Mutex::new(Status::default())),
        })
    }

    /// Handle tray clicks and control-socket actions on the GTK thread. Socket replies are
    /// sent before long-running work (dialogs, selection) so CLI callers return immediately.
    fn listen_for_actions(
        self: &Rc<Self>,
        actions: Receiver<ActionRequest>,
        tray_actions: Receiver<TrayAction>,
    ) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(ACTION_POLL_INTERVAL, move || {
            let Some(app) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            while let Ok(action) = tray_actions.try_recv() {
                // Defer: menu callbacks may open dialogs with their own main loop.
                let app = app.clone();
                glib::idle_add_local_once(move || match action {
                    TrayAction::Start(target) => {
                        if app.phase.get() == Phase::Idle {
                            let result = app.start(target, false);
                            app.report(result);
                        }
                    }
                    TrayAction::Stop => app.stop(),
                    TrayAction::OpenFolder => app.open_folder(),
                    TrayAction::Quit => app.quit(),
                });
            }
            while let Ok((action, reply)) = actions.try_recv() {
                let (target, skip_dialog) = match action {
                    Action::Stop => {
                        app.stop();
                        let _ = reply.send(Ok(()));
                        continue;
                    }
                    Action::Toggle { .. } if app.phase.get() != Phase::Idle => {
                        app.stop();
                        let _ = reply.send(Ok(()));
                        continue;
                    }
                    Action::Start {
                        target,
                        skip_dialog,
                    }
                    | Action::Toggle {
                        target,
                        skip_dialog,
                    } => (target, skip_dialog),
                };
                if app.phase.get() != Phase::Idle {
                    let _ = reply.send(Err(anyhow::anyhow!("already recording or preparing")));
                    continue;
                }
                let _ = reply.send(Ok(()));
                let app = app.clone();
                glib::idle_add_local_once(move || {
                    let result = app.start(target, skip_dialog);
                    app.report(result);
                });
            }
            ControlFlow::Continue
        });
    }

    fn set_phase(&self, phase: Phase) {
        if phase == Phase::Idle {
            self.marker.borrow_mut().take();
        }
        self.phase.set(phase);
        self.tick();
    }

    // ---- Starting -------------------------------------------------------------------------

    fn start(self: &Rc<Self>, target: Target, skip_dialog: bool) -> Result<()> {
        if self.phase.get() != Phase::Idle {
            bail!("already recording or preparing");
        }
        self.set_phase(Phase::Preparing);

        let result = match target {
            Target::Screen => self.start_screen(skip_dialog),
            Target::Window => {
                self.start_window(skip_dialog);
                Ok(())
            }
            Target::Region => {
                self.start_region(skip_dialog);
                Ok(())
            }
        };
        if result.is_err() {
            self.set_phase(Phase::Idle);
        }
        result
    }

    fn start_screen(self: &Rc<Self>, skip_dialog: bool) -> Result<()> {
        let (width, height) = x11::screen_size()?;
        let Some(options) = self.choose_options(Target::Screen, skip_dialog, None, None)? else {
            self.set_phase(Phase::Idle);
            return Ok(());
        };
        self.begin(
            Target::Screen,
            Region::new(0, 0, width, height),
            options,
            None,
        );
        Ok(())
    }

    fn start_window(self: &Rc<Self>, skip_dialog: bool) {
        let app = self.clone();
        applet::spawn_blocking(x11::select_window, move |selection| {
            let result = selection.and_then(|window| match window {
                Some(window) => app.stage_window(window, skip_dialog),
                None => Ok(false),
            });
            if !matches!(result, Ok(true)) {
                app.set_phase(Phase::Idle);
            }
            app.report(result.map(|_| ()));
        });
    }

    /// Ask for options, hand the window to i3, then begin once it has settled.
    /// Returns `Ok(false)` if the user cancelled.
    fn stage_window(self: &Rc<Self>, window: u32, skip_dialog: bool) -> Result<bool> {
        let current = x11::window_region(window)
            .ok()
            .map(|region| (region.width, region.height));
        let Some(options) = self.choose_options(Target::Window, skip_dialog, current, None)? else {
            return Ok(false);
        };
        let (width, height) = options
            .window_size
            .or(current)
            .context("could not determine the window size")?;

        let placement =
            i3::stage_window_for_recording(window, width, height, &self.config.window.workspace)?
                .filter(|_| self.config.window.restore);

        let app = self.clone();
        glib::timeout_add_local_once(WINDOW_SETTLE_DELAY, move || {
            match x11::window_region(window) {
                Ok(region) => app.begin(Target::Window, region, options, placement),
                Err(err) => {
                    restore_placement(placement);
                    app.set_phase(Phase::Idle);
                    app.report(Err(err));
                }
            }
        });
        Ok(true)
    }

    fn start_region(self: &Rc<Self>, skip_dialog: bool) {
        let app = self.clone();
        region_select::select_region(move |region| {
            let Some(region) = region else {
                app.set_phase(Phase::Idle);
                return;
            };
            let size = Some((region.width, region.height));
            match app.choose_options(Target::Region, skip_dialog, None, size) {
                Ok(Some(options)) => app.begin(Target::Region, region, options, None),
                Ok(None) => app.set_phase(Phase::Idle),
                Err(err) => {
                    app.set_phase(Phase::Idle);
                    app.report(Err(err));
                }
            }
        });
    }

    /// Show the setup dialog (unless skipped) and remember the result.
    fn choose_options(
        &self,
        target: Target,
        skip_dialog: bool,
        window_size: Option<(u32, u32)>,
        region_size: Option<(u32, u32)>,
    ) -> Result<Option<RecordingOptions>> {
        let initial = self.last_options.borrow().clone();
        if skip_dialog {
            return Ok(Some(initial));
        }
        let context = SetupContext {
            target,
            ffmpeg: self.config.recording.ffmpeg.clone(),
            screen: x11::screen_size()?,
            window_size,
            region_size,
        };
        let options = setup_dialog::run(&context, &initial);
        if let Some(options) = &options {
            if let Err(err) = saved_options::save(options) {
                eprintln!("warning: failed to save recording options: {err:#}");
            }
            *self.last_options.borrow_mut() = options.clone();
        }
        Ok(options)
    }

    /// Start now, or after the configured countdown.
    fn begin(
        self: &Rc<Self>,
        target: Target,
        region: Region,
        options: RecordingOptions,
        placement: Option<WindowPlacement>,
    ) {
        self.show_marker(region);
        let pending = PendingStart {
            target,
            region,
            options,
            placement,
        };
        if pending.options.countdown == 0 {
            self.record(pending);
            return;
        }

        let seconds = pending.options.countdown;
        notify("Recording soon", &format!("Recording starts in {seconds}s"));
        self.set_phase(Phase::Countdown(seconds));
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(app) = weak.upgrade() else {
                return ControlFlow::Break;
            };
            match app.phase.get() {
                Phase::Countdown(remaining) if remaining > 1 => {
                    app.set_phase(Phase::Countdown(remaining - 1));
                    ControlFlow::Continue
                }
                _ => {
                    // Take first: `record` re-borrows `countdown` while refreshing.
                    let pending = app.countdown.borrow_mut().take();
                    if let Some((_, pending)) = pending {
                        app.record(pending);
                    }
                    ControlFlow::Break
                }
            }
        });
        *self.countdown.borrow_mut() = Some((source, pending));
    }

    fn show_marker(&self, region: Region) {
        let marker_config = &self.config.marker;
        if !marker_config.enabled {
            return;
        }
        let Ok(screen) = x11::screen_size() else {
            return;
        };
        if let Some(region) = region.fit_for_encoding(screen.0, screen.1) {
            *self.marker.borrow_mut() = Some(Marker::show(region, screen, marker_config));
        }
    }

    fn encoder(&self) -> VideoEncoder {
        let recording = &self.config.recording;
        encoder::resolve(
            &recording.ffmpeg,
            recording.encoder,
            recording.vaapi_device.as_deref(),
        )
    }

    fn record(&self, pending: PendingStart) {
        let placement = pending.placement.clone();
        let result = self.try_record(pending);
        if result.is_err() {
            // Put a staged window back if the recording never started.
            restore_placement(placement);
            self.set_phase(Phase::Idle);
        }
        self.report(result);
    }

    fn try_record(&self, pending: PendingStart) -> Result<()> {
        let PendingStart {
            target,
            region,
            options,
            placement,
        } = pending;
        let (screen_width, screen_height) = x11::screen_size()?;
        let region = region
            .fit_for_encoding(screen_width, screen_height)
            .context("selected area is not visible on screen")?;
        let (capture, final_output) = output_paths(
            &self.config.recording.resolved_output_dir()?,
            &options.base_name,
            options.format,
        );
        let display = std::env::var("DISPLAY").context("DISPLAY is not set")?;

        let dnd = self
            .config
            .notifications
            .pause_dunst
            .then(dunst::pause)
            .flatten();
        // Started just before ffmpeg so the log's clock lines up with the video's.
        let cursor_logger = options
            .zoom
            .then(|| {
                CursorLogger::start(region, options.framerate, cursor_log::sidecar_path(&capture))
            })
            .transpose()?;
        let recording = Recording::start(
            &self.config.recording.ffmpeg,
            &display,
            region,
            &options,
            &self.encoder(),
            capture,
        )?;
        println!(
            "recording {}x{}+{},{} to {}",
            region.width,
            region.height,
            region.x,
            region.y,
            recording.output().display()
        );
        *self.recording.borrow_mut() = Some(ActiveRecording {
            recording,
            target,
            options,
            final_output,
            size: (region.width, region.height),
            cursor_logger,
            _dnd: dnd,
            placement,
        });
        self.set_phase(Phase::Recording);
        Ok(())
    }

    // ---- Stopping -------------------------------------------------------------------------

    fn stop(self: &Rc<Self>) {
        let countdown = self.countdown.borrow_mut().take();
        if let Some((source, pending)) = countdown {
            source.remove();
            restore_placement(pending.placement);
            self.set_phase(Phase::Idle);
            return;
        }
        let Some(active) = self.recording.borrow_mut().take() else {
            return;
        };
        self.set_phase(Phase::Idle);

        let ActiveRecording {
            recording,
            options,
            final_output,
            size,
            cursor_logger,
            _dnd,
            placement,
            ..
        } = active;
        let duration = recording.elapsed();
        let result = recording.stop();
        let zoom = match cursor_logger.map(CursorLogger::stop) {
            Some(Ok(())) => true,
            Some(Err(err)) => {
                eprintln!("warning: cursor log failed, skipping zoom: {err:#}");
                false
            }
            None => false,
        };
        drop(_dnd);
        restore_placement(placement);

        match result {
            Ok(capture) => {
                self.post_process(capture, final_output, duration, size, zoom, &options)
            }
            Err(err) => self.report(Err(err)),
        }
    }

    fn post_process(
        &self,
        capture: PathBuf,
        output: PathBuf,
        duration: Duration,
        (width, height): (u32, u32),
        zoom: bool,
        options: &RecordingOptions,
    ) {
        println!("saved capture to {}", capture.display());
        let intro = options.intro.then(|| IntroSettings {
            title: options.intro_title.clone(),
            logo: options.intro_logo.clone(),
            font: self.config.intro.font.clone(),
            duration: Duration::from_secs_f64(self.config.intro.duration),
            width,
            height,
            framerate: options.framerate,
            audio: options.has_audio(),
        });
        // Uncompressed Matroska needs no second pass: the capture is the final file.
        let zoom = zoom.then(|| self.config.zoom.clone());
        if !options.compress && options.format == Format::Mkv && intro.is_none() && zoom.is_none()
        {
            let result = std::fs::rename(&capture, &output)
                .with_context(|| format!("failed to rename {}", capture.display()));
            match result {
                Ok(()) => notify_saved(output, None),
                Err(err) => self.report(Err(err)),
            }
            return;
        }

        let job = TranscodeJob::start(
            &self.config.recording.ffmpeg,
            capture,
            output,
            duration,
            TranscodeSettings {
                format: options.format,
                quality: options.quality,
                compress: options.compress,
                keep_capture: options.keep_capture,
                encoder: self.encoder(),
                intro,
                zoom,
            },
        );
        self.jobs.borrow_mut().push(job);
        self.tick();
    }

    // ---- Periodic refresh -----------------------------------------------------------------

    fn tick(&self) {
        self.check_recording_alive();
        self.collect_finished_jobs();
        let status = self.snapshot();
        self.refresh_menu(&status);
        *self.shared_status.lock().unwrap() = status;
    }

    /// Notice when ffmpeg exits on its own (bad args, X error, disk full, ...).
    fn check_recording_alive(&self) {
        let exited = self
            .recording
            .borrow_mut()
            .as_mut()
            .and_then(|active| active.recording.try_exit_status());
        let Some(status) = exited else {
            return;
        };
        let active = self.recording.borrow_mut().take();
        self.marker.borrow_mut().take();
        self.phase.set(Phase::Idle);
        let details = active
            .map(|active| {
                restore_placement(active.placement);
                active.recording.error_output()
            })
            .unwrap_or_default();
        eprintln!("ffmpeg exited unexpectedly ({status}): {details}");
        notify(
            "Recording stopped",
            &format!("ffmpeg exited unexpectedly ({status})\n{details}"),
        );
    }

    fn collect_finished_jobs(&self) {
        self.jobs.borrow_mut().retain(|job| match job.state() {
            JobState::Running { .. } => true,
            JobState::Done { output } => {
                println!("saved recording to {}", output.display());
                notify_saved(output, Some(job.capture.clone()));
                false
            }
            JobState::Failed { error } => {
                eprintln!("error: post-processing {} failed: {error}", job.name);
                notify("Processing failed", &format!("{}: {error}", job.name));
                false
            }
        });
    }

    fn snapshot(&self) -> Status {
        let processing = self
            .jobs
            .borrow()
            .iter()
            .filter_map(|job| match job.state() {
                JobState::Running { progress } => Some(JobStatus {
                    name: job.name.clone(),
                    progress,
                }),
                _ => None,
            })
            .collect();

        let mut status = Status {
            processing,
            ..Status::default()
        };
        match self.phase.get() {
            Phase::Idle => status.state = State::Idle,
            Phase::Preparing => status.state = State::Preparing,
            Phase::Countdown(remaining) => {
                status.state = State::Countdown;
                status.countdown = Some(remaining);
                if let Some((_, pending)) = self.countdown.borrow().as_ref() {
                    status.target = Some(pending.target);
                }
            }
            Phase::Recording => {
                status.state = State::Recording;
                if let Some(active) = self.recording.borrow().as_ref() {
                    let region = active.recording.region();
                    status.target = Some(active.target);
                    status.width = Some(region.width);
                    status.height = Some(region.height);
                    status.elapsed_secs = active.recording.elapsed().as_secs();
                    status.mic_level = active.recording.mic_level().map(|level| level.get());
                    status.output = Some(active.final_output.display().to_string());
                }
            }
        }
        status
    }

    fn refresh_menu(&self, status: &Status) {
        self.applet.borrow_mut().update(tray_spec(status));
    }

    // ---- Misc -----------------------------------------------------------------------------

    fn open_folder(&self) {
        let result = self.config.recording.resolved_output_dir().and_then(|dir| {
            std::fs::create_dir_all(&dir)?;
            Command::new("xdg-open").arg(&dir).spawn()?;
            Ok(())
        });
        self.report(result);
    }

    fn quit(self: &Rc<Self>) {
        self.stop();
        if !self.jobs.borrow().is_empty() {
            eprintln!(
                "warning: quitting with post-processing in progress; .capture.mkv files are kept"
            );
        }
        process::exit(0);
    }

    fn report(&self, result: Result<()>) {
        if let Err(err) = result {
            eprintln!("error: {err:#}");
            notify("slycapture error", &format!("{err:#}"));
        }
    }
}

/// `(capture, final)` paths: `<dir>/<base>-<timestamp>.capture.mkv` and `.<format>`.
fn output_paths(dir: &Path, base_name: &str, format: Format) -> (PathBuf, PathBuf) {
    let stamp = glib::DateTime::now_local()
        .and_then(|now| now.format("%Y%m%d-%H%M%S"))
        .map(|stamp| stamp.to_string())
        .unwrap_or_else(|_| process::id().to_string());
    let stem = format!("{}-{stamp}", sanitize_base_name(base_name));
    (
        dir.join(format!("{stem}.capture.mkv")),
        dir.join(format!("{stem}.{}", format.extension())),
    )
}

/// "Recording saved" with Open / Show in Folder / Copy / Move to Trash actions. `capture` is
/// the `.capture.mkv` it was made from, if that may still exist.
fn notify_saved(output: PathBuf, capture: Option<PathBuf>) {
    let zoomed = Some(zoomed_path(&output)).filter(|path| path.exists());
    let name = output
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut body = name;
    if let Ok(metadata) = std::fs::metadata(&output) {
        body.push_str(&format!(" ({})", desktop::human_size(metadata.len())));
    }
    if zoomed.is_some() {
        body.push_str("\nplus a zoomed copy");
    }

    let mut actions = vec![("default", "Open"), ("open", "Open")];
    if zoomed.is_some() {
        actions.push(("open-zoomed", "Open Zoomed"));
    }
    actions.extend([
        ("folder", "Show in Folder"),
        ("copy", "Copy"),
        ("trash", "Move to Trash"),
    ]);

    notify_with_actions(
        "Recording saved",
        &body,
        SAVED_NOTIFICATION_TIMEOUT,
        &actions,
        move |action| {
            let result = match action.as_str() {
                "default" | "open" => desktop::open(&output),
                "open-zoomed" => zoomed.as_deref().map_or(Ok(()), desktop::open),
                "folder" => desktop::show_in_folder(&output),
                "copy" => desktop::copy_to_clipboard(&output),
                "trash" => {
                    // Everything this recording produced, so a failed take leaves nothing.
                    let mut paths = vec![output.clone()];
                    paths.extend(zoomed.clone());
                    if let Some(capture) = &capture {
                        paths.push(cursor_log::sidecar_path(capture));
                        paths.push(capture.clone());
                    }
                    desktop::trash(&paths).map(|trashed| {
                        notify(
                            "Recording moved to Trash",
                            &format!("{} file(s)", trashed.len()),
                        )
                    })
                }
                _ => Ok(()),
            };
            if let Err(err) = result {
                eprintln!("error: {err:#}");
                notify("slycapture error", &format!("{err:#}"));
            }
        },
    );
}

fn restore_placement(placement: Option<WindowPlacement>) {
    if let Some(placement) = placement {
        if let Err(err) = i3::restore_window(&placement) {
            eprintln!("warning: failed to restore window placement: {err:#}");
        }
    }
}

/// The tray's menu, icon and click behaviour for a status. While recording (or counting
/// down) a left click stops; otherwise it opens the menu.
fn tray_spec(status: &Status) -> TraySpec<TrayAction> {
    let headline = match status.state {
        State::Recording => {
            let mut label = format!("● Recording {}", format_elapsed(status.elapsed_secs));
            if let (Some(width), Some(height)) = (status.width, status.height) {
                label.push_str(&format!("  {width}x{height}"));
            }
            label
        }
        State::Countdown => format!("Starting in {}s…", status.countdown.unwrap_or(0)),
        State::Preparing => "Preparing…".to_string(),
        State::Idle | State::Offline => "Idle".to_string(),
    };

    let idle = status.state == State::Idle;
    let active = matches!(status.state, State::Recording | State::Countdown);
    let action = |label: &str, enabled: bool, action: TrayAction| MenuEntry::Action {
        label: label.to_string(),
        enabled,
        action,
    };

    let mut menu = vec![MenuEntry::Label(headline.clone())];
    menu.extend(status.processing.iter().map(|job| {
        MenuEntry::Label(format!(
            "Processing {} — {:.0}%",
            job.name,
            job.progress * 100.0
        ))
    }));
    menu.extend([
        MenuEntry::Separator,
        action(
            "Record Entire Screen…",
            idle,
            TrayAction::Start(Target::Screen),
        ),
        action("Record Window…", idle, TrayAction::Start(Target::Window)),
        action("Record Region…", idle, TrayAction::Start(Target::Region)),
        action(
            if status.state == State::Countdown {
                "Cancel Countdown"
            } else {
                "Stop Recording"
            },
            active,
            TrayAction::Stop,
        ),
        MenuEntry::Separator,
        action("Open Recordings Folder", true, TrayAction::OpenFolder),
        MenuEntry::Separator,
        action("Quit", true, TrayAction::Quit),
    ]);

    TraySpec {
        icon: if active {
            IconState::Attention
        } else {
            IconState::Normal
        },
        tooltip: if active {
            format!("{headline} (middle-click to stop)")
        } else {
            headline
        },
        menu,
        click_action: active.then_some(TrayAction::Stop),
    }
}
