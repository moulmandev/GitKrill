//! The commit panel's options for the next commit (`git::trailers`): sign
//! off, skip the hooks, co-authors. A button beside "Amend" opens them;
//! whatever is on shows as a row of chips under the description, each
//! removable with its ×. They apply to whatever the commit button does
//! (commit, amend, commit and push, commit and open a pull request), and go
//! back to their defaults once a commit succeeded: signed off as
//! Preferences > Commit says, hooks run, no co-authors.
//!
//! Co-authors are picked from the people in the history (`git shortlog`,
//! read in the background the first time the options open), filtered as
//! you type, or typed as `Name <email>`.
//!
//! Like `commit_button`, this keeps its state in egui's memory by working
//! directory: the choices, who git signs off as (asked once per identity,
//! so a message already signed off is not signed twice) and the people.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, CursorIcon, Id, Key, Modifiers, Popup, PopupCloseBehavior, Rect, RectAlign, RichText,
    Sense, Stroke, StrokeKind, TextEdit, Ui, pos2, vec2,
};

use crate::git::trailers::{self, CommitOptions, Person};
use crate::prefs::commit::RepoCommit;
use crate::prefs::identity;
use crate::state::RepoTab;
use crate::theme::{self, icons};
use crate::ui::widgets;

const PEOPLE_JOB: &str = "commit-options:people";
const IDENT_JOB: &str = "commit-options:ident";
/// People listed under the co-author field.
const SUGGESTIONS: usize = 8;
/// Co-authors remembered as picked lately, offered first.
const RECENT: usize = 8;

/// The options of one repository's commit panel.
#[derive(Clone, Default)]
struct State {
    /// `None`: as Preferences > Commit says.
    sign_off: Option<bool>,
    no_verify: bool,
    co_authors: Vec<Person>,
    /// Typed in the co-author field.
    query: String,
    /// The suggestion Enter picks.
    selected: usize,
    /// Put the keyboard in the co-author field (the options just opened).
    focus: bool,
    /// Who git signs off as, for the identity generation it was asked under
    /// (`None` inside while the answer is on its way).
    ident: Option<(u64, Option<String>)>,
    /// The people of the history, most commits first; `None` until read.
    people: Option<Arc<Vec<(Person, usize)>>>,
    reading: bool,
    /// Co-authors picked lately, most recent first.
    recent: Vec<Person>,
}

fn state_id(path: &Path) -> Id {
    Id::new(("commit-options", path))
}

fn load(ctx: &egui::Context, path: &Path) -> State {
    ctx.data(|d| d.get_temp(state_id(path))).unwrap_or_default()
}

fn save(ctx: &egui::Context, path: &Path, state: State) {
    ctx.data_mut(|d| d.insert_temp(state_id(path), state));
}

/// The options the next commit gets.
pub fn options(ctx: &egui::Context, tab: &RepoTab, prefs: &RepoCommit) -> CommitOptions {
    let state = load(ctx, &tab.path);
    CommitOptions {
        sign_off: state.sign_off.unwrap_or(prefs.sign_off),
        no_verify: state.no_verify,
        co_authors: state.co_authors,
    }
}

/// Who git signs off as (`Name <email>`), when it has been asked for the
/// identity in use.
pub fn sign_off_ident(ctx: &egui::Context, path: &Path) -> Option<String> {
    let (generation, ident) = load(ctx, path).ident?;
    if generation == identity::generation() { ident } else { None }
}

/// A commit succeeded: the options go back to their defaults.
pub fn committed(ctx: &egui::Context, path: &Path) {
    let mut state = load(ctx, path);
    state.sign_off = None;
    state.no_verify = false;
    state.co_authors.clear();
    state.query.clear();
    save(ctx, path, state);
}

