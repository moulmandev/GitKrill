//! What the user sees of updates: the offer dialog, the status bar's
//! "Restart to update", the Help menu entries, and the reactions to what the
//! updater finds (see `crate::update` for how each kind of copy updates).

use std::sync::Arc;

use eframe::egui::{self, Align, Button, Layout, RichText, Ui, vec2};

use crate::state::{Dialog, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::update::{self, Kind, Notice, Release, Status, Version};

/// Once a frame: runs the timer and reacts to what the updater found.
pub fn poll(ctx: &egui::Context, shared: &mut Shared) {
    shared.updater.tick(ctx, shared.settings.check_updates);

    for notice in shared.updater.notices() {
        match notice {
            Notice::Found { release, asked } => found(ctx, shared, release, asked),
            Notice::UpToDate => shared.toast(
                ToastKind::Success,
                "GitKrill is up to date",
                format!("Version {} is the latest.", Version::current()),
            ),
            Notice::Downloaded(release, file) => match shared.updater.kind {
                Kind::Installed => shared.toast(
                    ToastKind::Success,
                    format!("GitKrill {} is ready", release.version),
                    "It installs when you close GitKrill. Restart now from the status bar.",
                ),
                Kind::Portable => {
                    let replaced = update::replace_self(&file).and_then(|()| update::restart());
                    match replaced {
                        Ok(()) => {
                            shared.settings.save();
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(message) => shared.toast(ToastKind::Error, "Could not update GitKrill", message),
                    }
                }
                Kind::Browser | Kind::Off => {}
            },
            Notice::Failed { message, asked } => {
                if asked {
                    shared.toast(ToastKind::Error, "Could not update GitKrill", message);
                }
            }
        }
    }

    // An offer waits for the dialog in front to close.
    if shared.dialog.is_none()
        && let Some(release) = shared.updater.offer.take()
    {
        shared.show(Dialog::Update(release));
    }
}

/// A newer release: an installed copy fetches it straight away; the others
/// ask, unless the user skipped this version (a check they ran still asks).
fn found(ctx: &egui::Context, shared: &mut Shared, release: Arc<Release>, asked: bool) {
    match shared.updater.kind {
        Kind::Installed => {
            // A check the user ran shows the download's progress.
            if asked {
                shared.updater.offer = Some(release.clone());
            }
            shared.updater.download(ctx, release, asked);
        }
        Kind::Portable | Kind::Browser => {
            let skipped = shared.settings.skipped_update.as_deref() == Some(&release.version.to_string());
            if asked || !skipped {
                shared.updater.offer = Some(release);
            }
        }
        Kind::Off => {}
    }
}

/// Starts an installed copy's installer when the app closes, if an update
/// is waiting.
pub fn on_exit(shared: &mut Shared) {
    let _ = shared.updater.install(false);
}

/// The offer: what is new, and what to do about it. Returns true to close.
pub fn dialog(ui: &mut Ui, shared: &mut Shared, release: &Arc<Release>) -> bool {
    let kind = shared.updater.kind;
    let status = shared.updater.status();
    let ctx = ui.ctx().clone();

    crate::ui::dialogs::title(ui, &format!("GitKrill {} is available", release.version));
    ui.label(RichText::new(format!("You have {}.", Version::current())).color(theme::text_dim()));

    let notes = update::plain_notes(&release.notes);
    if !notes.is_empty() {
        egui::Frame::new().fill(theme::bg_input()).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                for line in &notes {
                    let heading = !line.starts_with('•');
                    let text = RichText::new(line).color(if heading { theme::text_bright() } else { theme::text() });
                    ui.label(if heading { text.strong() } else { text });
                }
            });
        });
    }
    let page = shared.updater.source().page(release);
    if ui.link(format!("{}  What's new on GitHub", icons::ARROW_SQUARE_OUT)).clicked() {
        crate::ui::hosting::open_link(&page);
    }

    // Progress, or why the last attempt failed.
    let downloading = match &status {
        Status::Downloading { release: r, received, total } if r.version == release.version => {
            let mb = |bytes: u64| bytes as f32 / (1024.0 * 1024.0);
            let text = match total {
                Some(total) => format!("Downloading… {:.1} of {:.1} MB", mb(*received), mb(*total)),
                None => format!("Downloading… {:.1} MB", mb(*received)),
            };
            let fraction = total.map_or(0.0, |total| *received as f32 / total.max(1) as f32);
            ui.add(egui::ProgressBar::new(fraction).text(text).desired_height(18.0));
            true
        }
        Status::Failed(message) => {
            ui.label(RichText::new(format!("{}  {message}", icons::WARNING_CIRCLE)).color(theme::red()));
            false
        }
        _ => false,
    };
    let ready = matches!(&status, Status::Ready { release: r, .. } if r.version == release.version);

    let mut close = false;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if kind != Kind::Installed && ui.add_enabled(!downloading, Button::new("Skip this version")).clicked() {
            shared.settings.skipped_update = Some(release.version.to_string());
            close = true;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (label, enabled) = match kind {
                Kind::Portable if downloading => ("Updating…", false),
                Kind::Portable => ("Update and restart", true),
                Kind::Installed if ready => ("Restart to update", true),
                Kind::Installed if downloading => ("Downloading…", false),
                Kind::Installed => ("Update", true),
                Kind::Browser | Kind::Off => ("Download", true),
            };
            let fill = if enabled { theme::solid(theme::accent(), 0.8) } else { theme::bg_raised() };
            let text = RichText::new(label).color(if enabled { theme::on_accent() } else { theme::text_dim() });
            if ui.add_enabled(enabled, Button::new(text).fill(fill).min_size(vec2(84.0, 28.0))).clicked() {
                match kind {
                    Kind::Portable => shared.updater.download(&ctx, release.clone(), true),
                    Kind::Installed if ready => {
                        restart_to_update(&ctx, shared);
                        close = true;
                    }
                    Kind::Installed => shared.updater.download(&ctx, release.clone(), true),
                    Kind::Browser | Kind::Off => {
                        crate::ui::hosting::open_link(&page);
                        close = true;
                    }
                }
            }
            if ui.add(Button::new("Later").min_size(vec2(72.0, 28.0))).clicked() {
                close = true;
            }
        });
    });
    close
}

