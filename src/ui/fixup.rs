//! Fixup commits from the graph, and autosquash (`git::fixup`): a commit's
//! right-click menu records the staged changes as a fixup of it (or a
//! squash, with the message typed in the commit panel), and a fixup
//! commit's menu folds every fixup of the branch into its commit, or says
//! why that one cannot be folded. The command palette offers both.

use eframe::egui::Ui;

use crate::git::fixup::{self, AUTOSQUASH_LABEL, Autosquash, Survey};
use crate::git::{CommitInfo, Oid, RepoSnapshot, RepoState, short};
use crate::state::{RepoTab, Selection, Shared};
use crate::theme::icons;
use crate::ui::graph::drag::{Relation, relation};
use crate::ui::merge::entry;
use crate::ui::palette::Entry;

/// The autosquash the checked-out branch allows, going by the loaded
/// history.
pub fn plan(snap: &RepoSnapshot) -> Option<Autosquash> {
    survey(snap).plan
}

/// What autosquash makes of the checked-out branch's fixups, going by the
/// loaded history. The other branches bound it, except the branch's own
/// upstream and its namesakes on the remotes: fixups already pushed for
/// review are still folded (and then force-pushed), what another branch
/// shares never.
pub fn survey(snap: &RepoSnapshot) -> Survey {
    let Some(head) = snap.head.oid else { return Survey::default() };
    let own = snap.current_branch();
    let upstream = own.and_then(|b| b.upstream.as_deref());
    let locals = snap.branches.iter().filter(|b| !b.is_head).filter_map(|b| b.oid);
    let remotes = snap
        .remote_branches
        .iter()
        .filter(|r| Some(r.full.as_str()) != upstream && own.is_none_or(|b| b.name != r.name))
        .map(|r| r.oid);
    // A branch at HEAD itself shares everything and would stop the plan
    // before it starts; it keeps the old commits, as a backup would.
    let stops: Vec<Oid> = locals.chain(remotes).filter(|oid| *oid != head).collect();
    fixup::survey(&snap.history, head, &stops)
}

/// Why `commit` cannot get a fixup now, if it cannot; `None` also when the
/// menu should not offer it at all (`offered` false).
fn fixup_blocker(snap: &RepoSnapshot, commit: &CommitInfo) -> (bool, Option<&'static str>) {
    let Some(head) = snap.head.oid else { return (false, None) };
    let on_branch = matches!(relation(&snap.history, head, commit.oid), Relation::Same | Relation::Ahead);
    if !on_branch || commit.parents.len() > 1 {
        return (false, None);
    }
    let why = if snap.state != RepoState::Clean {
        Some("Finish or abort the operation in progress first.")
    } else if snap.status.staged.is_empty() {
        Some("Stage the changes to fold into this commit first.")
    } else {
        None
    };
    (true, why)
}

/// Why the autosquash cannot run now (changes to tracked files, an
/// operation in progress, one running).
fn autosquash_blocker(tab: &RepoTab, snap: &RepoSnapshot) -> Option<String> {
    if tab.is_running(AUTOSQUASH_LABEL) {
        return Some("Autosquashing…".to_owned());
    }
    crate::git::rebase::tree_blocker(snap)
}

fn run_autosquash(tab: &mut RepoTab) {
    let Some(snap) = tab.snap.clone() else { return };
    if let Some(plan) = plan(&snap) {
        tab.run(fixup::autosquash(&plan, &snap.head));
    }
}

/// The message typed in the commit panel, for a squash commit; `None` when
/// there is none (or only the template, untouched).
fn panel_message(ctx: &eframe::egui::Context, tab: &RepoTab) -> Option<String> {
    let message = crate::ui::commit_button::message(tab);
    (!message.is_empty() && !crate::ui::commit_button::untouched_template(ctx, tab)).then_some(message)
}

fn squash_into(tab: &mut RepoTab, shared: &Shared, target: Oid, message: &str) {
    let mut cmd = fixup::squash_into(target, message);
    if crate::prefs::repo(&shared.settings, &tab.path).commit.strip_comments {
        cmd = cmd.strip_comments();
    }
    tab.run(cmd);
}

/// Entries for a commit's right-click menu: a fixup or squash of it with
/// the staged changes (for commits on the checked-out branch), and, on a
/// fixup commit that has something to fold into, autosquash.
pub fn menu_items(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, commit: &CommitInfo) {
    let (offered, blocked) = fixup_blocker(snap, commit);
    if offered {
        let hover = "Commits the staged changes as \"fixup! …\"; autosquash then folds them into this commit \
                     (git commit --fixup).";
        if entry(ui, icons::WRENCH, "Fixup this commit with staged changes", blocked, hover) {
            tab.run(fixup::fixup(commit.oid));
        }
        let message = panel_message(ui.ctx(), tab);
        let squash_blocked = blocked.or(message.is_none().then_some("Type the message to add in the commit panel first."));
        let hover = "Commits the staged changes as \"squash! …\" with the message in the commit panel; \
                     autosquash then folds them into this commit and adds that message to its own \
                     (git commit --squash).";
        if entry(ui, icons::STACK_PLUS, "Squash into this commit with staged changes", squash_blocked, hover)
            && let Some(message) = message
        {
            squash_into(tab, shared, commit.oid, &message);
        }
    }
    if !fixup::is_fixup(&commit.summary) {
        return;
    }
    let survey = survey(snap);
    if let Some(plan) = survey.plan.filter(|plan| plan.fixups.contains(&commit.oid)) {
        let branch = snap.head_label();
        let blocked = autosquash_blocker(tab, snap);
        let how = if plan.merges {
            ", inside a merged branch where that is, and keeps the merges (git rebase -i --rebase-merges)"
        } else {
            " (git rebase -i --autosquash)"
        };
        let hover = format!("Folds every fixup commit of {branch} into the commit it names{how}; {}.", plan.cost());
        if entry(ui, icons::MAGIC_WAND, &format!("Autosquash {}", plan.what()), blocked.as_deref(), &hover) {
            run_autosquash(tab);
        }
    } else if let Some((_, why)) = survey.left.iter().find(|(oid, _)| *oid == commit.oid) {
        entry(ui, icons::MAGIC_WAND, "Autosquash", Some(why), "");
    }
}

/// "Autosquash fixup commits" while the branch has any to fold, and a fixup
/// of the selected commit while changes are staged.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    if let Some(plan) = plan(snap)
        && autosquash_blocker(tab, snap).is_none()
    {
        list.push(
            Entry::on_tab("autosquash", icons::MAGIC_WAND, "Autosquash fixup commits", |tab, _| run_autosquash(tab))
                .detail(format!("{} on {}", plan.what(), snap.head_label()))
                .keywords("fixup squash fold rebase autosquash"),
        );
    }
    if let Selection::Commit(oid) = tab.selection
        && let Some(commit) = snap.row_of(oid).map(|row| &snap.history.commits[row])
        && let (true, None) = fixup_blocker(snap, commit)
    {
        list.push(
            Entry::on_tab("fixup-selected", icons::WRENCH, format!("Fixup {} with staged changes", short(oid)), move |tab, _| {
                tab.run(fixup::fixup(oid))
            })
            .detail(commit.summary.clone())
            .keywords("fixup commit amend older staged"),
        );
    }
}
