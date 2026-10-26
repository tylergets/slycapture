//! Reusable tray applet plumbing: an AppIndicator (same stack as slyboard) described by a
//! plain [`TraySpec`], bundled icons, notifications, and running blocking work off the GTK
//! thread.
//!
//! Menu clicks are delivered as values of the app's action type `A` on an mpsc channel; the
//! app drains it from the GTK main loop.

use std::env;
use std::ffi::CString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use gtk::glib::{self, ControlFlow};
use gtk::prelude::*;

const BACKGROUND_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// `APP_INDICATOR_CATEGORY_APPLICATION_STATUS`.
const CATEGORY_APPLICATION_STATUS: u32 = 0;
/// `APP_INDICATOR_STATUS_ACTIVE`.
const STATUS_ACTIVE: u32 = 1;

/// An SVG icon written to disk on startup.
pub struct BundledIcon {
    pub name: &'static str,
    pub svg: &'static str,
}

pub const IDLE_ICON: BundledIcon = BundledIcon {
    name: "slycapture",
    svg: r##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32" viewBox="0 0 24 24">
  <circle cx="12" cy="12" r="9" fill="none" stroke="#d3d2d1" stroke-width="2"/>
  <circle cx="12" cy="12" r="4.5" fill="#d3d2d1"/>
</svg>
"##,
};

pub const ATTENTION_ICON: BundledIcon = BundledIcon {
    name: "slycapture-recording",
    svg: r##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32" viewBox="0 0 24 24">
  <circle cx="12" cy="12" r="9" fill="none" stroke="#e5484d" stroke-width="2"/>
  <circle cx="12" cy="12" r="5.5" fill="#e5484d"/>
</svg>
"##,
};

const FALLBACK_ICON: &str = "media-record";

/// Which icon is shown: neutral, or red (e.g. while recording).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Normal,
    Attention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry<A> {
    /// Insensitive informational line.
    Label(String),
    Action {
        label: String,
        enabled: bool,
        action: A,
    },
    Separator,
}

/// Everything the tray shows. Rebuilt by the app whenever its state changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraySpec<A> {
    pub icon: IconState,
    pub tooltip: String,
    pub menu: Vec<MenuEntry<A>>,
    /// Sent on a secondary activation (middle click). AppIndicator always opens the menu on
    /// a left click.
    pub click_action: Option<A>,
}

pub struct Applet<A: Clone + Send + 'static> {
    indicator: *mut libappindicator::_AppIndicator,
    actions: Sender<A>,
    spec: TraySpec<A>,
    menu: gtk::Menu,
    /// Widgets for `spec.menu`, index for index.
    widgets: Vec<gtk::MenuItem>,
    icon: String,
    attention_icon: String,
}

impl<A: Clone + Send + PartialEq + 'static> Applet<A> {
    /// Create the indicator. GTK must already be initialized.
    pub fn new(id: &str, spec: TraySpec<A>, actions: Sender<A>) -> Result<Self> {
        if env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            eprintln!("warning: DBus session is not set; appindicator may not be visible");
        }

        let (theme_path, icon, attention_icon) =
            match install_icons(&[&IDLE_ICON, &ATTENTION_ICON]) {
                Some(dir) => (
                    dir.to_string_lossy().into_owned(),
                    IDLE_ICON.name.to_string(),
                    ATTENTION_ICON.name.to_string(),
                ),
                None => (
                    String::new(),
                    FALLBACK_ICON.to_string(),
                    FALLBACK_ICON.to_string(),
                ),
            };

        let id_c = CString::new(id).context("invalid applet id")?;
        let icon_c = CString::new(icon.as_str())?;
        let theme_path_c = CString::new(theme_path)?;
        // The indicator lives for the rest of the process, so it is never unreferenced.
        let indicator = unsafe {
            let indicator = libappindicator::app_indicator_new_with_path(
                id_c.as_ptr(),
                icon_c.as_ptr(),
                CATEGORY_APPLICATION_STATUS,
                theme_path_c.as_ptr(),
            );
            libappindicator::app_indicator_set_title(indicator, id_c.as_ptr());
            libappindicator::app_indicator_set_status(indicator, STATUS_ACTIVE);
            indicator
        };

        let menu = gtk::Menu::new();
        let mut applet = Self {
            indicator,
            actions,
            spec: spec.clone(),
            menu,
            widgets: Vec::new(),
            icon,
            attention_icon,
        };
        applet.rebuild_menu();
        unsafe {
            libappindicator::app_indicator_set_menu(
                applet.indicator,
                applet.menu.as_ptr(),
            );
        }
        applet.apply_icon_and_click_action();
        Ok(applet)
    }

    pub fn update(&mut self, spec: TraySpec<A>) {
        if spec == self.spec {
            return;
        }
        let same_shape = spec.menu.len() == self.spec.menu.len()
            && spec
                .menu
                .iter()
                .zip(&self.spec.menu)
                .all(|(new, old)| same_slot(new, old));
        self.spec = spec;

        // Update in place when only labels/sensitivity changed, so an open menu doesn't
        // flicker or close.
        if same_shape {
            for (entry, widget) in self.spec.menu.iter().zip(&self.widgets) {
                match entry {
                    MenuEntry::Label(label) => widget.set_label(label),
                    MenuEntry::Action { label, enabled, .. } => {
                        widget.set_label(label);
                        widget.set_sensitive(*enabled);
                    }
                    MenuEntry::Separator => {}
                }
            }
        } else {
            self.rebuild_menu();
        }
        self.apply_icon_and_click_action();
    }

    fn rebuild_menu(&mut self) {
        for child in self.menu.children() {
            self.menu.remove(&child);
        }
        self.widgets = self
            .spec
            .menu
            .iter()
            .map(|entry| {
                let widget = match entry {
                    MenuEntry::Label(label) => {
                        let item = gtk::MenuItem::with_label(label);
                        item.set_sensitive(false);
                        item
                    }
                    MenuEntry::Action {
                        label,
                        enabled,
                        action,
                    } => {
                        let item = gtk::MenuItem::with_label(label);
                        item.set_sensitive(*enabled);
                        let actions = self.actions.clone();
                        let action = action.clone();
                        item.connect_activate(move |_| {
                            let _ = actions.send(action.clone());
                        });
                        item
                    }
                    MenuEntry::Separator => gtk::SeparatorMenuItem::new().upcast(),
                };
                self.menu.append(&widget);
                widget
            })
            .collect();
        self.menu.show_all();
    }

    fn apply_icon_and_click_action(&self) {
        let name = match self.spec.icon {
            IconState::Normal => &self.icon,
            IconState::Attention => &self.attention_icon,
        };
        if let Ok(name) = CString::new(name.as_str()) {
            unsafe {
                libappindicator::app_indicator_set_icon_full(
                    self.indicator,
                    name.as_ptr(),
                    name.as_ptr(),
                );
            }
        }

        let target = self.spec.click_action.as_ref().and_then(|click| {
            self.spec
                .menu
                .iter()
                .zip(&self.widgets)
                .find(|(entry, _)| {
                    matches!(entry, MenuEntry::Action { action, .. } if action == click)
                })
                .map(|(_, widget)| widget.upcast_ref::<gtk::Widget>().as_ptr())
        });
        unsafe {
            libappindicator::app_indicator_set_secondary_activate_target(
                self.indicator,
                target.unwrap_or(std::ptr::null_mut()),
            );
        }
    }
}

