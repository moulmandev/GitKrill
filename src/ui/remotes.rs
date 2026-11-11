//! Adding, editing and removing remotes: the dialogs, the "+" on the
//! sidebar's REMOTE header, the remote's right-click menu, and the command
//! palette's entries. The git side is `git::remotes`.
//!
//! The edit and remove dialogs first ask the reader thread what the remote
//! has (`git::remotes::read`: its push URL, refspecs, the branches tracking
//! it), which the snapshot does not carry.

use std::time::Instant;

use eframe::egui::{self, Align2, Rect, Response, RichText, Sense, Spinner, TextEdit, Ui, pos2, vec2};

use crate::git::hidden::{self, REMOTES};
use crate::git::remotes::{self, Details};
use crate::git::{RemoteInfo, RepoSnapshot};
use crate::state::{Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;
use crate::ui::palette::Entry;
use crate::ui::sidebar::leaf;
use crate::ui::widgets::menu_item;

/// What [`Dialog::Remote`] holds.
pub enum Form {
    Add(AddForm),
    Edit(EditForm),
    Remove(RemoveForm),
}

pub struct AddForm {
    name: String,
    url: String,
    push_url: String,
    fetch: bool,
}

pub struct EditForm {
    /// The remote's name when the dialog opened.
    remote: String,
    /// What it has, once the reader thread has looked.
    details: Option<Result<Details, String>>,
    name: String,
    url: String,
    push_url: String,
}

pub struct RemoveForm {
    remote: String,
    details: Option<Result<Details, String>>,
}

/// Whether the dialog stays open.
pub enum Next {
    Keep,
    Close,
}

fn job_key(remote: &str) -> String {
    format!("remotes:{remote}")
}

/// Asks the reader thread what `remote` has; the dialog collects it.
fn look_up(tab: &mut RepoTab, remote: &str) {
    let name = remote.to_owned();
    tab.spawn_job(job_key(remote), move |loader| remotes::read(loader.repo(), &name));
}

/// Opens "Add remote". The first remote of a repository is called origin
/// unless the user says otherwise.
pub fn open_add(tab: &RepoTab, shared: &mut Shared) {
    let first = tab.snap.as_ref().is_some_and(|snap| snap.remotes.is_empty());
    shared.show(Dialog::Remote(Box::new(Form::Add(AddForm {
        name: if first { "origin".to_owned() } else { String::new() },
        url: String::new(),
        push_url: String::new(),
        fetch: true,
    }))));
}

pub fn open_edit(tab: &mut RepoTab, shared: &mut Shared, remote: &str) {
    look_up(tab, remote);
    shared.show(Dialog::Remote(Box::new(Form::Edit(EditForm {
        remote: remote.to_owned(),
        details: None,
        name: String::new(),
        url: String::new(),
        push_url: String::new(),
    }))));
}

pub fn open_remove(tab: &mut RepoTab, shared: &mut Shared, remote: &str) {
    look_up(tab, remote);
    shared.show(Dialog::Remote(Box::new(Form::Remove(RemoveForm { remote: remote.to_owned(), details: None }))));
}

pub fn dialog(ui: &mut Ui, form: &mut Form, tab: Option<&mut RepoTab>, fresh: bool) -> Next {
    let Some(tab) = tab else { return Next::Close };
    match form {
        Form::Add(form) => add_dialog(ui, form, tab, fresh),
        Form::Edit(form) => edit_dialog(ui, form, tab, fresh),
        Form::Remove(form) => remove_dialog(ui, form, tab),
    }
}

/// A dim caption over a field.
fn caption(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(theme::text_dim()));
}

fn field(ui: &mut Ui, value: &mut String, hint: &str, focus: bool) -> Response {
    let response = ui.add(TextEdit::singleline(value).hint_text(hint).desired_width(f32::INFINITY));
    if focus {
        response.request_focus();
    }
    response
}

fn warning(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(format!("{}  {text}", icons::WARNING)).color(theme::orange()));
}

fn note(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(theme::text_faint()));
}

/// What is wrong with `name` as the name of a remote, the remote it was
/// opened for (if any) aside. `None` when nothing is, or when it is empty:
/// the button is disabled then, without a word.
fn name_problem(name: &str, snap: Option<&RepoSnapshot>, itself: Option<&str>) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if let Err(problem) = remotes::check_name(name) {
        return Some(problem.to_owned());
    }
    let others = snap.into_iter().flat_map(|snap| &snap.remotes).map(|r| r.name.as_str());
    let other = remotes::clash(name, others.filter(|other| Some(*other) != itself))?;
    Some(if other == name {
        format!("A remote named {name} already exists.")
    } else {
        format!("Clashes with {other}: on Windows and macOS, names differing only in case share their branches.")
    })
}