/// Collects the answers of the background jobs, and starts them when
/// needed: who git signs off as while signing off, the people of the
/// history while the co-author field shows.
fn poll(tab: &mut RepoTab, state: &mut State, signing: bool, people: bool) {
    if let Some(answer) = tab.take_result::<(u64, Option<String>)>(IDENT_JOB) {
        state.ident = Some(answer);
    }
    if let Some(list) = tab.take_result::<Vec<(Person, usize)>>(PEOPLE_JOB) {
        state.people = Some(Arc::new(list));
        state.reading = false;
    }
    let generation = identity::generation();
    if signing && state.ident.as_ref().is_none_or(|(asked, _)| *asked != generation) {
        state.ident = Some((generation, None));
        let dir = tab.path.clone();
        tab.spawn_job(IDENT_JOB, move |_| {
            let ident = crate::git::cli::run(&dir, &trailers::ident_args(), Default::default()).ok();
            (generation, ident.as_deref().and_then(trailers::ident_of))
        });
    }
    if people && state.people.is_none() && !state.reading {
        read_people(tab, state);
    }
}

fn read_people(tab: &mut RepoTab, state: &mut State) {
    state.reading = true;
    let dir: PathBuf = tab.path.clone();
    tab.spawn_job(PEOPLE_JOB, move |_| trailers::authors(&dir).unwrap_or_default());
}

// ---- the button and its menu ------------------------------------------------

/// The options button, for the row of the "Amend" box: tinted while any
/// option is on. Opens the options.
pub fn button(ui: &mut Ui, tab: &mut RepoTab, prefs: &RepoCommit) {
    let ctx = ui.ctx().clone();
    let mut state = load(&ctx, &tab.path);
    let active = !options(&ctx, tab, prefs).is_plain();

    let (rect, response) = ui.allocate_exact_size(vec2(24.0, 22.0), Sense::click());
    let open = Popup::is_id_open(&ctx, Popup::default_response_id(&response));
    if ui.is_rect_visible(rect) {
        if open || response.hovered() {
            ui.painter().rect_filled(rect, 4.0, theme::bg_raised());
        }
        let color = if active { theme::accent() } else if response.hovered() { theme::text_bright() } else { theme::text_dim() };
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icons::SLIDERS_HORIZONTAL, theme::font(14.0), color);
    }
    let response = response
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Commit options: sign off, skip hooks, co-authors");
    if response.clicked() && !open {
        // Opening: the keyboard goes to the co-author field, and the people
        // are read again in the background (the old list shows meanwhile).
        state.focus = true;
        state.query.clear();
        if !state.reading {
            read_people(tab, &mut state);
        }
        save(&ctx, &tab.path, state);
    }

    // Upwards: the button sits at the bottom of the window, and the menu
    // grows as people are added.
    let popup = Popup::menu(&response)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .align(RectAlign::TOP_END)
        .align_alternatives(&[RectAlign::TOP_START, RectAlign::BOTTOM_END]);
    popup.show(|ui| {
        ui.set_width(310.0);
        let mut state = load(&ctx, &tab.path);
        let signing = state.sign_off.unwrap_or(prefs.sign_off);
        poll(tab, &mut state, signing, true);
        menu(ui, tab, prefs, &mut state);
        save(&ctx, &tab.path, state);
    });
}

fn menu(ui: &mut Ui, tab: &RepoTab, prefs: &RepoCommit, state: &mut State) {
    ui.label(RichText::new("Options for the next commit").size(11.5).color(theme::text_dim()));
    ui.add_space(2.0);

    let mut sign_off = state.sign_off.unwrap_or(prefs.sign_off);
    let ident = state.ident.as_ref().and_then(|(_, ident)| ident.clone());
    let signed_as = match &ident {
        Some(ident) => format!("Adds \"{}: {ident}\" (git commit --signoff).", trailers::SIGN_OFF),
        None => "Adds a Signed-off-by line with your name and email (git commit --signoff).".to_owned(),
    };
    let default = if prefs.sign_off { "on" } else { "off" };
    let hover = format!("{signed_as} Preferences > Commit turns it {default} for every commit.");
    if ui.checkbox(&mut sign_off, "Sign off").on_hover_text(hover).changed() {
        state.sign_off = Some(sign_off);
    }
    ui.checkbox(&mut state.no_verify, "Skip hooks")
        .on_hover_text("Commit without running the pre-commit and commit-msg hooks (git commit --no-verify).");

    ui.separator();
    ui.label(RichText::new("Co-authors").color(theme::text()));
    if !state.co_authors.is_empty() {
        let mut removed = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            for (i, person) in state.co_authors.iter().enumerate() {
                if chip(ui, icons::USER, &person.name, &person.ident(), theme::accent()) {
                    removed = Some(i);
                }
            }
        });
        if let Some(i) = removed {
            state.co_authors.remove(i);
        }
        ui.add_space(2.0);
    }
    co_author_field(ui, tab, state, ident.as_deref());
}

