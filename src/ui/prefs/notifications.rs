//! Preferences > Notifications. The setting lives in
//! `crate::prefs::appearance`; `ui::dialogs::toasts` acts on it.

use eframe::egui::Ui;

use super::{Page, form};

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {}

/// Where system notifications appear, in the system's own words.
const SYSTEM: &str = if cfg!(target_os = "macos") {
    "in Notification Center"
} else if cfg!(windows) {
    "in a Windows notification, which stays in the notification centre"
} else {
    "in a desktop notification"
};

/// How the window manager draws attention, in its own words.
const ATTENTION: &str = if cfg!(target_os = "macos") {
    "its Dock icon bounces"
} else if cfg!(windows) {
    "its taskbar button flashes until you come back"
} else {
    "its window asks for your attention"
};

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    let _ = state;
    form::row(ui, "Enable Desktop Notifications", |ui| {
        form::checkbox(ui, &mut page.shared.settings.appearance.desktop_notifications, "");
        form::help(
            ui,
            &format!(
                "When a push, pull, fetch or clone ends, or an operation fails, while GitKrill is in the \
                 background, {ATTENTION}."
            ),
        );
    });
    let appearance = &mut page.shared.settings.appearance;
    form::row(ui, "Show the Message in a System Notification", |ui| {
        let enabled = appearance.desktop_notifications;
        ui.add_enabled_ui(enabled, |ui| form::checkbox(ui, &mut appearance.system_notifications, ""));
        form::help(
            ui,
            &format!(
                "Also shows what the message says (\"Pushed main to origin\", why a pull failed) {SYSTEM}, \
                 and when git waits for a password or passphrase."
            ),
        );
    });
}
