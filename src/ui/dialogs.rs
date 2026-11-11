//! Modal dialogs and toast notifications.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Area, Button, Frame, Id, Key, Layout, Margin, Modal, Order,
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
    /// Keep, and ignore Escape and clicks outside: the dialog is working.
    Hold,
}

pub fn show(ctx: &egui::Context, shared: &mut Shared, mut tab: Option<&mut RepoTab>) {
    let Some(mut dialog) = shared.dialog.take() else { return };
    let fresh = std::mem::take(&mut shared.dialog_fresh);
    let width = match dialog {
        Dialog::NewPull(_) => 520.0,
        Dialog::Update(_) | Dialog::GitSetup | Dialog::DeleteFolder(_) | Dialog::Ignore(_) => 460.0,
        _ => 400.0,
    };

    let frame = Frame::new()
        .fill(theme::bg_toolbar())
        .stroke(Stroke::new(1.0, theme::border_light()))
        .corner_radius(8.0)
        .inner_margin(Margin::same(20));
    // A dialog that follows another in the next frame (a confirmation)
    // gets a new modal: the old one's remembered size would stretch it.
    let generation_id = Id::new("gitkrill-dialog-generation");
    let generation: u64 = ctx.data(|d| d.get_temp(generation_id)).unwrap_or(0);
    let modal = Modal::new(Id::new("gitkrill-dialog").with(generation)).frame(frame).show(ctx, |ui| {
        ui.set_width(width);
        ui.spacing_mut().item_spacing.y = 8.0;
        body(ui, &mut dialog, shared, tab.as_deref_mut(), fresh)
    });

    let close = match modal.inner {
        Outcome::Close => true,
        Outcome::Hold => false,
        Outcome::Keep => modal.should_close(),
    };
    // A dialog may have queued a follow-up; never overwrite that.
    let replaced = shared.dialog.is_some();
    if !close && !replaced {
        shared.dialog = Some(dialog);
    } else {
        if replaced {
            ctx.data_mut(|d| d.insert_temp(generation_id, generation + 1));
        }
        if let Dialog::NewPull(form) = &dialog {
            crate::ui::new_pull::dropped(form, tab);
        }
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
        let fill = if danger { theme::solid(theme::red(), 0.7) } else { theme::solid(theme::accent(), 0.8) };
        let primary = Button::new(RichText::new(confirm).color(theme::on_accent()))
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
pub(crate) fn sanitize_ref(name: &mut String) {
    if name.contains(' ') {
        *name = name.replace(' ', "-");
    }
}

pub(crate) fn valid_ref(name: &str) -> bool {
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

        Dialog::Stash(form) => match crate::ui::stash::dialog(ui, form, tab, fresh) {
            crate::ui::stash::Next::Keep => Outcome::Keep,
            crate::ui::stash::Next::Close => Outcome::Close,
        },
        Dialog::Ignore(form) => match crate::ui::ignore::dialog(ui, form, shared, tab, fresh) {
            crate::ui::ignore::Next::Keep => Outcome::Keep,
            crate::ui::ignore::Next::Close => Outcome::Close,
        },

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
                let pull = Button::new(RichText::new("Pull (rebase)").color(theme::on_accent()))
                    .fill(theme::solid(theme::accent(), 0.8))
                    .min_size(vec2(96.0, 28.0));
                if ui.add(pull).clicked() {
                    tab.run(ops::pull(PullMode::Rebase));
                    outcome = Outcome::Close;
                }
                if ui.add(Button::new("Pull").min_size(vec2(64.0, 28.0))).clicked() {
                    tab.run(ops::pull(PullMode::Default));
                    outcome = Outcome::Close;
                }
                let force = Button::new(RichText::new("Force push").color(theme::on_accent()))
                    .fill(theme::solid(theme::red(), 0.7))
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
        Dialog::NewPull(form) => match crate::ui::new_pull::show(ui, form, shared, tab, fresh) {
            crate::ui::new_pull::Next::Keep => Outcome::Keep,
            crate::ui::new_pull::Next::Close => Outcome::Close,
            crate::ui::new_pull::Next::Hold => Outcome::Hold,
        },

        Dialog::GitflowStart(form) => match crate::ui::gitflow::start_dialog(ui, form, tab, fresh) {
            crate::ui::gitflow::Next::Keep => Outcome::Keep,
            crate::ui::gitflow::Next::Close => Outcome::Close,
        },
        Dialog::GitflowFinish(form) => match crate::ui::gitflow::finish_dialog(ui, form, tab) {
            crate::ui::gitflow::Next::Keep => Outcome::Keep,
            crate::ui::gitflow::Next::Close => Outcome::Close,
        },
        Dialog::DeleteFolder(form) => match crate::ui::branch_folder::dialog(ui, form, tab) {
            crate::ui::branch_folder::Next::Keep => Outcome::Keep,
            crate::ui::branch_folder::Next::Close => Outcome::Close,
        },

        Dialog::Update(release) => {
            let release = release.clone();
            match crate::ui::update::dialog(ui, shared, &release) {
                true => Outcome::Close,
                false => Outcome::Keep,
            }
        }

        Dialog::GitSetup => match crate::ui::git_setup::dialog(ui, shared) {
            true => Outcome::Close,
            // An install keeps running if the dialog goes; Escape may close it.
            false => Outcome::Keep,
        },

        Dialog::About => {
            title(ui, "GitKrill");
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
            let git = match shared.git.state() {
                crate::git_setup::State::Found(version) => format!("Uses {version}, {}", crate::git::cli::program().display()),
                crate::git_setup::State::Checking => "Looking for Git…".to_owned(),
                _ => "Git was not found".to_owned(),
            };
            ui.label(RichText::new(git).color(theme::text_dim()));
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
    /// Long enough to reach for the button of a toast that has one.
    const LINK_TTL: Duration = Duration::from_secs(12);
    let ttl = |toast: &crate::state::Toast| if toast.link.is_some() { LINK_TTL } else { SUCCESS_TTL };
    /// Operations quicker than this finish before a progress toast would help.
    const PROGRESS_AFTER: Duration = Duration::from_millis(350);

    shared.toasts.retain(|t| t.kind == ToastKind::Error || t.born.elapsed() < ttl(t));

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
        .map(|t| ttl(t).saturating_sub(t.born.elapsed()))
        .min()
    {
        ctx.request_repaint_after(remaining + Duration::from_millis(20));
    }

    if shared.settings.appearance.desktop_notifications {
        notify(ctx, &shared.toasts);
    }

    // Preferences > UI Customization > Notification Location. At the top,
    // below the tab strip and the toolbar; at the bottom, above the status
    // bar.
    use crate::prefs::appearance::Corner;
    let toolbar = if tab.is_some() && shared.prefs.is_none() { crate::ui::toolbar::height(&shared.settings) } else { 0.0 };
    let top = 24.0 + 34.0 + toolbar + 12.0;
    let (anchor, offset) = match shared.settings.appearance.toast_corner {
        Corner::BottomLeft => (Align2::LEFT_BOTTOM, vec2(14.0, -36.0)),
        Corner::BottomRight => (Align2::RIGHT_BOTTOM, vec2(-14.0, -36.0)),
        Corner::TopLeft => (Align2::LEFT_TOP, vec2(14.0, top)),
        Corner::TopRight => (Align2::RIGHT_TOP, vec2(-14.0, top)),
    };

    let mut dismiss: Option<usize> = None;
    Area::new(Id::new("toasts"))
        .anchor(anchor, offset)
        .order(Order::Foreground)
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            for label in &running {
                Frame::new()
                    .fill(theme::bg_float())
                    .stroke(Stroke::new(1.0, theme::mix(theme::bg_float(), theme::accent(), 0.55)))
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
                    .fill(theme::bg_float())
                    .stroke(Stroke::new(1.0, theme::mix(theme::bg_float(), accent, 0.55)))
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
                        if let Some((label, url)) = &toast.link {
                            let text = format!("{}  {label}", icons::ARROW_SQUARE_OUT);
                            if ui.button(text).on_hover_text(url).clicked() {
                                crate::ui::hosting::open_link(url);
                            }
                        }
                    });
            }
        });

    if let Some(i) = dismiss {
        shared.toasts.remove(i);
    }
}

/// Preferences > Notifications: when toasts appeared since the last frame
/// while the window is in the background, asks the window manager for the
/// user's attention (the taskbar button flashes until the window is
/// focused; the dock icon bounces). Operations that end raise a toast in
/// the frame they end, so this is where they are noticed; nothing polls.
fn notify(ctx: &egui::Context, toasts: &[crate::state::Toast]) {
    let id = Id::new("toasts-notified");
    let Some(newest) = toasts.iter().map(|t| t.born).max() else { return };
    let seen: Option<Instant> = ctx.data(|d| d.get_temp(id));
    if seen.is_some_and(|seen| newest <= seen) {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(id, newest));
    if ctx.input(|i| i.viewport().focused).unwrap_or(true) {
        return;
    }
    let failed = toasts.iter().any(|t| t.kind == ToastKind::Error && seen.is_none_or(|seen| t.born > seen));
    let kind = if failed { egui::UserAttentionType::Critical } else { egui::UserAttentionType::Informational };
    // A scripted run's window is never focused: say it instead of flashing
    // the taskbar of whoever uses the machine.
    if std::env::var_os("GITKRILL_SHOT").is_some() {
        println!("attention requested: {kind:?}");
        return;
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(kind));
}