/// One row of the suggestions under the co-author field.
enum Suggestion {
    /// Someone in the history, with their number of commits.
    Known(Person, usize),
    /// What is typed, read as `Name <email>`.
    Typed(Person),
}

impl Suggestion {
    fn person(&self) -> &Person {
        match self {
            Suggestion::Known(person, _) | Suggestion::Typed(person) => person,
        }
    }
}

/// The people matching `query` (in their name or email, ignoring case):
/// picked lately first, then by number of commits. Leaves out those already
/// added and the committer (`me`).
fn suggestions(state: &State, me: Option<&Person>) -> Vec<Suggestion> {
    let query = state.query.trim().to_lowercase();
    let taken = |person: &Person| state.co_authors.iter().any(|p| p.same(person)) || me.is_some_and(|me| me.same(person));
    let matches = |person: &Person| {
        query.is_empty() || person.name.to_lowercase().contains(&query) || person.email.to_lowercase().contains(&query)
    };
    let mut list: Vec<Suggestion> = Vec::new();
    if let Some(typed) = Person::parse(&state.query)
        && !taken(&typed)
    {
        list.push(Suggestion::Typed(typed));
    }
    let people = state.people.as_deref().map(Vec::as_slice).unwrap_or_default();
    let count = |person: &Person| people.iter().find(|(p, _)| p.same(person)).map_or(0, |(_, n)| *n);
    let recent = state.recent.iter().map(|person| (person.clone(), count(person)));
    for (person, commits) in recent.chain(people.iter().cloned()) {
        if list.len() >= SUGGESTIONS {
            break;
        }
        if !taken(&person) && matches(&person) && !list.iter().any(|s| s.person().same(&person)) {
            list.push(Suggestion::Known(person, commits));
        }
    }
    list
}

fn add(state: &mut State, person: Person) {
    if !state.co_authors.iter().any(|p| p.same(&person)) {
        state.co_authors.push(person.clone());
    }
    state.recent.retain(|p| !p.same(&person));
    state.recent.insert(0, person);
    state.recent.truncate(RECENT);
    state.query.clear();
    state.selected = 0;
}

fn co_author_field(ui: &mut Ui, tab: &RepoTab, state: &mut State, ident: Option<&str>) {
    let me = ident.and_then(Person::parse);
    let list = suggestions(state, me.as_ref());
    let field_id = Id::new(("co-author-field", &tab.path));
    state.selected = state.selected.min(list.len().saturating_sub(1));

    // The field keeps the arrows and Enter for the list while it has the
    // keyboard (asked of egui's memory: the window itself may not have it).
    let mut picked: Option<Person> = None;
    if ui.ctx().memory(|m| m.has_focus(field_id)) {
        let (step, enter) = ui.input_mut(|i| {
            let down = i.count_and_consume_key(Modifiers::NONE, Key::ArrowDown) as isize;
            let up = i.count_and_consume_key(Modifiers::NONE, Key::ArrowUp) as isize;
            (down - up, i.consume_key(Modifiers::NONE, Key::Enter))
        });
        if step != 0 && !list.is_empty() {
            state.selected = (state.selected as isize + step).rem_euclid(list.len() as isize) as usize;
        }
        if enter && let Some(row) = list.get(state.selected) {
            picked = Some(row.person().clone());
        }
    }
    let before = state.query.clone();
    let field = ui.add(
        TextEdit::singleline(&mut state.query)
            .id(field_id)
            .hint_text(format!("{}  Name or email, or Name <email>", icons::USER_PLUS))
            .desired_width(f32::INFINITY),
    );
    if std::mem::take(&mut state.focus) {
        field.request_focus();
    }
    if state.query != before {
        state.selected = 0;
    }
    ui.add_space(2.0);

    if list.is_empty() {
        let text = if state.people.is_none() {
            "Reading the authors in the history…"
        } else if state.query.trim().is_empty() {
            "Type Name <email> to add someone."
        } else {
            "Nobody in the history matches. Type Name <email> to add someone else."
        };
        ui.add(egui::Label::new(RichText::new(text).size(11.5).color(theme::text_dim())).wrap());
    }
    // At most SUGGESTIONS rows: no scrolling needed.
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (i, row) in list.iter().enumerate() {
            if suggestion_row(ui, row, i == state.selected) {
                picked = Some(row.person().clone());
            }
        }
    });
    if let Some(person) = picked {
        add(state, person);
        // Ready for the next one.
        field.request_focus();
    }
}

