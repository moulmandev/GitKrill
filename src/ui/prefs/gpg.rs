//! Preferences > GPG: commit and tag signing for the profile in use (format,
//! key, sign by default), the program git signs with, and making a new
//! OpenPGP key.
//!
//! The signing settings belong to the profile and reach git as the
//! "Keep my .gitconfig updated" choice says (see `crate::prefs::identity`).
//! The program is a setting of this machine rather than of a profile, kept
//! in the global git config. Keys are listed and made on background
//! threads.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, ComboBox, RichText, TextEdit, Ui, vec2};

use super::{Page, form, profiles};
use crate::prefs::identity::{self, SecretKey, SignFormat};
use crate::state::ToastKind;
use crate::theme::{self, icons};
use crate::ui::widgets;

type Keys = Result<Vec<SecretKey>, String>;

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    /// The program setting of each format as shown, read from the global
    /// config once and then kept as changed here (the write runs in the
    /// background).
    programs: HashMap<SignFormat, String>,
    /// What the key list was asked for (format and program), and the answer.
    listed: Option<(SignFormat, String)>,
    keys: Option<Keys>,
    listing: Option<Receiver<Keys>>,
    passphrase: String,
    /// A key being made: its id.
    generating: Option<Receiver<Result<String, String>>>,
}

