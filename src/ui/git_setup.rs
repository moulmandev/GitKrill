//! What the user sees when Git is missing: the dialog that offers to get it
//! (see `crate::git_setup`), and a warning in the status bar until it is
//! there.

use eframe::egui::{self, Align, Button, Layout, RichText, Ui, vec2};

use crate::git_setup::{self, State};
use crate::state::{Dialog, Shared, ToastKind};
use crate::theme::{self, icons};

/// Once a frame: looks for git after start (and again when the window comes
/// back to the front while it is missing), asks once per run when it is
/// missing, and says how an install ended.
pub fn poll(ctx: &egui::Context, shared: &mut Shared) {
    shared.git.start(ctx);
    let state = shared.git.state();

    // Installed by hand in another window: noticed on return.
    let focused = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::WindowFocused(true))));
    if focused && state.is_missing() {
        shared.git.check(ctx);
    }

    if state.is_missing() && !shared.git.prompted && shared.dialog.is_none() {
        shared.git.prompted = true;
        shared.show(Dialog::GitSetup);
    }

    match shared.git.take_finished() {
        Some(Ok(version)) => {
            if matches!(shared.dialog, Some(Dialog::GitSetup)) {
                shared.dialog = None;
            }
            shared.toast(ToastKind::Success, "Git is installed", version);
        }
        // The dialog shows the reason when it is open.
        Some(Err(why)) if !matches!(shared.dialog, Some(Dialog::GitSetup)) => {
            shared.toast(ToastKind::Error, "Git could not be installed", why);
        }
        _ => {}
    }
    // A check the user asked for that found git closes the dialog too.
    if matches!(state, State::Found(_)) && matches!(shared.dialog, Some(Dialog::GitSetup)) {
        shared.dialog = None;
    }
}

/// The download page for this system.
fn download_page() -> String {
    let os = if cfg!(windows) {
        "win"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else {
        "linux"
    };
    format!("{}/{os}", git_setup::DOWNLOAD_PAGE)
}

/// The offer. Returns true to close.
pub fn dialog(ui: &mut Ui, shared: &mut Shared) -> bool {
    let ctx = ui.ctx().clone();
    let state = shared.git.state();

    crate::ui::dialogs::title(ui, "Git is not installed");
    ui.label(
        RichText::new(
            "GitKrill shows your repositories without it, but needs Git to commit, check out, \
             merge, pull and push.",
        )
        .color(theme::text()),
    );

    let boxed = |ui: &mut Ui, add: &mut dyn FnMut(&mut Ui)| {
        egui::Frame::new().fill(theme::bg_input()).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    };
    if shared.git.can_install() {
        boxed(ui, &mut |ui| {
            ui.label(
                RichText::new(
                    "GitKrill can download Git for Windows from its GitHub releases (about \
                     65\u{a0}MB), check it against its published SHA-256, and install it for your \
                     account only, with Git's usual settings. No administrator rights needed.",
                )
                .color(theme::text_dim()),
            );
        });
    } else if cfg!(target_os = "macos") {
        boxed(ui, &mut |ui| {
            ui.label(RichText::new("Apple's Command Line Tools include Git. Or, with Homebrew:").color(theme::text_dim()));
            command_line(ui, "brew install git");
        });
    } else {
        boxed(ui, &mut |ui| {
            ui.label(RichText::new("Install it with your package manager, then press Check again:").color(theme::text_dim()));
            command_line(ui, git_setup::this_linux_command());
        });
    }

    // What is happening, or why the last attempt failed.
    match &state {
        State::Downloading { received, total } => {
            let mb = |bytes: u64| bytes as f32 / (1024.0 * 1024.0);
            let text = match total {
                Some(total) => format!("Downloading Git… {:.1} of {:.1} MB", mb(*received), mb(*total)),
                None => format!("Downloading Git… {:.1} MB", mb(*received)),
            };
            let fraction = total.map_or(0.0, |total| *received as f32 / total.max(1) as f32);
            ui.add(egui::ProgressBar::new(fraction).text(text).desired_height(18.0));
        }
        State::Installing => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(theme::accent()));
                ui.label(RichText::new("Installing Git…").color(theme::text_dim()));
            });
            ctx.request_repaint();
        }
        State::Checking => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(theme::accent()));
                ui.label(RichText::new("Looking for Git…").color(theme::text_dim()));
            });
        }
        State::Failed(why) => {
            ui.label(RichText::new(format!("{}  {why}", icons::WARNING_CIRCLE)).color(theme::red()));
        }
        State::Missing(_) | State::Found(_) => {}
    }

    let busy = state.is_busy();
    let mut close = false;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.add_enabled(!busy, Button::new("Check again")).on_hover_text("Look for Git again").clicked() {
            shared.git.check(&ctx);
        }
        if ui.link(format!("{}  Download page", icons::ARROW_SQUARE_OUT)).clicked() {
            crate::ui::hosting::open_link(&download_page());
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let primary = if shared.git.can_install() {
                Some(if matches!(state, State::Failed(_)) { "Try again" } else { "Install Git" })
            } else if cfg!(target_os = "macos") {
                Some("Install the tools")
            } else {
                None
            };
            if let Some(label) = primary {
                let fill = if busy { theme::bg_raised() } else { theme::solid(theme::accent(), 0.8) };
                let text = RichText::new(label).color(if busy { theme::text_dim() } else { theme::on_accent() });
                if ui.add_enabled(!busy, Button::new(text).fill(fill).min_size(vec2(96.0, 28.0))).clicked() {
                    if shared.git.can_install() {
                        shared.git.install(&ctx);
                    } else if let Err(why) = git_setup::install_command_line_tools() {
                        shared.toast(ToastKind::Error, "Could not start the installation", why);
                    }
                }
            }
            // Closing never stops an install; the status bar follows it.
            if ui.add(Button::new("Not now").min_size(vec2(72.0, 28.0))).clicked() {
                close = true;
            }
        });
    });
    close
}

/// A command to type, in a monospace line with a Copy button.
fn command_line(ui: &mut Ui, command: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(command).font(theme::mono(13.0)).color(theme::text_bright()));
        if (command.starts_with("sudo") || command.starts_with("brew"))
            && ui.small_button(format!("{}  Copy", icons::COPY)).clicked()
        {
            ui.ctx().copy_text(command.to_owned());
        }
    });
}

/// The status bar's Git warning: missing (click to see the offer), or being
/// downloaded or installed.
pub fn status_item(ui: &mut Ui, shared: &mut Shared) {
    let dim = |text: &str| RichText::new(text).size(11.5).color(theme::text_dim());
    match shared.git.state() {
        State::Missing(_) | State::Failed(_) => {
            let label = RichText::new(format!("{}  Git not found", icons::WARNING)).size(11.5).color(theme::on_accent());
            let button = Button::new(label).fill(theme::solid(theme::orange(), 0.8)).min_size(vec2(0.0, 18.0));
            if ui.add(button).on_hover_text("GitKrill needs Git to change repositories. Click to get it.").clicked() {
                shared.show(Dialog::GitSetup);
            }
        }
        State::Downloading { received, total } => {
            let percent = total.map(|total| format!(" {}%", received * 100 / total.max(1))).unwrap_or_default();
            ui.add(egui::Spinner::new().size(11.0).color(theme::accent()));
            ui.label(dim(&format!("Downloading Git{percent}")));
        }
        State::Installing => {
            ui.add(egui::Spinner::new().size(11.0).color(theme::accent()));
            ui.label(dim("Installing Git"));
        }
        State::Checking | State::Found(_) => {}
    }
}
