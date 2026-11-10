//! Gitflow outside preferences: the start and finish dialogs, the Gitflow
//! submenu of the toolbar's branch picker, the Gitflow group of branch
//! context menus, and command palette entries.
//!
//! The commands are built by `crate::git::gitflow`. The setup comes with
//! every snapshot (`snap.gitflow`), so nothing here reads anything until a
//! dialog asks for a finish plan.

use eframe::egui::{RichText, TextEdit, Ui};

use crate::git::gitflow::{self, Config, Finish, FinishOptions, Kind};
use crate::git::{BranchInfo, RepoSnapshot};
use crate::state::{AppAction, Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;
use crate::ui::palette::{Entry, Kind as EntryKind};
use crate::ui::prefs::Section;
use crate::ui::widgets::menu_item;

/// What the start dialog holds.
pub struct StartForm {
    pub kind: Kind,
    /// Feature name or version, without the prefix.
    pub name: String,
}

/// What the finish dialog holds.
pub struct FinishForm {
    pub branch: String,
    pub delete: bool,
    pub push: bool,
    /// What is left to do, once the reader thread has looked.
    plan: Option<Finish>,
}

/// Whether a dialog stays open.
pub enum Next {
    Keep,
    Close,
}

pub fn icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Feature => icons::SPARKLE,
        Kind::Release => icons::ROCKET_LAUNCH,
        Kind::Hotfix => icons::FIRST_AID_KIT,
    }
}

/// Asks for the name of a new feature, release or hotfix.
pub fn open_start(shared: &mut Shared, kind: Kind) {
    shared.show(Dialog::GitflowStart(StartForm { kind, name: String::new() }));
}

fn plan_key(branch: &str) -> String {
    format!("gitflow:plan:{branch}")
}

/// Opens the finish dialog for `branch`, and has the reader thread work
/// out what finishing it takes.
pub fn open_finish(tab: &mut RepoTab, shared: &mut Shared, branch: &str) {
    let Some(config) = tab.snap.as_ref().and_then(|snap| snap.gitflow.clone()) else { return };
    let name = branch.to_owned();
    tab.spawn_job(plan_key(branch), move |loader| gitflow::plan_finish(loader.repo(), &config, &name));
    shared.show(Dialog::GitflowFinish(Box::new(FinishForm {
        branch: branch.to_owned(),
        delete: true,
        push: false,
        plan: None,
    })));
}

fn open_preferences(shared: &mut Shared) {
    shared.actions.push(AppAction::OpenPreferences(Section::Gitflow));
}

/// Branches that can be finished: the checked-out one first, then the
/// others by name.
fn finishable<'a>(snap: &'a RepoSnapshot, config: &Config) -> Vec<(Kind, &'a str, &'a str)> {
    let mut list: Vec<(bool, Kind, &str, &str)> = snap
        .branches
        .iter()
        .filter_map(|b| config.kind_of(&b.name).map(|(kind, name)| (!b.is_head, kind, name, b.name.as_str())))
        .collect();
    list.sort_by(|a, b| (a.0, a.3).cmp(&(b.0, b.3)));
    list.into_iter().map(|(_, kind, name, branch)| (kind, name, branch)).collect()
}

/// Whether a Gitflow command is queued or running, so nothing starts twice.
fn busy(tab: &RepoTab) -> bool {
    ["Start feature", "Start release", "Start hotfix", "Finish feature", "Finish release", "Finish hotfix", gitflow::INIT_LABEL]
        .iter()
        .any(|label| tab.is_running(label))
}

/// The Gitflow submenu (in the toolbar's branch picker): start a branch,
/// finish one, or set Gitflow up.
pub fn menu(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot) {
    ui.set_min_width(220.0);
    let Some(config) = &snap.gitflow else {
        if menu_item(ui, icons::GEAR, "Initialize Gitflow…") {
            open_preferences(shared);
        }
        return;
    };
    let busy = busy(tab);
    for kind in Kind::ALL {
        let item = ui.add_enabled(!busy, eframe::egui::Button::new(format!("{}  Start {}…", icon(kind), kind.word())));
        if item.clicked() {
            open_start(shared, kind);
        }
    }
    let finishable = finishable(snap, config);
    if !finishable.is_empty() {
        ui.separator();
        // A long list belongs in the sidebar; the menu keeps the first few.
        for (kind, name, branch) in finishable.into_iter().take(8) {
            let item = ui.add_enabled(
                !busy,
                eframe::egui::Button::new(format!("{}  Finish {} {name}…", icons::FLAG_CHECKERED, kind.word())),
            );
            if item.on_hover_text(branch).clicked() {
                open_finish(tab, shared, branch);
            }
        }
    }
    ui.separator();
    if menu_item(ui, icons::GEAR, "Gitflow preferences") {
        open_preferences(shared);
    }
}