const RENEW_URL: &str = "https://docs.github.com/en/authentication/managing-commit-signature-verification/updating-an-expired-gpg-key";

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    profiles::poll_failure(page.shared);
    if !profiles::ensure_profile(ui, page) {
        return;
    }
    page.global.load(page.ctx);
    poll(page, state);
    profiles::profile_note(ui, &page.shared.settings.identity, true);

    let ctx = page.ctx.clone();
    let ready = page.global.ready();
    for format in SignFormat::ALL {
        if ready && !state.programs.contains_key(&format) {
            let value = page.global.get(format.program_key()).unwrap_or("").to_owned();
            state.programs.insert(format, value);
        }
    }
    let identity = &mut page.shared.settings.identity;
    let Some(profile) = identity.current_mut() else { return };
    let ssh_public_key = profile.ssh.public_key.clone();
    let (author_name, author_email) = (profile.author_name.clone(), profile.author_email.clone());
    let signing = &mut profile.signing;
    let format = signing.format;

    form::row(ui, "GPG Format", |ui| {
        let options: Vec<(SignFormat, &str)> = SignFormat::ALL.iter().map(|f| (*f, f.label())).collect();
        form::choice(ui, "gpg-format", &mut signing.format, &options);
        form::help(
            ui,
            match signing.format {
                SignFormat::OpenPgp => "Sign with a GnuPG key.",
                SignFormat::Ssh => "Sign with an SSH key (git 2.34 or later). Your Git host must know it as a signing key.",
                SignFormat::X509 => "Sign with an X.509 certificate through gpgsm (S/MIME).",
            },
        );
    });
    let format_changed = signing.format != format;
    let format = signing.format;
    if format_changed {
        // A key of one format means nothing to another; SSH starts from the
        // profile's own SSH key.
        signing.key = if format == SignFormat::Ssh { ssh_public_key.trim().to_owned() } else { String::new() };
    }

    let (label, title) = match format {
        SignFormat::OpenPgp => ("GPG Program", "GPG program"),
        SignFormat::Ssh => ("SSH Signing Program", "SSH signing program"),
        SignFormat::X509 => ("X.509 Signing Program", "X.509 signing program"),
    };
    let mut relist = format_changed;
    form::row(ui, label, |ui| {
        let Some(program) = state.programs.get_mut(&format) else {
            form::loading(ui);
            return;
        };
        if profiles::draft_path(ui, format.program_key(), program, format.default_program(), title) {
            let value = program.trim();
            identity::set_global(&ctx, format.program_key(), (!value.is_empty()).then_some(value));
            relist = true;
        }
        form::help(
            ui,
            match format {
                SignFormat::OpenPgp => {
                    "This is usually \"gpg\" or \"gpg2\" depending on the version you are using. You can also browse for \
                     the path directly. Empty: the gpg that comes with Git."
                }
                SignFormat::Ssh => "This is usually \"ssh-keygen\". You can also browse for the path directly.",
                SignFormat::X509 => "This is usually \"gpgsm\". You can also browse for the path directly.",
            },
        );
        form::help(ui, &format!("Saved in your global Git config ({}), for every profile.", format.program_key()));
    });

    let program = state.programs.get(&format).cloned();
    if format != SignFormat::Ssh
        && let Some(program) = &program
        && (relist || state.listed.as_ref() != Some(&(format, program.clone())))
    {
        list_keys(&ctx, state, format, program);
    }

    form::row(ui, "Signing Key", |ui| {
        if format == SignFormat::Ssh {
            profiles::draft_path(ui, "gpg-ssh-key", &mut signing.key, "~/.ssh/id_ed25519.pub", "SSH public key to sign with");
            let profile_key = ssh_public_key.trim();
            if !profile_key.is_empty() && signing.key.trim() != profile_key && form::button(ui, "Use this profile's SSH key", theme::accent()).clicked() {
                signing.key = profile_key.to_owned();
            }
            form::help(ui, "The public key of the SSH key to sign with. Its private key must be next to it, or in your SSH agent.");
            return;
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let keys = state.keys.as_ref().and_then(|k| k.as_ref().ok()).map(Vec::as_slice).unwrap_or_default();
            key_choice(ui, &mut signing.key, keys);
            if state.listing.is_some() {
                ui.add(egui::Spinner::new().size(14.0).color(theme::accent()));
            } else if widgets::icon_button(ui, icons::ARROWS_CLOCKWISE, 15.0, "List the keys again").clicked()
                && let Some(program) = &program
            {
                list_keys(&ctx, state, format, program);
            }
        });
        match &state.keys {
            Some(Err(e)) => form::error(ui, e),
            Some(Ok(keys)) if keys.is_empty() => form::help(ui, "No secret keys yet. Generate one below, or import yours into GnuPG."),
            Some(Ok(keys)) => {
                if keys.iter().any(|k| k.matches(&signing.key) && (k.expired || k.revoked)) {
                    form::warning(ui, "This key has expired: commits signed with it will not show as verified.");
                }
                if keys.iter().any(|k| k.expired) {
                    form::link(ui, "How do I renew an expired key?", RENEW_URL);
                }
            }
            None => {}
        }
        if signing.key.is_empty() {
            form::help(ui, "None: gpg picks the key that matches your author email.");
        }
    });

    if format == SignFormat::OpenPgp {
        let mut generate = false;
        form::row(ui, "Generate new GPG Key", |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let field = TextEdit::singleline(&mut state.passphrase)
                    .password(true)
                    .hint_text("Passphrase (optional)")
                    .desired_width(form::FIELD_WIDTH - 78.0)
                    .margin(vec2(8.0, 5.0));
                ui.add_enabled(state.generating.is_none(), field);
                if state.generating.is_some() {
                    ui.add(egui::Spinner::new().size(14.0).color(theme::accent()));
                    ui.label(RichText::new("Generating…").color(theme::text_dim()));
                } else if form::button(ui, "Generate", theme::green()).clicked() {
                    generate = true;
                }
            });
            form::help(
                ui,
                "This will create a 4096 bit key with the name and email in your current profile that expires in 2 \
                 years. This will also be set as your active signing key.",
            );
            if !state.passphrase.is_empty() {
                form::warning(ui, "GitKrill cannot ask for a passphrase when it signs: keep gpg-agent unlocked, or leave this empty.");
            }
        });
        if generate && let Some(program) = &program {
            let (tx, rx) = channel();
            let (program, passphrase, ctx) = (program.clone(), std::mem::take(&mut state.passphrase), ctx.clone());
            std::thread::spawn(move || {
                let _ = tx.send(identity::generate_gpg_key(&program, &author_name, &author_email, &passphrase));
                ctx.request_repaint();
            });
            state.generating = Some(rx);
        }
    }

    form::row(ui, "Sign Commits by Default", |ui| {
        form::checkbox(ui, &mut signing.sign_commits, "");
        if signing.sign_commits && format == SignFormat::Ssh && signing.key.trim().is_empty() {
            form::warning(ui, "SSH signing needs a signing key.");
        }
    });
    form::row(ui, "Sign Tags by Default", |ui| {
        form::checkbox(ui, &mut signing.sign_tags, "");
        form::help(ui, "Annotated tags (with a message) are signed.");
    });
}