/// Installed copies: runs the waiting installer now and closes, so it can
/// replace the program and start it again.
fn restart_to_update(ctx: &egui::Context, shared: &mut Shared) {
    match shared.updater.install(true) {
        Ok(true) => {
            shared.settings.save();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        Ok(false) => {}
        Err(message) => shared.toast(ToastKind::Error, "Could not update GitKrill", message),
    }
}

/// The status bar's update indicator, at its right end: an update waiting
/// to be installed, one being downloaded, or one on offer.
pub fn status_item(ui: &mut Ui, shared: &mut Shared) {
    let ctx = ui.ctx().clone();
    let pill = |ui: &mut Ui, icon: &str, text: String| {
        let label = RichText::new(format!("{icon}  {text}")).size(11.5).color(theme::on_accent());
        ui.add(Button::new(label).fill(theme::solid(theme::accent(), 0.8)).min_size(vec2(0.0, 18.0)))
    };
    match shared.updater.status() {
        Status::Ready { release, .. } if shared.updater.kind == Kind::Installed && !shared.updater.applied => {
            let response = pill(ui, icons::ARROW_CLOCKWISE, format!("Restart to update to {}", release.version));
            if response.on_hover_text("Installs the update and opens GitKrill again").clicked() {
                restart_to_update(&ctx, shared);
            }
        }
        Status::Downloading { release, received, total } => {
            let percent = total.map(|total| format!(" {}%", received * 100 / total.max(1))).unwrap_or_default();
            ui.add(egui::Spinner::new().size(11.0).color(theme::accent()));
            let text = format!("Downloading GitKrill {}{percent}", release.version);
            ui.label(RichText::new(text).size(11.5).color(theme::text_dim()));
        }
        Status::Available(release) if shared.updater.kind != Kind::Installed => {
            let response = pill(ui, icons::ARROW_CIRCLE_UP, format!("GitKrill {} is available", release.version));
            if response.clicked() {
                shared.show(Dialog::Update(release));
            }
        }
        _ => {}
    }
}

/// "Check for Updates…" and the automatic-check switch, for the Help menu.
pub fn menu(ui: &mut Ui, shared: &mut Shared) {
    let ctx = ui.ctx().clone();
    let kind = shared.updater.kind;
    let checking = matches!(shared.updater.status(), Status::Checking);
    let label = if checking { "Checking for Updates…" } else { "Check for Updates…" };
    let response = ui.add_enabled(kind != Kind::Off && !checking, Button::new(label));
    if response.clicked() {
        check_now(&ctx, shared);
        ui.close();
    }
    response.on_disabled_hover_text(match kind {
        Kind::Off => "Builds made from source do not update themselves",
        _ => "Checking…",
    });
    if kind != Kind::Off {
        ui.checkbox(&mut shared.settings.check_updates, "Check for Updates Automatically");
    }
}

/// A check the user asked for: says what it found even when it is nothing.
pub fn check_now(ctx: &egui::Context, shared: &mut Shared) {
    match shared.updater.status() {
        // Already known: show it again rather than asking GitHub.
        Status::Available(release) if shared.updater.kind != Kind::Installed => shared.show(Dialog::Update(release)),
        Status::Ready { release, .. } => shared.show(Dialog::Update(release)),
        _ => shared.updater.check(ctx, true),
    }
}