/// A person under the co-author field: initials, name, email. True when
/// clicked.
fn suggestion_row(ui: &mut Ui, row: &Suggestion, selected: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response.clicked();
    }
    let painter = ui.painter();
    if selected || response.hovered() {
        painter.rect_filled(rect, 3.0, if selected { theme::bg_selected() } else { theme::bg_hover() });
    }
    let person = row.person();
    let center = pos2(rect.left() + 16.0, rect.center().y);
    widgets::avatar(painter, center, 11.0, &person.name, &person.email, None);
    let x = rect.left() + 34.0;
    let width = rect.right() - x - 8.0;
    let (name, detail) = match row {
        Suggestion::Typed(person) => (format!("Add {}", person.name), person.email.clone()),
        Suggestion::Known(person, commits) => {
            let commits = match commits {
                0 => String::new(),
                1 => " · 1 commit".to_owned(),
                n => format!(" · {n} commits"),
            };
            (person.name.clone(), format!("{}{commits}", person.email))
        }
    };
    let name = widgets::elided(painter, &name, theme::font(12.5), theme::text_bright(), width);
    painter.galley(pos2(x, rect.top() + 3.0), name, theme::text_bright());
    let detail = widgets::elided(painter, &detail, theme::font(11.0), theme::text_dim(), width);
    painter.galley(pos2(x, rect.top() + 18.0), detail, theme::text_dim());
    response.on_hover_cursor(CursorIcon::PointingHand).clicked()
}

// ---- the chips ---------------------------------------------------------------

/// The options that are on, as chips under the message fields; nothing at
/// all while every option is at its plain default. Also collects what the
/// background jobs answered.
pub fn chips(ui: &mut Ui, tab: &mut RepoTab, prefs: &RepoCommit) {
    let ctx = ui.ctx().clone();
    let mut state = load(&ctx, &tab.path);
    let sign_off = state.sign_off.unwrap_or(prefs.sign_off);
    let had = (state.ident.clone(), state.people.is_some());
    poll(tab, &mut state, sign_off, false);
    let mut changed = had != (state.ident.clone(), state.people.is_some());

    if sign_off || state.no_verify || !state.co_authors.is_empty() {
        let ident = state.ident.as_ref().and_then(|(_, ident)| ident.clone());
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            let signed = match &ident {
                Some(ident) => format!("{}: {ident}", trailers::SIGN_OFF),
                None => format!("Adds a {} line (git commit --signoff)", trailers::SIGN_OFF),
            };
            if sign_off && chip(ui, icons::SIGNATURE, "Signed off", &signed, theme::green()) {
                state.sign_off = Some(false);
                changed = true;
            }
            if state.no_verify && chip(ui, icons::SKIP_FORWARD, "Hooks skipped", "git commit --no-verify", theme::orange()) {
                state.no_verify = false;
                changed = true;
            }
            let mut removed = None;
            for (i, person) in state.co_authors.iter().enumerate() {
                let tip = format!("{}: {}", trailers::CO_AUTHOR, person.ident());
                if chip(ui, icons::USER, &person.name, &tip, theme::accent()) {
                    removed = Some(i);
                }
            }
            if let Some(i) = removed {
                state.co_authors.remove(i);
                changed = true;
            }
        });
    }
    if changed {
        save(&ctx, &tab.path, state);
    }
}