/// Starts listing the secret keys of `format` with `program`.
fn list_keys(ctx: &egui::Context, state: &mut State, format: SignFormat, program: &str) {
    let (tx, rx) = channel();
    let (ctx, program_owned) = (ctx.clone(), program.to_owned());
    std::thread::spawn(move || {
        let _ = tx.send(identity::list_secret_keys(format, &program_owned));
        ctx.request_repaint();
    });
    state.listed = Some((format, program.to_owned()));
    state.listing = Some(rx);
}

/// Collects listed and generated keys.
fn poll(page: &mut Page<'_>, state: &mut State) {
    if let Some(keys) = state.listing.as_ref().and_then(|rx| rx.try_recv().ok()) {
        state.keys = Some(keys);
        state.listing = None;
    }
    if let Some(made) = state.generating.as_ref().and_then(|rx| rx.try_recv().ok()) {
        state.generating = None;
        match made {
            Ok(id) => {
                if let Some(profile) = page.shared.settings.identity.current_mut() {
                    profile.signing.format = SignFormat::OpenPgp;
                    profile.signing.key = id.clone();
                }
                page.shared.toast(ToastKind::Success, "GPG key created", format!("{id} is now your signing key."));
                // List again, with the new key.
                state.listed = None;
            }
            Err(e) => page.shared.toast(ToastKind::Error, "Could not create a GPG key", e),
        }
    }
}

/// The signing key drop-down: "<None>" and every secret key, expired ones
/// in red. A configured value that is not in the list shows as it is.
fn key_choice(ui: &mut Ui, key: &mut String, keys: &[SecretKey]) {
    let selected = if key.is_empty() {
        "<None>".to_owned()
    } else {
        keys.iter().find(|k| k.matches(key)).map(key_label).unwrap_or_else(|| key.clone())
    };
    let width = form::FIELD_WIDTH - 34.0;
    // A long key label is cut to the field; the list shows it whole.
    ui.scope(|ui| {
        ui.set_max_width(width);
        ComboBox::from_id_salt("gpg-signing-key").selected_text(selected).width(width).truncate().show_ui(ui, |ui| {
            ui.set_min_width(480.0);
            if ui.selectable_label(key.is_empty(), "<None>").clicked() {
                key.clear();
            }
            for k in keys {
                let color = if k.expired || k.revoked { theme::red() } else { theme::text() };
                if ui.selectable_label(k.matches(key), RichText::new(key_label(k)).color(color)).clicked() {
                    *key = k.id.clone();
                }
            }
        });
    });
}

/// "3AA5C34371567BD2  Ada <ada@example.com>  (expires Nov 9, 2028)".
fn key_label(key: &SecretKey) -> String {
    let date = |secs: i64| {
        use chrono::{Local, TimeZone};
        Local.timestamp_opt(secs, 0).single().map(|t| t.format("%b %-d, %Y").to_string()).unwrap_or_default()
    };
    let status = match (key.revoked, key.expired, key.expires) {
        (true, _, _) => "revoked".to_owned(),
        (_, true, Some(t)) => format!("expired {}", date(t)),
        (_, true, None) => "expired".to_owned(),
        (_, false, Some(t)) => format!("expires {}", date(t)),
        (_, false, None) => "no expiry".to_owned(),
    };
    format!("{}  {}  ({status})", key.id, key.user_id)
}
