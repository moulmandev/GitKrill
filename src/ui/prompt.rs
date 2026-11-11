//! The dialog for what git, ssh and gpg ask while one of GitKrill's
//! commands runs (`git::prompt`): a user name and password for an HTTPS
//! remote, an SSH key's passphrase, whether to trust a new host, the
//! passphrase of the key commits are signed with.
//!
//! It is its own modal, above any other dialog, and shows the oldest open
//! question. Cancel (or Escape) answers nothing, and the command fails
//! with a sentence saying it was cancelled. When the question arrives with
//! the window in the background, the window asks for attention, and a
//! system notification says so when Preferences > Notifications has them.

use std::sync::{Mutex, PoisonError};

use eframe::egui::{
    self, Align, Frame, Id, Key, Layout, Margin, Modal, RichText, Stroke, TextEdit, Ui, text_edit::TextEditState, vec2,
};

use crate::git::prompt::{self, Asked, Kind, wipe};
use crate::state::Settings;
use crate::theme::{self, icons};
use crate::ui::dialogs::buttons;
use crate::ui::widgets::icon_button;

/// What is typed into the dialog of question `id`.
#[derive(Default)]
struct Form {
    id: u64,
    user: String,
    secret: String,
    /// The secret is shown as typed (the eye button).
    reveal: bool,
    /// The first frame: focus the first field.
    fresh: bool,
}

impl Form {
    fn clear(&mut self, ctx: &egui::Context) {
        wipe(&mut self.secret);
        wipe(&mut self.user);
        // The text fields keep an undo history of what was typed.
        ctx.data_mut(|d| {
            d.remove::<TextEditState>(secret_id(self.id));
            d.remove::<TextEditState>(user_id(self.id));
        });
    }
}

static FORM: Mutex<Option<Form>> = Mutex::new(None);

fn secret_id(id: u64) -> Id {
    Id::new("gitkrill-prompt-secret").with(id)
}

fn user_id(id: u64) -> Id {
    Id::new("gitkrill-prompt-user").with(id)
}

enum Outcome {
    Answer(String),
    Login(String, String),
    Cancel,
}

/// Shows the oldest open question, if any. Costs one atomic load while
/// none is open.
pub fn show(ctx: &egui::Context, settings: &Settings) {
    let Some(bridge) = prompt::app() else { return };
    let mut slot = FORM.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(asked) = bridge.first() else {
        if let Some(mut form) = slot.take() {
            form.clear(ctx);
        }
        return;
    };
    if slot.as_ref().is_none_or(|form| form.id != asked.id) {
        if let Some(mut old) = slot.take() {
            old.clear(ctx);
        }
        *slot = Some(Form { id: asked.id, fresh: true, ..Default::default() });
        arrived(ctx, settings, &asked);
    }
    let Some(form) = slot.as_mut() else { return };

    let frame = Frame::new()
        .fill(theme::bg_toolbar())
        .stroke(Stroke::new(1.0, theme::border_light()))
        .corner_radius(8.0)
        .inner_margin(Margin::same(20));
    let modal = Modal::new(Id::new("gitkrill-prompt").with(asked.id)).frame(frame).show(ctx, |ui| {
        ui.set_width(430.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        body(ui, &asked, form)
    });
    form.fresh = false;
    // Escape cancels; a click beside the dialog does not.
    let escape = ctx.input(|i| i.key_pressed(Key::Escape));
    let outcome = modal.inner.or(escape.then_some(Outcome::Cancel));
    let Some(outcome) = outcome else { return };
    match outcome {
        Outcome::Answer(answer) => bridge.answer(asked.id, Some(answer)),
        Outcome::Login(user, password) => bridge.answer_login(asked.id, user, password),
        Outcome::Cancel => bridge.answer(asked.id, None),
    }
    form.clear(ctx);
    *slot = None;
    ctx.request_repaint();
}

/// A new question: say so when nobody is looking.
fn arrived(ctx: &egui::Context, settings: &Settings, asked: &Asked) {
    let title = asked.question.title();
    if std::env::var_os("GITKRILL_SHOT").is_some() {
        println!("prompt: {title}");
        return;
    }
    if ctx.input(|i| i.viewport().focused).unwrap_or(true) {
        return;
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Critical));
    let notifications = &settings.appearance;
    if notifications.desktop_notifications && notifications.system_notifications {
        crate::notification::send(&title, "GitKrill is waiting for your answer.");
    }
}