/// The URL's problem, once something is typed.
fn url_problem(url: &str) -> Option<&'static str> {
    if url.is_empty() { None } else { remotes::check_url(url).err() }
}

fn add_dialog(ui: &mut Ui, form: &mut AddForm, tab: &mut RepoTab, fresh: bool) -> Next {
    dialogs::title(ui, "Add remote");
    // The name may be filled in already (origin): then the URL is next.
    let named = !form.name.is_empty();
    caption(ui, "Name");
    field(ui, &mut form.name, "origin, upstream, a colleague's name…", fresh && !named);
    dialogs::sanitize_ref(&mut form.name);
    let name = form.name.trim().to_owned();
    let name_problem = name_problem(&name, tab.snap.as_deref(), None);
    if let Some(problem) = &name_problem {
        warning(ui, problem);
    }
    caption(ui, "Fetch URL");
    field(ui, &mut form.url, "https://… or git@… or a folder", fresh && named);
    if let Some(problem) = url_problem(form.url.trim()) {
        warning(ui, problem);
    }
    caption(ui, "Push URL (optional)");
    field(ui, &mut form.push_url, "Same as the fetch URL", false);
    let push_problem = url_problem(form.push_url.trim());
    if let Some(problem) = push_problem {
        warning(ui, problem);
    }
    ui.checkbox(&mut form.fetch, "Fetch after adding");

    let ready = !name.is_empty()
        && name_problem.is_none()
        && remotes::check_url(&form.url).is_ok()
        && push_problem.is_none()
        && !tab.is_running("Add remote");
    match dialogs::buttons(ui, "Add remote", ready, false) {
        Some(true) => {
            tab.run(remotes::add(&name, form.url.trim(), form.push_url.trim(), form.fetch));
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

/// Collects what the reader thread found, once. A spinner until then; the
/// problem and a Close button if it failed. `Some` once there is something
/// to work with.
fn loaded<'a>(
    ui: &mut Ui,
    tab: &mut RepoTab,
    remote: &str,
    details: &'a mut Option<Result<Details, String>>,
    confirm: &str,
    danger: bool,
) -> Result<&'a Details, Next> {
    if details.is_none() {
        *details = tab.take_result(&job_key(remote));
    }
    match details {
        None => {
            ui.horizontal(|ui| {
                ui.add(Spinner::new().size(13.0).color(theme::accent()));
                ui.label(RichText::new(format!("Reading the settings of {remote}…")).color(theme::text_dim()));
            });
            Err(match dialogs::buttons(ui, confirm, false, danger) {
                Some(false) => Next::Close,
                _ => Next::Keep,
            })
        }
        Some(Err(problem)) => {
            warning(ui, problem);
            Err(match dialogs::buttons(ui, "Close", true, false) {
                Some(_) => Next::Close,
                None => Next::Keep,
            })
        }
        Some(Ok(details)) => Ok(details),
    }
}

fn edit_dialog(ui: &mut Ui, form: &mut EditForm, tab: &mut RepoTab, fresh: bool) -> Next {
    dialogs::title(ui, &format!("Edit remote {}", form.remote));
    let had = form.details.is_some();
    let details = match loaded(ui, tab, &form.remote, &mut form.details, "Save", false) {
        Ok(details) => details,
        Err(next) => return next,
    };
    let first = !had;
    if first {
        form.name = details.name.clone();
        form.url = details.url().to_owned();
        form.push_url = details.push_url().to_owned();
    }
    let focus = fresh || first;

    caption(ui, "Name");
    field(ui, &mut form.name, "Name", focus);
    dialogs::sanitize_ref(&mut form.name);
    let name = form.name.trim().to_owned();
    let name_problem = name_problem(&name, tab.snap.as_deref(), Some(&details.name));
    if let Some(problem) = &name_problem {
        warning(ui, problem);
    }
    caption(ui, "Fetch URL");
    field(ui, &mut form.url, "https://… or git@… or a folder", false);
    if let Some(problem) = url_problem(form.url.trim()) {
        warning(ui, problem);
    }
    if details.urls.len() > 1 {
        note(ui, &format!("{} has {} fetch URLs; this is the first.", details.name, details.urls.len()));
    }
    caption(ui, "Push URL");
    field(ui, &mut form.push_url, "Same as the fetch URL", false);
    let push_problem = url_problem(form.push_url.trim());
    if let Some(problem) = push_problem {
        warning(ui, problem);
    }
    if details.push_urls.len() > 1 {
        note(ui, &format!("{} has {} push URLs; this is the first.", details.name, details.push_urls.len()));
    }
    let renamed = name != details.name;
    let mut notes = Vec::new();
    if renamed {
        notes.push("Its remote-tracking branches are renamed, and branches tracking it follow.");
        if !details.default_refspecs() {
            notes.push("Git does not rename branches fetched by its own refspecs.");
        }
    }
    if !details.push_url().is_empty() && form.push_url.trim().is_empty() {
        notes.push("Without a push URL, pushes go to the fetch URL.");
    }
    if !notes.is_empty() {
        note(ui, &notes.join(" "));
    }

    let valid = !name.is_empty()
        && name_problem.is_none()
        && remotes::check_url(&form.url).is_ok()
        && push_problem.is_none();
    let cmd = valid.then(|| remotes::edit(details, &name, form.url.trim(), form.push_url.trim())).flatten();
    let ready = cmd.is_some() && !tab.is_running(&format!("Edit remote {}", details.name));
    match dialogs::buttons(ui, "Save", ready, false) {
        Some(true) => {
            if let Some(cmd) = cmd {
                if renamed {
                    expect_rename(ui.ctx(), tab, &cmd.label, &details.name, &name);
                }
                tab.run(cmd);
            }
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

fn remove_dialog(ui: &mut Ui, form: &mut RemoveForm, tab: &mut RepoTab) -> Next {
    dialogs::title(ui, &format!("Remove remote {}", form.remote));
    let details = match loaded(ui, tab, &form.remote, &mut form.details, "Remove", true) {
        Ok(details) => details,
        Err(next) => return next,
    };
    let name = &details.name;
    let branches = tab
        .snap
        .as_ref()
        .map_or(0, |snap| snap.remote_branches.iter().filter(|b| &b.remote == name).count());

    let url = details.url();
    let at = if url.is_empty() { String::new() } else { format!(" ({url})") };
    ui.label(format!("Removes {name}{at} from this repository. Nothing changes on the remote itself."));
    let mut goes = Vec::new();
    match branches {
        0 => {}
        1 => goes.push("Its remote-tracking branch goes too.".to_owned()),
        n => goes.push(format!("Its {n} remote-tracking branches go too.")),
    }
    match details.tracking.as_slice() {
        [] => {}
        [one] => goes.push(format!("{one} tracks it and loses its upstream.")),
        many => {
            const LISTED: usize = 4;
            let mut list = many.iter().take(LISTED).cloned().collect::<Vec<_>>().join(", ");
            if many.len() > LISTED {
                list.push_str(&format!(" and {} more", many.len() - LISTED));
            }
            goes.push(format!("{} branches track it and lose their upstream: {list}.", many.len()));
        }
    }
    for line in &goes {
        ui.label(RichText::new(format!("•  {line}")).color(theme::text()));
    }
    note(
        ui,
        if remotes::can_restore(details) {
            "Undo adds it back with the same settings and upstreams; its branches come back with the next fetch."
        } else {
            "This cannot be undone from GitKrill: the remote's URL is not in this repository's own settings."
        },
    );

    let ready = !tab.is_running(&format!("Remove remote {name}"));
    match dialogs::buttons(ui, "Remove", ready, true) {
        Some(true) => {
            tab.run(remotes::remove(details));
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

// ---- following a rename -------------------------------------------------

/// A rename handed to git and not finished yet.
#[derive(Clone)]
struct Rename {
    label: String,
    since: Instant,
    old: String,
    new: String,
}

fn renames_id(tab: &RepoTab) -> egui::Id {
    egui::Id::new(("remote-renames", &tab.path))
}

fn expect_rename(ctx: &egui::Context, tab: &RepoTab, label: &str, old: &str, new: &str) {
    let rename = Rename { label: label.to_owned(), since: Instant::now(), old: old.to_owned(), new: new.to_owned() };
    ctx.data_mut(|data| data.get_temp_mut_or_default::<Vec<Rename>>(renames_id(tab)).push(rename));
}

/// Once a rename has gone through, what was hidden of the remote and
/// whether its sidebar row was collapsed carry over to the new name. The
/// old name keeps its rules, so undoing the rename finds them in place.
fn follow_renames(ctx: &egui::Context, tab: &mut RepoTab, shared: &mut Shared) {
    let id = renames_id(tab);
    let Some(mut pending) = ctx.data(|data| data.get_temp::<Vec<Rename>>(id)) else { return };
    pending.retain(|rename| match tab.take_outcome(&rename.label, rename.since) {
        Some(true) => {
            let (from, to) = (hidden::folder(REMOTES, &rename.old), hidden::folder(REMOTES, &rename.new));
            // Hidden or not as a whole (perhaps by a folder above it), then
            // the branches inside that differ.
            let whole = tab.hidden.hides(&from);
            tab.set_hidden(&mut shared.settings, &to, whole);
            for (path, hide) in tab.hidden.rules_under(&from).into_iter().filter(|(path, _)| *path != from) {
                tab.set_hidden(&mut shared.settings, &format!("{to}{}", &path[from.len()..]), hide);
            }
            if tab.collapsed.contains(&format!("remote:{}", rename.old)) {
                tab.collapsed.insert(format!("remote:{}", rename.new));
            }
            false
        }
        Some(false) => false,
        None => true,
    });
    ctx.data_mut(|data| {
        if pending.is_empty() {
            data.remove::<Vec<Rename>>(id);
        } else {
            data.insert_temp(id, pending);
        }
    });
}

// ---- sidebar --------------------------------------------------------------

/// The "+" on the REMOTE section's header, left of its count of branches
/// (shown, total). Like the "+" of the pull request list, it shows while the
/// pointer is over the header. Called every frame the sidebar is drawn.
pub fn header_button(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, header: &Response, (shown, total): (usize, usize)) {
    follow_renames(ui.ctx(), tab, shared);
    let bar = header.rect;
    if !ui.rect_contains_pointer(bar) || ui.ctx().dragged_id().is_some() {
        return;
    }
    // Where `sidebar::section_header` painted the count.
    let count = if shown == total { total.to_string() } else { format!("{shown}/{total}") };
    let count_width = ui.painter().layout_no_wrap(count, theme::bold(11.5), theme::accent()).size().x;
    let size = vec2(22.0, 22.0);
    let plus = Rect::from_min_size(pos2(bar.right() - 12.0 - count_width - 6.0 - size.x, bar.center().y - size.y / 2.0), size);
    let response = ui.interact(plus, header.id.with("add-remote"), Sense::click());
    if ui.is_rect_visible(plus) {
        let painter = ui.painter();
        // The header behind is hovered unless the button is.
        let fill = if response.hovered() { theme::bg_raised() } else { theme::bg_hover() };
        painter.rect_filled(plus, 4.0, fill);
        let color = if response.hovered() { theme::text_bright() } else { theme::text_dim() };
        painter.text(plus.center(), Align2::CENTER_CENTER, icons::PLUS, theme::font(14.0), color);
    }
    if response.clicked() {
        open_add(tab, shared);
    }
    response.on_hover_text("Add remote");
}

/// The row an empty REMOTE section shows.
pub fn add_row(ui: &mut Ui, tab: &RepoTab, shared: &mut Shared) {
    let response = leaf(ui, 0, false, icons::PLUS, "Add a remote…", theme::text_dim(), "");
    if response.clicked() {
        open_add(tab, shared);
    }
}

/// The first items of a remote's right-click menu.
pub fn menu(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, remote: &RemoteInfo) {
    ui.set_min_width(200.0);
    let name = &remote.name;
    if menu_item(ui, icons::ARROWS_CLOCKWISE, &format!("Fetch {name}")) {
        tab.run(remotes::fetch(name));
    }
    if menu_item(ui, icons::BROOM, &format!("Prune {name}")) {
        tab.run(remotes::prune(name));
    }
    ui.separator();
    if menu_item(ui, icons::PENCIL_SIMPLE, &format!("Edit {name}…")) {
        open_edit(tab, shared, name);
    }
    if menu_item(ui, icons::COPY, "Copy URL") {
        ui.ctx().copy_text(remote.url.clone());
    }
}

/// The last item of a remote's right-click menu.
pub fn remove_item(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, remote: &RemoteInfo) {
    ui.separator();
    if menu_item(ui, icons::TRASH, &format!("Remove {}…", remote.name)) {
        open_remove(tab, shared, &remote.name);
    }
}

// ---- command palette ------------------------------------------------------

pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    list.push(
        Entry::on_tab("remote-add", icons::PLUS, "Add remote…", |tab, shared| open_add(tab, shared))
            .keywords("new remote origin upstream fork url"),
    );
    for remote in &snap.remotes {
        let name = remote.name.clone();
        if !tab.is_running("Fetch") {
            let fetched = name.clone();
            list.push(
                Entry::on_tab(format!("remote-fetch:{name}"), icons::ARROWS_CLOCKWISE, format!("Fetch {name}"), move |tab, _| {
                    tab.run(remotes::fetch(&fetched))
                })
                .detail(remote.url.clone())
                .keywords("remote download update"),
            );
        }
        let edited = name.clone();
        list.push(
            Entry::on_tab(format!("remote-edit:{name}"), icons::PENCIL_SIMPLE, format!("Edit remote {name}…"), move |tab, shared| {
                open_edit(tab, shared, &edited)
            })
            .detail(remote.url.clone())
            .keywords("rename url push set-url change"),
        );
        list.push(
            Entry::on_tab(format!("remote-remove:{name}"), icons::TRASH, format!("Remove remote {name}…"), move |tab, shared| {
                open_remove(tab, shared, &name)
            })
            .detail(remote.url.clone())
            .keywords("delete"),
        );
    }
}
