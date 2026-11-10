//! Modal dialogs and toast notifications.

use std::time::Duration;

use eframe::egui::{
    self, Align, Align2, Area, Button, Color32, Frame, Id, Key, Layout, Margin, Modal, Order,
    RichText, Stroke, TextEdit, Ui, vec2,
};

use crate::git::ops::{self, PullMode};
use crate::state::{AppAction, Dialog, RepoTab, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::toolbar::push_cmd;
use crate::ui::widgets::icon_button;

enum Outcome {
    Keep,
    Close,
}

pub fn show(ctx: &egui::Context, shared: &mut Shared, tab: Option<&mut RepoTab>) {
    let Some(mut dialog) = shared.dialog.take() else { return };
    let fresh = std::mem::take(&mut shared.dialog_fresh);

    let frame = Frame::new()
        .fill(theme::bg_toolbar())
        .stroke(Stroke::new(1.0, theme::border_light()))
        .corner_radius(8.0)
        .inner_margin(Margin::same(20));
    let modal = Modal::new(Id::new("gitkrill-dialog")).frame(frame).show(ctx, |ui| {
        ui.set_width(400.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        body(ui, &mut dialog, shared, tab, fresh)
    });

    let close = matches!(modal.inner, Outcome::Close) || modal.should_close();
    // A dialog may have queued a follow-up; never overwrite that.
    if !close && shared.dialog.is_none() {
        shared.dialog = Some(dialog);
    }
}

pub(crate) fn title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::bold(16.0)).color(theme::text_bright()));
    ui.add_space(2.0);
}

fn field(ui: &mut Ui, value: &mut String, hint: &str, focus: bool) -> egui::Response {
    let response = ui.add(TextEdit::singleline(value).hint_text(hint).desired_width(f32::INFINITY));
    if focus {
        response.request_focus();
    }
    response
}

/// Cancel / confirm row. Returns `Some(true)` on confirm, `Some(false)` on cancel.
pub(crate) fn buttons(ui: &mut Ui, confirm: &str, enabled: bool, danger: bool) -> Option<bool> {
    let mut result = None;
    ui.add_space(6.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let fill = if danger { theme::mix(theme::bg_app(), theme::red(), 0.7) } else { theme::mix(theme::bg_app(), theme::accent(), 0.8) };
        let primary = Button::new(RichText::new(confirm).color(Color32::WHITE))
            .fill(if enabled { fill } else { theme::bg_raised() })
            .min_size(vec2(84.0, 28.0));
        if ui.add_enabled(enabled, primary).clicked() {
            result = Some(true);
        }
        if ui.add(Button::new("Cancel").min_size(vec2(72.0, 28.0))).clicked() {
            result = Some(false);
        }
    });
    // Enter confirms from anywhere in the dialog.
    // (destructive confirmations must be clicked).
    if enabled && !danger && result.is_none() && ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift) {
        result = Some(true);
    }
    result
}

/// Git ref names cannot contain spaces; typing one inserts a dash instead.
fn sanitize_ref(name: &mut String) {
    if name.contains(' ') {
        *name = name.replace(' ', "-");
    }
}

fn valid_ref(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.chars().any(|c| c.is_control() || " ~^:?*[\\".contains(c))
}