/// Whether two entries can share a widget (same kind; actions must match since the
/// activate handler captures the action).
fn same_slot<A: PartialEq>(new: &MenuEntry<A>, old: &MenuEntry<A>) -> bool {
    match (new, old) {
        (MenuEntry::Label(_), MenuEntry::Label(_)) => true,
        (MenuEntry::Separator, MenuEntry::Separator) => true,
        (MenuEntry::Action { action: a, .. }, MenuEntry::Action { action: b, .. }) => a == b,
        _ => false,
    }
}

/// Write `icons` into the user's hicolor theme (as slyboard does) and return that directory,
/// which is also passed to the tray host as the indicator's icon theme path so hosts that
/// cached the theme before the icons existed still find them.
fn install_icons(icons: &[&BundledIcon]) -> Option<PathBuf> {
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")))?;
    let dir = data_home.join("icons/hicolor/scalable/apps");

    if let Err(err) = fs::create_dir_all(&dir) {
        eprintln!("warning: failed to create icon directory {}: {err}", dir.display());
        return None;
    }
    for icon in icons {
        let path = dir.join(format!("{}.svg", icon.name));
        if let Err(err) = fs::write(&path, icon.svg) {
            eprintln!("warning: failed to write tray icon {}: {err}", path.display());
            return None;
        }
    }
    Some(dir)
}

pub fn notify(summary: &str, body: &str) {
    if let Err(err) = Command::new("notify-send")
        .args(["-a", "slycapture", "-t", "3000", summary, body])
        .spawn()
    {
        eprintln!("warning: failed to send notification: {err}");
    }
}

/// Run `work` on a background thread and deliver its result to `done` on the GTK thread.
pub fn spawn_blocking<T, W, D>(work: W, done: D)
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
    D: FnOnce(T) + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(work());
    });

    let mut done = Some(done);
    glib::timeout_add_local(BACKGROUND_POLL_INTERVAL, move || match rx.try_recv() {
        Ok(value) => {
            if let Some(done) = done.take() {
                done(value);
            }
            ControlFlow::Break
        }
        Err(TryRecvError::Empty) => ControlFlow::Continue,
        Err(TryRecvError::Disconnected) => ControlFlow::Break,
    });
}

#[cfg(test)]
mod tests {
    use super::{same_slot, MenuEntry};

    #[test]
    fn menu_slots_match_by_kind_and_action() {
        let label = |text: &str| MenuEntry::<u8>::Label(text.into());
        let action = |action: u8| MenuEntry::Action {
            label: "x".into(),
            enabled: true,
            action,
        };
        assert!(same_slot(&label("a"), &label("b")));
        assert!(same_slot(&action(1), &action(1)));
        assert!(!same_slot(&action(1), &action(2)));
        assert!(!same_slot(&label("a"), &MenuEntry::Separator));
    }
}