/// A small pill with an icon, a text and a × that removes it. True when the
/// × was clicked.
fn chip(ui: &mut Ui, icon: &str, text: &str, tooltip: &str, color: Color32) -> bool {
    let painter = ui.painter();
    let glyph = painter.layout_no_wrap(icon.to_owned(), theme::font(12.0), color);
    let label = widgets::elided(painter, text, theme::font(11.5), theme::text(), 170.0);
    let height = 20.0;
    let width = 7.0 + glyph.size().x + 5.0 + label.size().x + 4.0 + 16.0;
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let close = Rect::from_center_size(pos2(rect.right() - 10.0, rect.center().y), vec2(16.0, 16.0));
    let x = ui.interact(close, response.id.with("remove"), Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(
            rect,
            10.0,
            theme::mix(theme::bg_app(), color, 0.14),
            Stroke::new(1.0, theme::mix(theme::bg_app(), color, 0.5)),
            StrokeKind::Inside,
        );
        let y = |h: f32| (rect.center().y - h / 2.0).round();
        let glyph_w = glyph.size().x;
        painter.galley(pos2(rect.left() + 7.0, y(glyph.size().y)), glyph, color);
        painter.galley(pos2(rect.left() + 7.0 + glyph_w + 5.0, y(label.size().y)), label, theme::text());
        if x.hovered() {
            painter.rect_filled(close.shrink(1.0), 8.0, theme::mix(theme::bg_app(), color, 0.35));
        }
        let x_color = if x.hovered() { theme::text_bright() } else { theme::text_dim() };
        painter.text(close.center(), Align2::CENTER_CENTER, icons::X, theme::font(10.5), x_color);
    }
    response.on_hover_text(tooltip);
    x.on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Remove").clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(name: &str, email: &str) -> Person {
        Person::new(name, email)
    }

    #[test]
    fn suggestions_filter_as_you_type_and_leave_out_who_is_taken() {
        let mut state = State {
            people: Some(Arc::new(vec![
                (person("Test User", "test@example.com"), 50),
                (person("Alice Ångström", "alice@example.com"), 12),
                (person("Bob", "bob@example.com"), 3),
            ])),
            ..State::default()
        };
        let me = person("Test User", "TEST@example.com");
        let names = |state: &State| -> Vec<String> {
            suggestions(state, Some(&me)).iter().map(|s| s.person().name.clone()).collect()
        };
        assert_eq!(names(&state), ["Alice Ångström", "Bob"], "never yourself");
        state.query = "ÅNG".to_owned();
        assert_eq!(names(&state), ["Alice Ångström"]);
        state.query = "bob@".to_owned();
        assert_eq!(names(&state), ["Bob"]);

        // Typed in full: offered first, then whoever else matches.
        state.query = "Carol <carol@example.com>".to_owned();
        assert!(matches!(suggestions(&state, Some(&me)).first(), Some(Suggestion::Typed(p)) if p.email == "carol@example.com"));

        // Added ones are left out; picked lately come first.
        state.query.clear();
        add(&mut state, person("Bob", "bob@example.com"));
        assert_eq!(state.co_authors.len(), 1);
        assert_eq!(names(&state), ["Alice Ångström"]);
        state.co_authors.clear();
        assert_eq!(names(&state), ["Bob", "Alice Ångström"]);
        add(&mut state, person("Bob", "BOB@example.com"));
        add(&mut state, person("Bob", "bob@example.com"));
        assert_eq!(state.co_authors.len(), 1, "one chip per person");
    }
}