fn body(ui: &mut Ui, asked: &Asked, form: &mut Form) -> Option<Outcome> {
    let question = &asked.question;
    let icon = match question.kind {
        Kind::Username { .. } | Kind::Password { .. } | Kind::SshPassword { .. } => icons::SIGN_IN,
        Kind::KeyPassphrase { .. } => icons::LOCK_KEY,
        Kind::Signing(_) => icons::SEAL_CHECK,
        Kind::HostKey { .. } => icons::FINGERPRINT,
        Kind::Confirm | Kind::Notice => icons::KEY,
        Kind::Other => icons::PASSWORD,
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(20.0).color(theme::accent()));
        ui.label(RichText::new(question.title()).font(theme::bold(16.0)).color(theme::text_bright()));
    });

    let dim = |ui: &mut Ui, text: &str| {
        ui.add(egui::Label::new(RichText::new(text).size(12.5).color(theme::text_dim())).wrap());
    };
    let confirm = match &question.kind {
        Kind::Username { url } => {
            dim(ui, &format!("Git needs a user name and password for {url}."));
            label(ui, "User name");
            let user = ui.add(TextEdit::singleline(&mut form.user).id(user_id(form.id)).desired_width(f32::INFINITY));
            if form.fresh {
                user.request_focus();
            }
            label(ui, "Password or token");
            secret(ui, form, false);
            hint(ui, "GitHub and GitLab take a personal access token as the password.");
            "Sign in"
        }
        Kind::Password { url, user } => {
            match user {
                Some(user) => dim(ui, &format!("Git needs the password of {user} for {}.", strip_user(url))),
                None => dim(ui, &format!("Git needs a password for {url}.")),
            }
            label(ui, "Password or token");
            secret(ui, form, form.fresh);
            "Sign in"
        }
        Kind::KeyPassphrase { key } => {
            match key {
                Some(key) => dim(ui, &format!("ssh needs the passphrase of {key}.")),
                None => dim(ui, "ssh needs the passphrase of your key."),
            }
            label(ui, "Passphrase");
            secret(ui, form, form.fresh);
            hint(ui, "Add the key to ssh-agent to be asked only once per session.");
            "Unlock"
        }
        Kind::Signing(signing) => {
            let who = signing.user.as_deref().unwrap_or("Your OpenPGP key");
            dim(ui, &format!("Signing needs the passphrase of {who}."));
            if !signing.key.is_empty() {
                ui.label(RichText::new(format!("Key {}", signing.key)).font(theme::mono(12.0)).color(theme::text_dim()));
            }
            if signing.retry {
                ui.label(RichText::new("That passphrase was not right. Try again.").size(12.5).color(theme::red()));
            }
            label(ui, "Passphrase or PIN");
            secret(ui, form, form.fresh);
            hint(ui, "gpg-agent keeps it for a while, so the next commits are signed without asking.");
            "Unlock"
        }
        Kind::SshPassword { .. } | Kind::Other => {
            dim(ui, &question.prompt);
            secret(ui, form, form.fresh);
            "OK"
        }
        Kind::HostKey { .. } => {
            // ssh's own words, without its question about typing yes.
            let text: Vec<&str> = question.prompt.lines().filter(|line| !line.contains("continue connecting")).collect();
            ui.add(
                egui::Label::new(RichText::new(text.join("\n")).font(theme::mono(11.5)).color(theme::text()))
                    .wrap(),
            );
            hint(ui, "Connect only if the fingerprint is the one the host publishes. It is then remembered in known_hosts.");
            "Connect"
        }
        Kind::Confirm => {
            dim(ui, &question.prompt);
            "Allow"
        }
        Kind::Notice => {
            dim(ui, &question.prompt);
            // Nothing to answer: ssh closes this once it is done.
            ui.add_space(6.0);
            let mut cancel = false;
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 28.0), Layout::right_to_left(Align::Center), |ui| {
                cancel = ui.add(egui::Button::new("Cancel").min_size(vec2(72.0, 28.0))).clicked();
                ui.add(egui::Spinner::new().size(14.0).color(theme::accent()));
            });
            return cancel.then_some(Outcome::Cancel);
        }
    };

    let ready = match question.kind {
        Kind::Username { .. } => !form.user.trim().is_empty(),
        _ => true,
    };
    match buttons(ui, confirm, ready, false)? {
        false => Some(Outcome::Cancel),
        true => Some(match &question.kind {
            Kind::Username { .. } => Outcome::Login(form.user.trim().to_owned(), std::mem::take(&mut form.secret)),
            Kind::HostKey { .. } | Kind::Confirm => Outcome::Answer("yes".to_owned()),
            _ => Outcome::Answer(std::mem::take(&mut form.secret)),
        }),
    }
}

fn label(ui: &mut Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(text).size(12.5).color(theme::text()));
}

fn hint(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(11.5).color(theme::text_faint())).wrap());
}

/// The masked field, with an eye to show what was typed.
fn secret(ui: &mut Ui, form: &mut Form, focus: bool) {
    ui.horizontal(|ui| {
        let width = ui.available_width() - 30.0;
        let field = TextEdit::singleline(&mut form.secret)
            .id(secret_id(form.id))
            .password(!form.reveal)
            .desired_width(width);
        let response = ui.add(field);
        if focus {
            response.request_focus();
        }
        let (icon, tip) = if form.reveal { (icons::EYE_SLASH, "Hide") } else { (icons::EYE, "Show what was typed") };
        if icon_button(ui, icon, 14.0, tip).clicked() {
            form.reveal = !form.reveal;
        }
    });
}

/// `https://ada@example.com` without the user, which the text names.
fn strip_user(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_owned() };
    let end = rest.find('/').unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn urls_lose_their_user_name() {
        assert_eq!(super::strip_user("https://ada@example.com"), "https://example.com");
        assert_eq!(super::strip_user("https://ada@example.com/a@b"), "https://example.com/a@b");
        assert_eq!(super::strip_user("https://example.com/a@b"), "https://example.com/a@b");
        assert_eq!(super::strip_user("example.com"), "example.com");
    }
}
