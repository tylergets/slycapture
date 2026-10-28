//! Keeps a libappindicator tray icon registered with the StatusNotifierWatcher.
//!
//! libappindicator only registers its item if a tray host (the bar) is already
//! registered with the watcher. Otherwise it falls back to an XEmbed icon and
//! waits for a `PropertiesChanged` signal it never receives, because its watcher
//! proxy is created with `G_DBUS_PROXY_FLAGS_DO_NOT_CONNECT_SIGNALS`. At login the
//! bar often starts after us, so the icon never shows up.
//!
//! We watch for hosts and watcher restarts ourselves and poke the indicator by
//! re-setting `icon-theme-path` to its current value. That is a no-op except that
//! it makes libappindicator re-run its registration. Watchers ignore duplicate
//! registrations, so extra pokes are harmless.

use std::time::Duration;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
/// Gives libappindicator time to build its own watcher proxy after the watcher appears.
const APPEAR_DELAY: Duration = Duration::from_secs(2);

/// Must be called on the GTK thread. The indicator must outlive the process.
pub fn keep_registered(indicator: *mut libappindicator::_AppIndicator) {
    let connection = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        Ok(connection) => connection,
        Err(err) => {
            eprintln!("warning: cannot watch for tray hosts: {err}");
            return;
        }
    };
    let indicator: glib::Object =
        unsafe { glib::translate::from_glib_none(indicator as *mut glib::gobject_ffi::GObject) };

    // A host (re)registered: the bar started or restarted.
    let on_host = indicator.clone();
    #[allow(deprecated)]
    connection.signal_subscribe(
        Some(WATCHER_NAME),
        Some(WATCHER_NAME),
        Some("StatusNotifierHostRegistered"),
        Some(WATCHER_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, _| reregister(&on_host),
    );

    // The watcher (re)started. Wait for libappindicator to build its new proxy first.
    let on_watcher = indicator.clone();
    #[allow(deprecated)]
    connection.signal_subscribe(
        Some("org.freedesktop.DBus"),
        Some("org.freedesktop.DBus"),
        Some("NameOwnerChanged"),
        Some("/org/freedesktop/DBus"),
        Some(WATCHER_NAME),
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, _| reregister_later(&on_watcher),
    );

    // Covers a host that registered before we subscribed.
    reregister_later(&indicator);
}

fn reregister_later(indicator: &glib::Object) {
    let indicator = indicator.clone();
    glib::timeout_add_local_once(APPEAR_DELAY, move || reregister(&indicator));
}

fn reregister(indicator: &glib::Object) {
    let theme_path: Option<String> = indicator.property("icon-theme-path");
    indicator.set_property("icon-theme-path", theme_path);
}