/// The Gitflow group of a local branch's context menu: finish a feature,
/// release or hotfix branch; start one from develop or the production
/// branch. Adds nothing for other branches or without Gitflow.
pub fn branch_menu(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, branch: &BranchInfo) {
    let Some(config) = &snap.gitflow else { return };
    let starts: &[Kind] = if branch.name == config.develop {
        &[Kind::Feature, Kind::Release]
    } else if branch.name == config.master {
        &[Kind::Hotfix]
    } else {
        &[]
    };
    let finish = config.kind_of(&branch.name);
    if starts.is_empty() && finish.is_none() {
        return;
    }
    let busy = busy(tab);
    ui.separator();
    ui.label(RichText::new("Gitflow").size(11.0).color(theme::text_dim()));
    if let Some((kind, name)) = finish
        && ui
            .add_enabled(!busy, eframe::egui::Button::new(format!("{}  Finish {} {name}…", icons::FLAG_CHECKERED, kind.word())))
            .clicked()
    {
        open_finish(tab, shared, &branch.name);
    }
    for kind in starts {
        if ui.add_enabled(!busy, eframe::egui::Button::new(format!("{}  Start {}…", icon(*kind), kind.word()))).clicked() {
            open_start(shared, *kind);
        }
    }
}

/// "Gitflow: start feature" and the like, for the command palette.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    let Some(config) = &snap.gitflow else {
        list.push(
            Entry::new(EntryKind::Action, "gitflow-init", icons::GEAR, "Gitflow: initialize", |env| {
                env.shared.actions.push(AppAction::OpenPreferences(Section::Gitflow))
            })
            .detail("in this repository's preferences")
            .keywords("git flow setup develop master"),
        );
        return;
    };
    if busy(tab) {
        return;
    }
    for kind in Kind::ALL {
        list.push(
            Entry::on_tab(format!("gitflow-start-{}", kind.word()), icon(kind), format!("Gitflow: start {}", kind.word()), move |_, shared| {
                open_start(shared, kind)
            })
            .detail(format!("from {}", config.base(kind)))
            .keywords("git flow new branch"),
        );
    }
    for (kind, name, branch) in finishable(snap, config) {
        let branch = branch.to_owned();
        let detail = match kind {
            Kind::Feature => format!("into {}", config.develop),
            _ => format!("into {} and {}, tag {}", config.master, config.develop, config.tag(name)),
        };
        list.push(
            Entry::on_tab(
                format!("gitflow-finish-{branch}"),
                icons::FLAG_CHECKERED,
                format!("Gitflow: finish {} {name}", kind.word()),
                move |tab, shared| open_finish(tab, shared, &branch),
            )
            .detail(detail)
            .keywords("git flow merge complete close"),
        );
    }
}