fn body(
    ui: &mut Ui,
    dialog: &mut Dialog,
    shared: &mut Shared,
    tab: Option<&mut RepoTab>,
    fresh: bool,
) -> Outcome {
    match dialog {
        Dialog::CreateBranch { at, at_label, name, checkout } => {
            title(ui, "Create branch");
            ui.label(RichText::new(format!("Starting at {at_label}")).color(theme::text_dim()));
            field(ui, name, "Branch name", fresh);
            sanitize_ref(name);
            ui.checkbox(checkout, "Check out the new branch");
            let exists = tab
                .as_ref()
                .and_then(|t| t.snap.as_ref())
                .is_some_and(|s| s.branches.iter().any(|b| b.name == name.trim()));
            if exists {
                ui.colored_label(theme::orange(), "A branch with this name already exists.");
            }
            match buttons(ui, "Create", valid_ref(name) && !exists, false) {
                Some(true) => {
                    if let Some(tab) = tab {
                        tab.run(ops::create_branch(name.trim(), at.as_deref(), *checkout));
                    }
                    Outcome::Close
                }
                Some(false) => Outcome::Close,
                None => Outcome::Keep,
            }
        }

        Dialog::RenameBranch { old, name } => {
            title(ui, "Rename branch");
            ui.label(RichText::new(format!("Renaming {old}")).color(theme::text_dim()));
            field(ui, name, "New name", fresh);
            sanitize_ref(name);
            match buttons(ui, "Rename", valid_ref(name) && name.trim() != old, false) {
                Some(true) => {
                    if let Some(tab) = tab {
                        tab.run(ops::rename_branch(old, name.trim()));
                    }
                    Outcome::Close
                }
                Some(false) => Outcome::Close,
                None => Outcome::Keep,
            }
        }

        Dialog::CreateTag { at, at_label, name, message } => {
            title(ui, "Create tag");
            ui.label(RichText::new(format!("Tagging {at_label}")).color(theme::text_dim()));
            field(ui, name, "Tag name", fresh);
            sanitize_ref(name);
            field(ui, message, "Message (optional, makes the tag annotated)", false);
            match buttons(ui, "Create", valid_ref(name), false) {
                Some(true) => {
                    if let Some(tab) = tab {
                        tab.run(ops::create_tag(name.trim(), at, message));
                    }
                    Outcome::Close
                }
                Some(false) => Outcome::Close,
                None => Outcome::Keep,
            }
        }

        Dialog::Stash { message } => {
            title(ui, "Stash changes");
            ui.label(
                RichText::new("All changes, including untracked files, are saved and removed from the working tree.")
                    .color(theme::text_dim()),
            );
            field(ui, message, "Message (optional)", fresh);
            match buttons(ui, "Stash", true, false) {
                Some(true) => {
                    if let Some(tab) = tab {
                        tab.run(ops::stash_push(message));
                    }
                    Outcome::Close
                }
                Some(false) => Outcome::Close,
                None => Outcome::Keep,
            }
        }

        Dialog::Confirm { title: heading, body, button, danger, cmd } => {
            title(ui, heading);
            ui.label(RichText::new(body.as_str()).color(theme::text()));
            match buttons(ui, button, true, *danger) {
                Some(true) => {
                    if let Some(tab) = tab {
                        tab.run(cmd.clone());
                    }
                    Outcome::Close
                }
                Some(false) => Outcome::Close,
                None => Outcome::Keep,
            }
        }

        Dialog::PushRejected => {
            title(ui, "Push rejected");
            ui.label(
                "The remote branch has commits you do not have locally. Pull them first, or force \
                 push to replace the remote branch with yours.",
            );
            ui.add_space(6.0);
            let mut outcome = Outcome::Keep;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let Some(tab) = tab else { return };
                let pull = Button::new(RichText::new("Pull (rebase)").color(Color32::WHITE))
                    .fill(theme::mix(theme::bg_app(), theme::accent(), 0.8))
                    .min_size(vec2(96.0, 28.0));
                if ui.add(pull).clicked() {
                    tab.run(ops::pull(PullMode::Rebase));
                    outcome = Outcome::Close;
                }
                if ui.add(Button::new("Pull").min_size(vec2(64.0, 28.0))).clicked() {
                    tab.run(ops::pull(PullMode::Default));
                    outcome = Outcome::Close;
                }
                let force = Button::new(RichText::new("Force push").color(Color32::WHITE))
                    .fill(theme::mix(theme::bg_app(), theme::red(), 0.7))
                    .min_size(vec2(90.0, 28.0));
                if ui.add(force).clicked() {
                    if let Some(cmd) = push_cmd(tab, true) {
                        tab.run(cmd);
                    }
                    outcome = Outcome::Close;
                }
                if ui.add(Button::new("Cancel").min_size(vec2(72.0, 28.0))).clicked() {
                    outcome = Outcome::Close;
                }
            });
            outcome
        }

        Dialog::Clone { url, dest } => {
            title(ui, "Clone repository");
            field(ui, url, "Repository URL (https or ssh)", fresh);
            ui.horizontal(|ui| {
                if ui.button("Browse…").clicked()
                    && let Some(dir) = rfd::FileDialog::new().set_directory(dest.as_str()).pick_folder() {
                        *dest = dir.display().to_string();
                    }
                ui.add(TextEdit::singleline(dest).hint_text("Clone into this folder").desired_width(f32::INFINITY));
            });
            let ready = !url.trim().is_empty() && std::path::Path::new(dest.trim()).is_dir();
            match buttons(ui, "Clone", ready, false) {
                Some(true) => {
                    shared.actions.push(AppAction::Clone {
                        url: url.trim().to_owned(),
                        dest: dest.trim().into(),
                    });
                    Outcome::Close
                }
                Some(false) => Outcome::Close,
                None => Outcome::Keep,
            }
        }

        Dialog::ConnectServer { host, path, choice } => {
            match crate::ui::hosting::connect_body(ui, shared, host, path, choice) {
                true => Outcome::Close,
                false => Outcome::Keep,
            }
        }

        Dialog::About => {
            title(ui, "GitKrill");
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
            ui.label(
                RichText::new(
                    "A native Git client written in Rust. History, status and diffs are read \
                     in-process through libgit2; everything that changes the repository runs \
                     through your installed git, so hooks, signing and credentials behave as usual.",
                )
                .color(theme::text_dim()),
            );
            ui.add_space(6.0);
            let mut outcome = Outcome::Keep;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(Button::new("Close").min_size(vec2(72.0, 28.0))).clicked() {
                    outcome = Outcome::Close;
                }
            });
            outcome
        }
    }
}

