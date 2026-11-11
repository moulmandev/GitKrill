//! Tags on remotes (`git::tags`): "Push all tags" on the TAGS header's
//! menu and in the command palette, "Delete from <remote>" on a tag, and
//! the tag deletion dialog, which can delete it on remotes too.

use eframe::egui::{Checkbox, Label, RichText, Ui};

use crate::git::{RepoSnapshot, tags};
use crate::state::{Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;
use crate::ui::palette::Entry;
use crate::ui::widgets::menu_item;

/// "Push all tags to <remote>", or a submenu of remotes when there are
/// several. Nothing without tags or remotes.
pub fn header_items(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot) {
    if snap.tags.is_empty() || tab.is_running("Push all tags") {
        return;
    }
    match snap.remotes.as_slice() {
        [] => {}
        [one] => {
            if menu_item(ui, icons::CLOUD_ARROW_UP, &format!("Push all tags to {}", one.name)) {
                tab.run(tags::push_all(&one.name));
            }
        }
        many => {
            ui.menu_button(format!("{}  Push all tags to", icons::CLOUD_ARROW_UP), |ui| {
                for remote in many {
                    if ui.button(&remote.name).on_hover_text(&remote.url).clicked() {
                        tab.run(tags::push_all(&remote.name));
                    }
                }
            });
        }
    }
}

/// "Delete from <remote>…" for the tag `name`, or a submenu of remotes.
pub fn remote_items(ui: &mut Ui, shared: &mut Shared, snap: &RepoSnapshot, name: &str) {
    match snap.remotes.as_slice() {
        [] => {}
        [one] => {
            if menu_item(ui, icons::CLOUD_X, &format!("Delete from {}…", one.name)) {
                confirm_remote(shared, &one.name, name);
            }
        }
        many => {
            ui.menu_button(format!("{}  Delete from remote", icons::CLOUD_X), |ui| {
                for remote in many {
                    if ui.button(format!("{}…", remote.name)).on_hover_text(&remote.url).clicked() {
                        confirm_remote(shared, &remote.name, name);
                    }
                }
            });
        }
    }
}

fn confirm_remote(shared: &mut Shared, remote: &str, tag: &str) {
    shared.show(Dialog::confirm(
        "Delete tag from remote",
        format!(
            "Delete the tag \"{tag}\" on the remote \"{remote}\"? This affects everyone using that remote. \
             The tag stays in this repository."
        ),
        "Delete",
        tags::delete_remote(remote, tag),
    ));
}

/// Asks before deleting the tag `name` here: with remotes, the dialog also
/// offers to delete it on them.
pub fn delete(shared: &mut Shared, snap: &RepoSnapshot, name: &str) {
    if snap.remotes.is_empty() {
        shared.show(Dialog::confirm(
            "Delete tag",
            format!("Delete the tag \"{name}\" from this repository?"),
            "Delete",
            tags::delete(name, &[]),
        ));
        return;
    }
    let remotes = snap.remotes.iter().map(|r| (r.name.clone(), r.url.clone(), false)).collect();
    shared.show(Dialog::DeleteTag(Box::new(DeleteForm { tag: name.to_owned(), remotes })));
}

/// What the tag deletion dialog holds.
pub struct DeleteForm {
    tag: String,
    /// Each remote's name and URL, and whether to delete the tag there too.
    remotes: Vec<(String, String, bool)>,
}

/// Whether the dialog stays open.
pub enum Next {
    Keep,
    Close,
}

pub fn dialog(ui: &mut Ui, form: &mut DeleteForm, tab: Option<&mut RepoTab>) -> Next {
    dialogs::title(ui, "Delete tag");
    ui.label(RichText::new(format!("Delete the tag \"{}\" from this repository?", form.tag)).color(theme::text()));
    ui.add_space(2.0);
    ui.label(RichText::new("Also delete it on").color(theme::text_dim()));
    for (name, url, chosen) in &mut form.remotes {
        ui.horizontal(|ui| {
            ui.add(Checkbox::new(chosen, name.as_str()));
            ui.add(Label::new(RichText::new(url.as_str()).size(12.0).color(theme::text_faint())).truncate());
        });
    }
    let chosen: Vec<String> = form.remotes.iter().filter(|r| r.2).map(|r| r.0.clone()).collect();
    let note = match chosen.len() {
        0 => "Deleting it here only can be undone.",
        1 => "Everyone using that remote loses it too, and that cannot be undone from here. If the remote \
              does not have the tag, it is deleted here only.",
        _ => "Everyone using those remotes loses it too, and that cannot be undone from here. A remote that \
              does not have the tag is skipped.",
    };
    ui.label(RichText::new(note).size(12.0).color(theme::text_dim()));
    match dialogs::buttons(ui, "Delete", true, true) {
        Some(true) => {
            if let Some(tab) = tab {
                tab.run(tags::delete(&form.tag, &chosen));
            }
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

/// "Push all tags", once per remote.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    if snap.tags.is_empty() || tab.is_running("Push all tags") {
        return;
    }
    let n = snap.tags.len();
    for remote in &snap.remotes {
        let name = remote.name.clone();
        list.push(
            Entry::on_tab(format!("push-tags:{name}"), icons::CLOUD_ARROW_UP, "Push all tags", move |tab, _| {
                tab.run(tags::push_all(&name))
            })
            .detail(format!("{n} {} to {}", if n == 1 { "tag" } else { "tags" }, remote.name))
            .keywords("tags push remote release"),
        );
    }
}