/// The start dialog: a name (or version), checked against the branches and
/// tags there are.
pub fn start_dialog(ui: &mut Ui, form: &mut StartForm, tab: Option<&mut RepoTab>, fresh: bool) -> Next {
    let kind = form.kind;
    dialogs::title(ui, &format!("Start {}", kind.word()));
    let Some((tab, snap, config)) = setup(tab) else {
        return not_set_up(ui);
    };
    let base = config.base(kind);
    ui.label(RichText::new(format!("Branches from {base}")).color(theme::text_dim()));
    let hint = if kind.is_versioned() { "Version, such as 1.2.0" } else { "Feature name" };
    let field = ui.add(TextEdit::singleline(&mut form.name).hint_text(hint).desired_width(f32::INFINITY));
    if fresh {
        field.request_focus();
    }
    dialogs::sanitize_ref(&mut form.name);
    let name = form.name.trim();
    let branch = config.branch(kind, name);
    let tag = config.tag(name);
    // git-flow keeps one release and one hotfix going at a time.
    let other = kind
        .is_versioned()
        .then(|| snap.branches.iter().find(|b| config.kind_of(&b.name).is_some_and(|(k, _)| k == kind)))
        .flatten();

    let problem = if !snap.branches.iter().any(|b| b.name == base) {
        Some(format!("There is no branch named {base}."))
    } else if let Some(other) = other {
        Some(format!("Finish {} first: Gitflow has one {} at a time.", other.name, kind.word()))
    } else if name.is_empty() {
        None
    } else if !dialogs::valid_ref(&branch) {
        Some(format!("{branch} is not a valid branch name."))
    } else if snap.branches.iter().any(|b| b.name == branch) {
        Some(format!("{branch} already exists."))
    } else if kind.is_versioned() && snap.tags.iter().any(|t| t.name == tag) {
        Some(format!("The tag {tag} already exists."))
    } else {
        None
    };
    match &problem {
        Some(problem) => {
            ui.colored_label(theme::orange(), problem);
        }
        None if !name.is_empty() => {
            ui.label(RichText::new(format!("Creates {branch} and checks it out.")).color(theme::text_dim()));
        }
        None => {}
    }
    let ready = problem.is_none() && !name.is_empty() && !busy(tab);
    match dialogs::buttons(ui, &format!("Start {}", kind.word()), ready, false) {
        Some(true) => {
            tab.run(gitflow::start(&config, kind, name));
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

/// The finish dialog: what is left to do (worked out in the background),
/// whether to delete the branch and push afterwards.
pub fn finish_dialog(ui: &mut Ui, form: &mut FinishForm, tab: Option<&mut RepoTab>) -> Next {
    let Some((tab, snap, config)) = setup(tab) else {
        dialogs::title(ui, "Finish");
        return not_set_up(ui);
    };
    if form.plan.is_none() {
        form.plan = tab.take_result::<Finish>(&plan_key(&form.branch));
    }
    let (kind, name) = config.kind_of(&form.branch).unwrap_or((Kind::Feature, &form.branch));
    let name = name.to_owned();
    dialogs::title(ui, &format!("Finish {} {name}", kind.word()));

    match &form.plan {
        None => {
            ui.horizontal(|ui| {
                ui.add(eframe::egui::Spinner::new().size(13.0).color(theme::accent()));
                ui.label(RichText::new(format!("Looking at {}…", form.branch)).color(theme::text_dim()));
            });
        }
        Some(plan) => match &plan.blocked {
            Some(reason) => {
                ui.colored_label(theme::orange(), reason);
            }
            None => {
                let done = plan.done_already(&config);
                if !done.is_empty() {
                    let text = format!("Carrying on from an earlier finish: {}.", done.join(", "));
                    ui.label(RichText::new(text).color(theme::text_dim()));
                }
                let steps = plan.steps(&config, form.delete);
                if steps.is_empty() {
                    ui.label(RichText::new("Nothing is left to do.").color(theme::text_dim()));
                }
                for step in steps {
                    ui.label(format!("•  {step}"));
                }
            }
        },
    }

    ui.add_space(4.0);
    ui.checkbox(&mut form.delete, format!("Delete {} afterwards", form.branch));
    let remote = snap.default_remote().map(str::to_owned);
    if let Some(remote) = &remote {
        let what = if kind.is_versioned() {
            format!("{}, {} and the tag", config.develop, config.master)
        } else {
            config.develop.clone()
        };
        ui.checkbox(&mut form.push, format!("Push {what} to {remote} afterwards"));
    }
    let note = if form.push && remote.is_some() {
        "Merges never fast-forward. A pushed finish cannot be undone from here."
    } else {
        "Merges never fast-forward. Undo puts every branch and tag back."
    };
    ui.label(RichText::new(note).size(12.0).color(theme::text_faint()));

    let ready = form.plan.as_ref().is_some_and(|plan| plan.blocked.is_none()) && !busy(tab);
    match dialogs::buttons(ui, &format!("Finish {}", kind.word()), ready, false) {
        Some(true) => {
            if let Some(plan) = &form.plan {
                let push = remote.filter(|_| form.push);
                let remote_branch = push
                    .as_ref()
                    .is_some_and(|remote| snap.remote_branches.iter().any(|b| &b.remote == remote && b.name == form.branch));
                let options = FinishOptions { delete: form.delete, push, remote_branch };
                tab.run(gitflow::finish(&config, plan, &options));
            }
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

type Setup<'a> = (&'a mut RepoTab, std::sync::Arc<RepoSnapshot>, Config);

fn setup(tab: Option<&mut RepoTab>) -> Option<Setup<'_>> {
    let tab = tab?;
    let snap = tab.snap.clone()?;
    let config = snap.gitflow.clone()?;
    Some((tab, snap, config))
}

fn not_set_up(ui: &mut Ui) -> Next {
    ui.label(RichText::new("Gitflow is not set up in this repository.").color(theme::text_dim()));
    match dialogs::buttons(ui, "Close", true, false) {
        Some(_) => Next::Close,
        None => Next::Keep,
    }
}