/// Notifications stacked in the bottom-left corner.
pub fn toasts(ctx: &egui::Context, shared: &mut Shared, tab: Option<&RepoTab>) {
    const SUCCESS_TTL: Duration = Duration::from_secs(4);
    /// Operations quicker than this finish before a progress toast would help.
    const PROGRESS_AFTER: Duration = Duration::from_millis(350);

    shared.toasts.retain(|t| t.kind == ToastKind::Error || t.born.elapsed() < SUCCESS_TTL);

    // Slow operations (push, pull, fetch...) announce themselves while they
    // run, so a click is never met with silence.
    let running: Vec<&str> = tab
        .map(|tab| {
            tab.busy
                .iter()
                .filter(|op| !op.quiet && op.since.elapsed() >= PROGRESS_AFTER)
                .map(|op| op.label.as_str())
                .collect()
        })
        .unwrap_or_default();

    if shared.toasts.is_empty() && running.is_empty() {
        return;
    }
    // Wake up to expire the oldest success toast.
    if let Some(remaining) = shared
        .toasts
        .iter()
        .filter(|t| t.kind == ToastKind::Success)
        .map(|t| SUCCESS_TTL.saturating_sub(t.born.elapsed()))
        .min()
    {
        ctx.request_repaint_after(remaining + Duration::from_millis(20));
    }

    let mut dismiss: Option<usize> = None;
    Area::new(Id::new("toasts"))
        .anchor(Align2::LEFT_BOTTOM, vec2(14.0, -36.0))
        .order(Order::Foreground)
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            for label in &running {
                Frame::new()
                    .fill(theme::bg_raised())
                    .stroke(Stroke::new(1.0, theme::mix(theme::bg_raised(), theme::accent(), 0.55)))
                    .corner_radius(6.0)
                    .inner_margin(Margin::symmetric(12, 9))
                    .show(ui, |ui| {
                        ui.set_width(380.0);
                        ui.horizontal(|ui| {
                            let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), egui::Sense::hover());
                            crate::ui::widgets::spinner(ui.painter(), rect.center(), 6.5, theme::accent());
                            ui.label(
                                RichText::new(format!("{label} in progress…"))
                                    .font(theme::bold(13.0))
                                    .color(theme::text_bright()),
                            );
                        });
                    });
            }
            for (i, toast) in shared.toasts.iter().enumerate() {
                let (accent, icon) = match toast.kind {
                    ToastKind::Success => (theme::green(), icons::CHECK_CIRCLE),
                    ToastKind::Error => (theme::red(), icons::WARNING_CIRCLE),
                };
                Frame::new()
                    .fill(theme::bg_raised())
                    .stroke(Stroke::new(1.0, theme::mix(theme::bg_raised(), accent, 0.55)))
                    .corner_radius(6.0)
                    .inner_margin(Margin::symmetric(12, 9))
                    .show(ui, |ui| {
                        ui.set_width(380.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icon).size(16.0).color(accent));
                            ui.label(RichText::new(&toast.title).font(theme::bold(13.0)).color(theme::text_bright()));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if icon_button(ui, icons::X, 11.0, "").clicked() {
                                    dismiss = Some(i);
                                }
                                if toast.kind == ToastKind::Error
                                    && icon_button(ui, icons::COPY, 11.0, "Copy message").clicked()
                                {
                                    ui.ctx().copy_text(toast.body.clone());
                                }
                            });
                        });
                        if !toast.body.is_empty() {
                            // Git errors can be long; keep the toast a sane size.
                            let shown: String = toast.body.lines().take(8).collect::<Vec<_>>().join("\n");
                            ui.label(RichText::new(shown).size(12.0).color(theme::text_dim()));
                        }
                    });
            }
        });

    if let Some(i) = dismiss {
        shared.toasts.remove(i);
    }
}
