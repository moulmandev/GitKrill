//! Preferences > SSH: how git reaches remotes over SSH for the profile in
//! use (the system's agent, or a key of the profile's own), and making a
//! new key pair.
//!
//! Key files are checked, read and made on background threads.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText, Ui};

use super::{Page, form, profiles};
use crate::prefs::identity;
use crate::state::ToastKind;
use crate::theme::{self, icons};
use crate::ui::widgets;

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    /// The (private, public) paths last checked, and whether each is a file
    /// (`None` while the check runs).
    checked: Option<(String, String)>,
    found: Option<(bool, bool)>,
    checking: Option<Receiver<(bool, bool)>>,
    /// A key pair being made: its (private, public) paths.
    generating: Option<Receiver<Result<(String, String), String>>>,
    /// The public key being read for the clipboard.
    copying: Option<Receiver<Result<String, String>>>,
}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    profiles::poll_failure(page.shared);
    if !profiles::ensure_profile(ui, page) {
        return;
    }
    poll(page, state);
    profiles::profile_note(ui, &page.shared.settings.identity, false);

    let ctx = page.ctx.clone();
    let mut copy = None;
    let mut generate = None;
    let Some(profile) = page.shared.settings.identity.current_mut() else { return };
    let email = profile.author_email.clone();
    let ssh = &mut profile.ssh;

    form::row(ui, "Use local SSH agent", |ui| {
        form::checkbox(ui, &mut ssh.use_agent, "");
        form::help(
            ui,
            if ssh.use_agent {
                "Git uses your system's ssh, with its agent and ~/.ssh/config."
            } else {
                "GitKrill's git commands use the key below (and still read ~/.ssh/config)."
            },
        );
    });

    if !ssh.use_agent {
        let paths = (ssh.private_key.clone(), ssh.public_key.clone());
        if state.checked.as_ref() != Some(&paths) {
            state.found = None;
            state.checking = Some(check_files(&ctx, paths.clone()));
            state.checked = Some(paths);
        }
        let found = state.found;

        form::row(ui, "SSH Private Key", |ui| {
            profiles::draft_path(ui, "ssh-private", &mut ssh.private_key, "~/.ssh/id_ed25519", "SSH private key");
            if ssh.private_key.trim().is_empty() {
                form::help(ui, "Without a key, git uses your system's ssh as with the agent.");
            } else if found.is_some_and(|(private, _)| !private) {
                form::error(ui, "This file does not exist.");
            }
        });
        form::row(ui, "SSH Public Key", |ui| {
            ui.horizontal(|ui| {
                profiles::draft_path(ui, "ssh-public", &mut ssh.public_key, "~/.ssh/id_ed25519.pub", "SSH public key");
                let can_copy = !ssh.public_key.trim().is_empty() && state.copying.is_none();
                let button = ui.add_enabled_ui(can_copy, |ui| widgets::icon_button(ui, icons::CLIPBOARD_TEXT, 15.0, "Copy the public key")).inner;
                if button.clicked() && can_copy {
                    copy = Some(ssh.public_key.clone());
                }
            });
            if !ssh.public_key.trim().is_empty() && found.is_some_and(|(_, public)| !public) {
                form::error(ui, "This file does not exist.");
            } else {
                form::help(ui, "Add it to your Git host (GitHub, GitLab…) so it lets this key in.");
            }
        });
    }

    form::row(ui, "Generate new Private/Public key", |ui| {
        if state.generating.is_some() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(13.0).color(theme::accent()));
                ui.label(RichText::new("Generating…").color(theme::text_dim()));
            });
        } else if form::button(ui, "Generate", theme::green()).clicked() {
            generate = Some(email);
        }
        form::help(ui, "This will create new keys and configure GitKrill to use them");
    });

    if let Some(path) = copy {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let read = std::fs::read_to_string(expand_home(&path)).map_err(|e| format!("Could not read {path}: {e}"));
            let _ = tx.send(read);
            ctx.request_repaint();
        });
        state.copying = Some(rx);
    }
    if let Some(email) = generate {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let made = identity::generate_ssh_key(&identity::ssh_dir(), &email)
                .map(|(private, public)| (private.display().to_string(), public.display().to_string()));
            let _ = tx.send(made);
            ctx.request_repaint();
        });
        state.generating = Some(rx);
    }
}

/// Collects what the background threads found.
fn poll(page: &mut Page<'_>, state: &mut State) {
    if let Some(found) = state.checking.as_ref().and_then(|rx| rx.try_recv().ok()) {
        state.found = Some(found);
        state.checking = None;
    }
    if let Some(read) = state.copying.as_ref().and_then(|rx| rx.try_recv().ok()) {
        state.copying = None;
        match read {
            Ok(key) => {
                page.ctx.copy_text(key.trim().to_owned());
                page.shared.toast(ToastKind::Success, "Public key copied", "Paste it in your Git host's SSH key settings.");
            }
            Err(e) => page.shared.toast(ToastKind::Error, "Could not copy the public key", e),
        }
    }
    if let Some(made) = state.generating.as_ref().and_then(|rx| rx.try_recv().ok()) {
        state.generating = None;
        match made {
            Ok((private, public)) => {
                if let Some(profile) = page.shared.settings.identity.current_mut() {
                    profile.ssh = identity::Ssh { use_agent: false, private_key: private, public_key: public.clone() };
                }
                state.checked = None;
                page.shared.toast(ToastKind::Success, "SSH key created", format!("Add {public} to your Git host."));
            }
            Err(e) => page.shared.toast(ToastKind::Error, "Could not create an SSH key", e),
        }
    }
}

/// Whether the private and public key files exist, found in the background.
fn check_files(ctx: &egui::Context, (private, public): (String, String)) -> Receiver<(bool, bool)> {
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let exists = |path: &str| path.trim().is_empty() || expand_home(path.trim()).is_file();
        let _ = tx.send((exists(&private), exists(&public)));
        ctx.request_repaint();
    });
    rx
}

/// `~/x` as ssh reads it: in the home folder.
fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    }
}
