//! "Create pull request": pick the two branches, write a title and a
//! description, then GitKrill pushes the branch if the remote does not have
//! it yet and asks the hosting service to open the pull request.
//!
//! * [`open`] is the one way in: the branch context menus, a branch dropped
//!   on the sidebar's pull request list, its "+" button and the command
//!   palette all call it.
//! * The dialog ([`NewPullForm`], shown through `ui::dialogs`) keeps the
//!   whole flow: editing, waiting for the push (an ordinary `git push` run
//!   with `tab.run`, so it shows its progress and errors like any other),
//!   then waiting for the service's answer on the hosting thread.
//! * What the commits between the branches are is read on the reader thread
//!   ([`compare`]); the prefill and the push decision are pure functions of
//!   that and the snapshot, tested below.

use std::path::PathBuf;
use std::time::Instant;

use eframe::egui::{self, Align, Button, ComboBox, Key, Layout, RichText, TextEdit, Ui, vec2};

use crate::git::ops;
use crate::git::repo::lossy;
use crate::git::{BranchInfo, RemoteBranch, RepoSnapshot};
use crate::hosting::{self, NewPull, PullRequest, RepoId};
use crate::state::{BusyOp, Dialog, RepoTab, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::graph::drag::Branch;
use crate::ui::hosting::open_link;
use crate::ui::widgets::spinner;

/// Reader-thread job that lists the commits between the two branches.
const COMPARE: &str = "new-pull:compare";
/// The busy-state label while the service is being asked.
const CREATING: &str = "Create pull request";
/// Commits counted at most; the description lists fewer.
const COUNT_LIMIT: usize = 1000;
const LIST_LIMIT: usize = 50;

// ---- opening the dialog -------------------------------------------------------

/// Opens the "Create pull request" dialog with `branch` as the branch the
/// changes come from: the checked-out branch when `None`. Says why in a
/// toast instead when no hosting service is connected for this repository.
pub fn open(tab: &mut RepoTab, shared: &mut Shared, branch: Option<Branch>) {
    let Some(snap) = tab.snap.clone() else { return };
    let Some(shown) = tab.hosting.connected() else {
        shared.toast(
            ToastKind::Error,
            "No pull or merge requests here",
            "This repository has no remote on a hosting service GitKrill knows, or the integration is switched off in the View menu.",
        );
        return;
    };
    let head = branch
        .filter(|branch| usable_head(&snap, &shown.remote, branch))
        .or_else(|| snap.head.branch.as_deref().map(Branch::local))
        .or_else(|| snap.branches.first().map(|branch| Branch::local(&branch.name)));
    let Some(head) = head else {
        shared.toast(ToastKind::Error, format!("No branch to open a {} from", shown.repo.service.pull_word()), "");
        return;
    };

    let on_remote = remote_names(&snap, &shown.remote);
    let local: Vec<String> = snap.branches.iter().map(|branch| branch.name.clone()).collect();
    let base = default_base(None, &on_remote, &local).unwrap_or_default();
    let mut form = NewPullForm {
        repo_path: tab.path.clone(),
        remote: shown.remote.clone(),
        repo: shown.repo.clone(),
        head,
        base,
        base_chosen: false,
        title: String::new(),
        body: String::new(),
        title_edited: false,
        body_edited: false,
        draft: false,
        compared: None,
        step: Step::Editing,
        problem: None,
    };
    form.title = readable(&form.head_name());
    form.compare(tab, true);
    shared.show(Dialog::NewPull(Box::new(form)));
}

/// Whether a pull request can come from `branch`: a local branch, or a
/// branch on the hosted remote itself.
fn usable_head(snap: &RepoSnapshot, remote: &str, branch: &Branch) -> bool {
    match branch {
        Branch::Local(name) => snap.branches.iter().any(|known| &known.name == name),
        Branch::Remote { remote: owner, name } => owner == remote && name != "HEAD",
    }
}

/// Whether dropping `branch` on the pull request list can open the dialog.
pub fn accepts(snap: &RepoSnapshot, remote: &str, branch: &Branch) -> bool {
    usable_head(snap, remote, branch)
}

fn remote_names(snap: &RepoSnapshot, remote: &str) -> Vec<String> {
    let mut names: Vec<String> = snap
        .remote_branches
        .iter()
        .filter(|branch| branch.remote == remote && branch.name != "HEAD")
        .map(|branch| branch.name.clone())
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names
}

// ---- pure decisions -----------------------------------------------------------

/// The branch to merge into when nothing else says: the remote's default
/// branch (what `refs/remotes/<remote>/HEAD` points at), else `main`, else
/// `master`, on the remote first and then locally, else the remote's first.
pub fn default_base(remote_head: Option<&str>, on_remote: &[String], local: &[String]) -> Option<String> {
    if let Some(head) = remote_head.filter(|head| !head.is_empty() && *head != "HEAD") {
        return Some(head.to_owned());
    }
    let has = |names: &[String], name: &str| names.iter().any(|known| known == name);
    for names in [on_remote, local] {
        for name in ["main", "master"] {
            if has(names, name) {
                return Some(name.to_owned());
            }
        }
    }
    on_remote.first().cloned()
}

/// A branch name as a title: its last part, words instead of dashes and
/// underscores, first letter capitalised. `feature/add-login_form` reads
/// "Add login form". A `fix/` (or `bugfix/`, `hotfix/`) prefix is kept as
/// the word "Fix", since the rest rarely says so itself.
pub fn readable(branch: &str) -> String {
    let last = branch.rsplit('/').find(|part| !part.is_empty()).unwrap_or(branch);
    let words: Vec<&str> = last.split(['-', '_', '.']).filter(|word| !word.is_empty()).collect();
    let mut text = if words.is_empty() { last.to_owned() } else { words.join(" ") };
    let kind = branch.split_once('/').map(|(kind, _)| kind.to_ascii_lowercase());
    let fix = matches!(kind.as_deref(), Some("fix" | "bugfix" | "hotfix"));
    if fix && !text.to_lowercase().starts_with("fix") {
        text = format!("fix {text}");
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The commits a pull request would bring in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Commits {
    pub count: usize,
    /// There are more than [`COUNT_LIMIT`]; `count` stops there.
    pub more: bool,
    /// Summaries, oldest first, at most [`LIST_LIMIT`].
    pub summaries: Vec<String>,
    /// The message body of the newest commit (after its summary line).
    pub newest_body: String,
}

/// Title and description to start from. One commit: its summary and body,
/// as GitHub does. Several: the branch name made readable, and the list of
/// what they do.
pub fn prefill(branch: &str, commits: &Commits) -> (String, String) {
    match commits.count {
        1 => {
            let title = commits.summaries.first().cloned().unwrap_or_else(|| readable(branch));
            (title, commits.newest_body.trim().to_owned())
        }
        0 => (readable(branch), String::new()),
        count => {
            let mut lines: Vec<String> = commits.summaries.iter().map(|summary| format!("- {summary}")).collect();
            let listed = commits.summaries.len();
            if count > listed || commits.more {
                let rest = count - listed;
                lines.push(format!("- …and {rest}{} more", if commits.more { "+" } else { "" }));
            }
            (readable(branch), lines.join("\n"))
        }
    }
}

/// How the head branch gets onto the remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushPlan {
    /// The branch's name on the remote: what the service is asked to merge.
    pub head: String,
    /// It has to be pushed first: it is not there yet, or it is ahead.
    pub push: bool,
    /// Commits that would be pushed, when it tracks a branch there.
    pub ahead: usize,
}

pub fn push_plan(branches: &[BranchInfo], remote_branches: &[RemoteBranch], remote: &str, head: &Branch) -> PushPlan {
    let name = match head {
        Branch::Remote { name, .. } => return PushPlan { head: name.clone(), push: false, ahead: 0 },
        Branch::Local(name) => name,
    };
    let Some(branch) = branches.iter().find(|branch| &branch.name == name) else {
        return PushPlan { head: name.clone(), push: true, ahead: 0 };
    };
    let there = |remote_name: &str| {
        remote_branches.iter().find(|known| known.remote == remote && known.name == remote_name)
    };
    // Tracking a branch on this remote that has every commit already.
    let tracked = branch
        .upstream
        .as_deref()
        .and_then(|upstream| upstream.strip_prefix(remote)?.strip_prefix('/'))
        .filter(|upstream| there(upstream).is_some());
    if let Some(upstream) = tracked {
        if branch.ahead == 0 {
            return PushPlan { head: upstream.to_owned(), push: false, ahead: 0 };
        }
        if upstream == name {
            return PushPlan { head: name.clone(), push: true, ahead: branch.ahead };
        }
    }
    // Not tracked, but the same commit is there under the same name.
    if let (Some(known), Some(tip)) = (there(name), branch.oid)
        && known.oid == tip
    {
        return PushPlan { head: name.clone(), push: false, ahead: 0 };
    }
    PushPlan { head: name.clone(), push: true, ahead: 0 }
}

// ---- reading the commits between the branches -------------------------------

/// What the reader thread found for one pair of branches.
pub struct Compared {
    head: Branch,
    base: String,
    /// The base was not given and was worked out here.
    defaulted: bool,
    commits: Result<Commits, String>,
}

fn head_ref(head: &Branch) -> String {
    match head {
        Branch::Local(name) => format!("refs/heads/{name}"),
        Branch::Remote { remote, name } => format!("refs/remotes/{remote}/{name}"),
    }
}

/// The commits `head` has that `base` lacks. `base` is a branch on
/// `remote`, or a local branch when the remote has none of that name.
/// Without a base, the remote's default branch is looked up first.
fn compare(
    repo: &git2::Repository,
    remote: &str,
    head: Branch,
    base: Option<String>,
    on_remote: Vec<String>,
    local: Vec<String>,
) -> Compared {
    let defaulted = base.is_none();
    let base = base.unwrap_or_else(|| {
        let symbolic = repo
            .find_reference(&format!("refs/remotes/{remote}/HEAD"))
            .ok()
            .and_then(|reference| reference.symbolic_target_bytes().map(lossy))
            .and_then(|target| target.strip_prefix(&format!("refs/remotes/{remote}/")).map(str::to_owned));
        default_base(symbolic.as_deref(), &on_remote, &local).unwrap_or_default()
    });
    let commits = walk(repo, remote, &head, &base);
    Compared { head, base, defaulted, commits }
}

fn walk(repo: &git2::Repository, remote: &str, head: &Branch, base: &str) -> Result<Commits, String> {
    let tip = |name: &str| repo.refname_to_id(name).ok();
    let head_name = match head {
        Branch::Local(name) | Branch::Remote { name, .. } => name.clone(),
    };
    let head_tip = tip(&head_ref(head)).ok_or_else(|| format!("{head_name} is not a branch here"))?;
    let base_tip = tip(&format!("refs/remotes/{remote}/{base}"))
        .or_else(|| tip(&format!("refs/heads/{base}")))
        .ok_or_else(|| format!("{base} is not a branch here"))?;

    let mut revwalk = repo.revwalk().map_err(|e| e.message().to_owned())?;
    revwalk.push(head_tip).map_err(|e| e.message().to_owned())?;
    revwalk.hide(base_tip).map_err(|e| e.message().to_owned())?;
    let mut commits = Commits::default();
    // Newest first, as the walk gives them.
    let mut summaries = Vec::new();
    for oid in revwalk {
        let oid = oid.map_err(|e| e.message().to_owned())?;
        if commits.count == COUNT_LIMIT {
            commits.more = true;
            break;
        }
        commits.count += 1;
        if summaries.len() < LIST_LIMIT
            && let Ok(commit) = repo.find_commit(oid)
        {
            if commits.count == 1 {
                commits.newest_body = commit.body_bytes().map(lossy).unwrap_or_default();
            }
            summaries.push(lossy(commit.summary_bytes().unwrap_or_default()));
        }
    }
    summaries.reverse();
    commits.summaries = summaries;
    Ok(commits)
}

// ---- the dialog ---------------------------------------------------------------

pub struct NewPullForm {
    /// The repository the dialog belongs to.
    repo_path: PathBuf,
    remote: String,
    repo: RepoId,
    head: Branch,
    base: String,
    /// The user picked the base, so the remote's default no longer replaces it.
    base_chosen: bool,
    title: String,
    body: String,
    /// The user typed in these: changing branches leaves them alone.
    title_edited: bool,
    body_edited: bool,
    draft: bool,
    /// What the reader found for the current pair, once it has.
    compared: Option<Compared>,
    step: Step,
    /// Why the last attempt failed, until the next one.
    problem: Option<String>,
}

enum Step {
    Editing,
    /// The head branch is being pushed by the git command labelled `label`;
    /// `then` is what comes once it is there.
    Pushing { label: String, since: Instant, then: Then },
    /// The service has the request; its answer comes back under `id`.
    Sending { id: u64 },
}

enum Then {
    Create(NewPull),
    /// Open the service's own form at this address.
    Browse(String),
}

/// What the dialog wants from `ui::dialogs` after a frame.
pub enum Next {
    Keep,
    Close,
    /// Working: neither Escape nor a click outside may close it.
    Hold,
}

impl NewPullForm {
    fn head_name(&self) -> String {
        match &self.head {
            Branch::Local(name) | Branch::Remote { name, .. } => name.clone(),
        }
    }

    fn busy(&self) -> bool {
        !matches!(self.step, Step::Editing)
    }

    /// Asks the reader for the commits of the current pair; `default_base`
    /// lets it pick the base as well.
    fn compare(&mut self, tab: &mut RepoTab, default_base: bool) {
        let Some(snap) = tab.snap.clone() else { return };
        self.compared = None;
        let (remote, head) = (self.remote.clone(), self.head.clone());
        let base = (!default_base).then(|| self.base.clone());
        let on_remote = remote_names(&snap, &remote);
        let local: Vec<String> = snap.branches.iter().map(|branch| branch.name.clone()).collect();
        tab.spawn_job(COMPARE, move |loader| compare(loader.repo(), &remote, head, base, on_remote, local));
    }

    /// Takes in the reader's answer when it is about the current pair.
    fn collect(&mut self, tab: &mut RepoTab) {
        let Some(found) = tab.take_result::<Compared>(COMPARE) else { return };
        if found.head != self.head {
            return;
        }
        if found.defaulted && !self.base_chosen {
            self.base = found.base.clone();
        } else if found.base != self.base {
            return;
        }
        if let Ok(commits) = &found.commits {
            let (title, body) = prefill(&self.head_name(), commits);
            if !self.title_edited {
                self.title = title;
            }
            if !self.body_edited {
                self.body = body;
            }
        }
        self.compared = Some(found);
    }

    fn commits(&self) -> Option<&Commits> {
        self.compared.as_ref().and_then(|found| found.commits.as_ref().ok())
    }

    /// Why there is no pull request to open for this pair, if there is not:
    /// neither Create nor the service's own form can help.
    fn blocker(&self, snap: &RepoSnapshot, open: &[PullRequest]) -> Option<String> {
        let plan = push_plan(&snap.branches, &snap.remote_branches, &self.remote, &self.head);
        if self.base.is_empty() {
            return Some("Pick the branch to merge into.".to_owned());
        }
        if plan.head == self.base {
            return Some(format!("{} cannot be merged into itself: pick another branch.", self.base));
        }
        if let Some(existing) = open
            .iter()
            .find(|pull| pull.head_ref == plan.head && pull.base_ref == self.base && pull.is_from(&self.repo))
        {
            return Some(format!(
                "{} {} already merges {} into {}.",
                self.repo.service.pull_word_title(),
                self.repo.service.pull_number(existing.number),
                plan.head,
                self.base
            ));
        }
        match self.compared.as_ref().map(|found| &found.commits) {
            Some(Err(error)) => return Some(format!("{error}.")),
            Some(Ok(commits)) if commits.count == 0 => {
                return Some(format!(
                    "Nothing to merge: {} already has every commit of {}.",
                    self.base,
                    self.head_name()
                ));
            }
            _ => {}
        }
        None
    }

    /// The press of Create, or of the browser button (`browse` is then the
    /// address): push first when the remote needs it. True when the dialog
    /// has done its job already.
    fn submit(&mut self, tab: &mut RepoTab, snap: &RepoSnapshot, browse: Option<String>) -> bool {
        let plan = push_plan(&snap.branches, &snap.remote_branches, &self.remote, &self.head);
        let then = match browse {
            Some(url) => Then::Browse(url),
            None => Then::Create(NewPull {
                head: plan.head.clone(),
                base: self.base.clone(),
                title: self.title.trim().to_owned(),
                body: self.body.trim_end().to_owned(),
                draft: self.draft,
            }),
        };
        self.problem = None;
        if plan.push {
            let cmd = ops::push_branch(&self.remote, &self.head_name());
            let label = cmd.label.clone();
            tab.run(cmd);
            self.step = Step::Pushing { label, since: Instant::now(), then };
            false
        } else {
            self.then(tab, then)
        }
    }

    /// What comes once the branch is on the remote. True when the dialog
    /// has done its job.
    fn then(&mut self, tab: &mut RepoTab, then: Then) -> bool {
        match then {
            Then::Create(new) => {
                self.send(tab, new);
                false
            }
            Then::Browse(url) => {
                self.step = Step::Editing;
                open_link(&url);
                true
            }
        }
    }

    fn send(&mut self, tab: &mut RepoTab, new: NewPull) {
        match tab.hosting.create_pull(new) {
            Some(id) => {
                tab.busy.push(BusyOp { label: CREATING.to_owned(), quiet: false, since: Instant::now() });
                self.step = Step::Sending { id };
            }
            None => {
                self.step = Step::Editing;
                self.problem = Some(format!("The {} connection is off.", self.repo.service.name()));
            }
        }
    }

    /// Moves the flow along: the push finished, or the answer arrived.
    fn advance(&mut self, tab: &mut RepoTab, shared: &mut Shared) -> bool {
        match &self.step {
            Step::Editing => {}
            Step::Pushing { label, since, .. } => match tab.take_outcome(label, *since) {
                Some(true) => {
                    let Step::Pushing { then, .. } = std::mem::replace(&mut self.step, Step::Editing) else {
                        return false;
                    };
                    return self.then(tab, then);
                }
                Some(false) => {
                    // git's own message is already in a toast.
                    self.step = Step::Editing;
                    self.problem = Some(format!(
                        "Pushing {} to {} failed, so nothing was opened.",
                        self.head_name(),
                        self.remote
                    ));
                }
                None => {}
            },
            Step::Sending { id } => {
                let id = *id;
                tab.hosting.poll();
                let answer = tab.hosting.take_created(id).or_else(|| {
                    (!tab.hosting.is_creating(id)).then(|| {
                        Err(hosting::Problem::Failed("The connection to the service stopped".to_owned()))
                    })
                });
                let Some(answer) = answer else { return false };
                forget_busy(tab);
                self.step = Step::Editing;
                match answer {
                    Ok(pull) => {
                        let url = self.repo.pull_url(pull.number);
                        let (site, service) = (self.repo.site(), self.repo.service);
                        shared.toast_link(
                            ToastKind::Success,
                            format!("Opened {} {}", service.pull_word(), service.pull_number(pull.number)),
                            pull.title.clone(),
                            format!("Open on {site}"),
                            url,
                        );
                        return true;
                    }
                    Err(problem) => self.problem = Some(hosting::create_problem(&problem, self.repo.service)),
                }
            }
        }
        false
    }
}

fn forget_busy(tab: &mut RepoTab) {
    if let Some(index) = tab.busy.iter().position(|op| op.label == CREATING) {
        tab.busy.remove(index);
    }
}

/// The dialog went away without finishing (replaced by another one): the
/// busy state it added must not stay behind. A pull request asked for still
/// shows up in the list when the service has opened it.
pub fn dropped(form: &NewPullForm, tab: Option<&mut RepoTab>) {
    if let (Step::Sending { .. }, Some(tab)) = (&form.step, tab) {
        forget_busy(tab);
    }
}

/// Draws the dialog for one frame.
pub fn show(ui: &mut Ui, form: &mut NewPullForm, shared: &mut Shared, tab: Option<&mut RepoTab>, fresh: bool) -> Next {
    let Some(tab) = tab.filter(|tab| tab.path == form.repo_path) else { return Next::Close };
    let Some(snap) = tab.snap.clone() else { return Next::Keep };
    form.collect(tab);
    if form.advance(tab, shared) {
        return Next::Close;
    }
    if form.busy() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    let shown = tab.hosting.connected();
    let open_pulls: Vec<PullRequest> = shown.as_ref().map(|shown| shown.view.pulls.items.clone()).unwrap_or_default();
    let auth = shown.as_ref().map(|shown| shown.view.auth).unwrap_or_default();
    let service = form.repo.site();
    let word = form.repo.service.pull_word();

    ui.set_width(520.0);
    ui.label(RichText::new(format!("Create {word}")).font(theme::bold(16.0)).color(theme::text_bright()));
    ui.label(RichText::new(format!("{} on {service}", form.repo.slug())).color(theme::text_dim()));
    ui.add_space(4.0);

    let (head_before, base_before) = (form.head.clone(), form.base.clone());
    ui.add_enabled_ui(!form.busy(), |ui| {
        branch_pickers(ui, form, &snap);
    });
    if form.head != head_before || form.base != base_before {
        if form.base != base_before {
            form.base_chosen = true;
        }
        form.compare(tab, false);
    }

    // What will happen: how many commits, and whether a push comes first.
    let plan = push_plan(&snap.branches, &snap.remote_branches, &form.remote, &form.head);
    let mut summary = match (form.commits(), &form.compared) {
        (Some(commits), _) => match (commits.count, commits.more) {
            (_, true) => format!("More than {COUNT_LIMIT} commits"),
            // The warning below says it.
            (0, _) => String::new(),
            (1, _) => "1 commit".to_owned(),
            (n, _) => format!("{n} commits"),
        },
        (None, None) => "Counting commits…".to_owned(),
        (None, Some(_)) => String::new(),
    };
    if !summary.is_empty() && form.commits().is_some_and(|commits| commits.count > 0) {
        summary.push_str(&format!(" from {} into {}", form.head_name(), form.base));
    }
    if plan.head == form.base {
        // The warning below says what is wrong; nothing would happen.
        summary.clear();
    } else if plan.push {
        let push = match plan.ahead {
            0 => format!("{} will be pushed to {} first", form.head_name(), form.remote),
            1 => format!("1 new commit will be pushed to {} first", form.remote),
            n => format!("{n} new commits will be pushed to {} first", form.remote),
        };
        summary = if summary.is_empty() { push } else { format!("{summary}  ·  {push}") };
    }
    ui.label(RichText::new(summary).size(12.0).color(theme::text_dim()));
    ui.add_space(2.0);

    let mut entered = false;
    ui.add_enabled_ui(!form.busy(), |ui| {
        ui.label(RichText::new("Title").size(12.0).color(theme::text_dim()));
        let title = ui.add(TextEdit::singleline(&mut form.title).hint_text("What it does").desired_width(f32::INFINITY));
        if fresh {
            title.request_focus();
        }
        form.title_edited |= title.changed();
        // Enter in the title creates, as in the other dialogs.
        entered = title.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        ui.label(RichText::new("Description").size(12.0).color(theme::text_dim()));
        let body = ui.add(
            TextEdit::multiline(&mut form.body)
                .hint_text("Why, and how to try it (optional)")
                .desired_rows(6)
                .desired_width(f32::INFINITY),
        );
        form.body_edited |= body.changed();
        ui.checkbox(&mut form.draft, "Draft (not ready for review yet)");
    });

    // Whatever stands in the way, or went wrong, or is happening now.
    let blocker = form.blocker(&snap, &open_pulls);
    let untitled = form.title.trim().is_empty();
    // Known to have no usable sign-in: Create would only be refused.
    let signed_out = matches!(auth, hosting::Auth::Anonymous | hosting::Auth::Rejected);
    let existing = open_pulls.iter().find(|pull| {
        pull.head_ref == plan.head && pull.base_ref == form.base && pull.is_from(&form.repo)
    });
    ui.add_space(2.0);
    let mut check_again = false;
    match &form.step {
        Step::Pushing { .. } => working(ui, &format!("Pushing {} to {}…", form.head_name(), form.remote)),
        Step::Sending { .. } => working(ui, &format!("Opening the {word} on {service}…")),
        Step::Editing => {
            if let Some(problem) = &form.problem {
                ui.label(RichText::new(format!("{}  {problem}", icons::WARNING_CIRCLE)).color(theme::red()));
            } else if let Some(blocker) = &blocker {
                ui.label(RichText::new(format!("{}  {blocker}", icons::WARNING)).color(theme::orange()));
            } else if untitled {
                ui.label(RichText::new(format!("{}  Add a title.", icons::WARNING)).color(theme::orange()));
            }
            if signed_out && form.problem.is_none() {
                let text = match shown.as_ref().is_some_and(|shown| shown.disconnected) {
                    true => format!(
                        "GitKrill does not use your {} sign-in: the account is disconnected in Preferences > \
                         Integrations. Connect it there to open a {} from here.",
                        form.repo.host,
                        form.repo.service.pull_word()
                    ),
                    false => hosting::create_problem(&hosting::Problem::NotSignedIn, form.repo.service),
                };
                ui.label(RichText::new(text).size(12.0).color(theme::text_dim()));
                let busy = shown.as_ref().is_some_and(|shown| shown.view.busy);
                let button = Button::new(format!("{}  Check again", icons::ARROWS_CLOCKWISE)).small();
                check_again = ui.add_enabled(!busy, button).clicked();
            }
        }
    }
    if check_again {
        // A manual refresh looks for a sign-in again.
        tab.hosting.refresh();
    }

    // Buttons: the browser on the left, Cancel and Create on the right.
    ui.add_space(6.0);
    let mut next = if form.busy() { Next::Hold } else { Next::Keep };
    let ready = blocker.is_none() && !untitled && !signed_out && !form.busy();
    ui.horizontal(|ui| {
        match existing {
            Some(pull) => {
                let url = form.repo.pull_url(pull.number);
                let label = format!("{}  Open #{}", icons::ARROW_SQUARE_OUT, pull.number);
                let button = Button::new(label).min_size(vec2(0.0, 28.0));
                if ui.add(button).on_hover_text(&url).clicked() {
                    open_link(&url);
                }
            }
            None => {
                // The service's own form needs the branch there too, so it
                // is pushed first, as for Create.
                let enabled = blocker.is_none() && !form.busy();
                let label = match plan.push {
                    true => format!("{}  Push and open on {service}", icons::ARROW_SQUARE_OUT),
                    false => format!("{}  Open on {service}", icons::ARROW_SQUARE_OUT),
                };
                let tip = format!("Opens the form on {service} in the browser: no sign-in needed here");
                let response = ui.add_enabled(enabled, Button::new(label).min_size(vec2(0.0, 28.0)));
                let response = if enabled { response.on_hover_text(tip) } else { response };
                if response.clicked() {
                    let compare = form.repo.compare_url(&form.base, &plan.head);
                    if form.submit(tab, &snap, Some(compare)) {
                        next = Next::Close;
                    } else {
                        next = Next::Hold;
                    }
                }
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let label = if form.draft { "Create draft".to_owned() } else { format!("Create {word}") };
            let text = if ready { theme::on_accent() } else { theme::text_dim() };
            let primary = Button::new(RichText::new(label).color(text))
                .fill(if ready { theme::solid(theme::accent(), 0.8) } else { theme::bg_raised() })
                .min_size(vec2(84.0, 28.0));
            let response = ui.add_enabled(ready, primary);
            let reason = match (&blocker, untitled, signed_out) {
                (Some(reason), _, _) => Some(reason.clone()),
                (None, true, _) => Some("Add a title.".to_owned()),
                (None, false, true) => Some(format!("Not signed in to {service}.")),
                (None, false, false) => None,
            };
            let response = match reason {
                Some(reason) if !form.busy() => response.on_disabled_hover_text(reason),
                _ => response,
            };
            let pressed = response.clicked()
                || (ready && (entered || ui.input(|i| i.key_pressed(Key::Enter) && i.modifiers.command)));
            if pressed {
                form.submit(tab, &snap, None);
                next = Next::Hold;
            }
            let cancel = ui.add_enabled(!form.busy(), Button::new("Cancel").min_size(vec2(72.0, 28.0)));
            if cancel.clicked() {
                next = Next::Close;
            }
        });
    });
    next
}

/// A spinner and what is going on.
fn working(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), egui::Sense::hover());
        spinner(ui.painter(), rect.center(), 6.0, theme::accent());
        ui.label(RichText::new(text).color(theme::text()));
    });
}

/// "From [branch] → Into [branch]".
fn branch_pickers(ui: &mut Ui, form: &mut NewPullForm, snap: &RepoSnapshot) {
    let remote = form.remote.clone();
    ui.horizontal(|ui| {
        ui.label(RichText::new("From").color(theme::text_dim()));
        let head_text = match &form.head {
            Branch::Local(name) => format!("{}  {name}", icons::LAPTOP),
            Branch::Remote { remote, name } => format!("{}  {remote}/{name}", icons::CLOUD),
        };
        ComboBox::from_id_salt("new-pull-head").selected_text(head_text).width(190.0).height(320.0).show_ui(
            ui,
            |ui| {
                for branch in &snap.branches {
                    let value = Branch::local(&branch.name);
                    let text = format!("{}  {}", icons::LAPTOP, branch.name);
                    ui.selectable_value(&mut form.head, value, text);
                }
                // Branches only the remote has.
                for branch in snap.remote_branches.iter().filter(|branch| {
                    branch.remote == remote
                        && branch.name != "HEAD"
                        && !snap.branches.iter().any(|local| local.name == branch.name)
                }) {
                    let text = format!("{}  {}", icons::CLOUD, branch.full);
                    ui.selectable_value(&mut form.head, Branch::remote(branch), text);
                }
            },
        );
        ui.label(RichText::new(icons::ARROW_RIGHT).color(theme::text_dim()));
        ui.label(RichText::new("Into").color(theme::text_dim()));
        let mut bases = remote_names(snap, &remote);
        if bases.is_empty() {
            bases = snap.branches.iter().map(|branch| branch.name.clone()).collect();
        }
        let base_text = if form.base.is_empty() { "Pick a branch".to_owned() } else { form.base.clone() };
        ComboBox::from_id_salt("new-pull-base").selected_text(base_text).width(190.0).height(320.0).show_ui(
            ui,
            |ui| {
                for name in bases {
                    let text = format!("{}  {name}", icons::GIT_BRANCH);
                    ui.selectable_value(&mut form.base, name, text);
                }
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Oid;
    use crate::tests::{Fixture, git};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_base_defaults_to_the_remotes_default_branch_then_main_then_master() {
        let remote = names(&["develop", "main", "master"]);
        // What the remote says its default is wins.
        assert_eq!(default_base(Some("develop"), &remote, &[]).as_deref(), Some("develop"));
        // Without that: main, then master; the remote's before local ones.
        assert_eq!(default_base(None, &remote, &[]).as_deref(), Some("main"));
        assert_eq!(default_base(None, &names(&["master", "x"]), &names(&["main"])).as_deref(), Some("master"));
        assert_eq!(default_base(None, &[], &names(&["topic", "main"])).as_deref(), Some("main"));
        assert_eq!(default_base(None, &[], &names(&["master"])).as_deref(), Some("master"));
        // Neither anywhere: the remote's first branch, or nothing.
        assert_eq!(default_base(None, &names(&["trunk", "x"]), &[]).as_deref(), Some("trunk"));
        assert_eq!(default_base(None, &[], &names(&["topic"])), None);
        // An empty or self-referring answer is not believed.
        assert_eq!(default_base(Some(""), &remote, &[]).as_deref(), Some("main"));
        assert_eq!(default_base(Some("HEAD"), &remote, &[]).as_deref(), Some("main"));
    }

    #[test]
    fn branch_names_are_made_readable_for_titles() {
        for (branch, title) in [
            ("feature/add-login_form", "Add login form"),
            ("fix-crash", "Fix crash"),
            ("hotfix/crash", "Fix crash"),
            ("fix/typo-in-readme", "Fix typo in readme"),
            ("bugfix/fix-the-build", "Fix the build"),
            ("main", "Main"),
            ("user/ada/dark.mode", "Dark mode"),
            ("trailing/", "Trailing"),
            ("--", "--"),
            ("élan", "Élan"),
            ("", ""),
        ] {
            assert_eq!(readable(branch), title, "{branch}");
        }
    }

    #[test]
    fn one_commit_gives_its_summary_and_body_several_give_a_list() {
        let one = Commits {
            count: 1,
            more: false,
            summaries: names(&["Add the login screen"]),
            newest_body: "\nSober form over the window.\n\n".to_owned(),
        };
        assert_eq!(
            prefill("feature/login", &one),
            ("Add the login screen".to_owned(), "Sober form over the window.".to_owned())
        );

        let three = Commits {
            count: 3,
            more: false,
            summaries: names(&["First", "Second", "Third"]),
            newest_body: "only the newest".to_owned(),
        };
        assert_eq!(
            prefill("feature/login-screen", &three),
            ("Login screen".to_owned(), "- First\n- Second\n- Third".to_owned())
        );

        // Longer than the list: the rest is counted.
        let long = Commits { count: 60, more: false, summaries: names(&["a", "b"]), newest_body: String::new() };
        assert_eq!(prefill("x", &long).1, "- a\n- b\n- …and 58 more");
        let huge = Commits { count: 1000, more: true, summaries: names(&["a"]), newest_body: String::new() };
        assert_eq!(prefill("x", &huge).1, "- a\n- …and 999+ more");

        // Nothing yet: the branch name alone.
        assert_eq!(prefill("docs/typo", &Commits::default()), ("Typo".to_owned(), String::new()));
    }

    fn oid(n: u8) -> Oid {
        Oid::from_bytes(&[n; 20]).unwrap()
    }

    fn local(name: &str, at: u8, upstream: Option<&str>, ahead: usize) -> BranchInfo {
        BranchInfo {
            name: name.to_owned(),
            oid: Some(oid(at)),
            is_head: false,
            upstream: upstream.map(str::to_owned),
            ahead,
            behind: 0,
        }
    }

    fn on(remote: &str, name: &str, at: u8) -> RemoteBranch {
        RemoteBranch {
            remote: remote.to_owned(),
            name: name.to_owned(),
            full: format!("{remote}/{name}"),
            oid: oid(at),
        }
    }

    #[test]
    fn the_branch_is_pushed_only_when_the_remote_lacks_something() {
        let plan = |branches: &[BranchInfo], remotes: &[RemoteBranch], head: Branch| {
            let plan = push_plan(branches, remotes, "origin", &head);
            (plan.head, plan.push, plan.ahead)
        };
        let s = |text: &str| text.to_owned();

        // Never pushed: push, under its own name.
        assert_eq!(plan(&[local("topic", 1, None, 0)], &[], Branch::local("topic")), (s("topic"), true, 0));
        // Tracking origin/topic and level with it: nothing to push.
        let tracking = [local("topic", 1, Some("origin/topic"), 0)];
        assert_eq!(plan(&tracking, &[on("origin", "topic", 1)], Branch::local("topic")), (s("topic"), false, 0));
        // Ahead of it: push the new commits.
        let ahead = [local("topic", 2, Some("origin/topic"), 2)];
        assert_eq!(plan(&ahead, &[on("origin", "topic", 1)], Branch::local("topic")), (s("topic"), true, 2));
        // Tracking a branch of another name that has it all: that name.
        let renamed = [local("topic", 1, Some("origin/ada/topic"), 0)];
        assert_eq!(
            plan(&renamed, &[on("origin", "ada/topic", 1)], Branch::local("topic")),
            (s("ada/topic"), false, 0)
        );
        // Not tracked, but the same commit is there by that name.
        assert_eq!(
            plan(&[local("topic", 1, None, 0)], &[on("origin", "topic", 1)], Branch::local("topic")),
            (s("topic"), false, 0)
        );
        // ...a different commit is: push (git refuses if it would lose any).
        assert_eq!(
            plan(&[local("topic", 2, None, 0)], &[on("origin", "topic", 1)], Branch::local("topic")),
            (s("topic"), true, 0)
        );
        // Tracking another remote: this one still lacks it.
        let elsewhere = [local("topic", 1, Some("fork/topic"), 0)];
        assert_eq!(plan(&elsewhere, &[on("fork", "topic", 1)], Branch::local("topic")), (s("topic"), true, 0));
        // An upstream that was deleted on the remote.
        let gone = [local("topic", 1, Some("origin/topic"), 0)];
        assert_eq!(plan(&gone, &[], Branch::local("topic")), (s("topic"), true, 0));
        // A remote branch is already there.
        let remote = Branch::Remote { remote: s("origin"), name: s("their/work") };
        assert_eq!(plan(&[], &[], remote), (s("their/work"), false, 0));
    }

    #[test]
    fn commits_between_the_branches_are_read_from_the_repository() {
        let fx = Fixture::new("new-pull-compare");
        fx.write("a.txt", "base\n");
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "feature/login"]);
        fx.write("b.txt", "one\n");
        fx.commit_all("Add the login form");
        fx.write("b.txt", "one\ntwo\n");
        git(&fx.dir, &["add", "-A"]);
        git(&fx.dir, &["commit", "-q", "-m", "Remember the last user\n\nSo nobody types it twice."]);
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("c.txt", "main moves on\n");
        fx.commit_all("Unrelated work on main");

        // A remote whose default branch is `trunk`, which holds main.
        git(&fx.dir, &["update-ref", "refs/remotes/origin/trunk", "main"]);
        git(&fx.dir, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk"]);
        let repo = git2::Repository::open(&fx.dir).unwrap();

        let found = compare(&repo, "origin", Branch::local("feature/login"), None, names(&["trunk"]), names(&["main"]));
        assert!(found.defaulted);
        assert_eq!(found.base, "trunk");
        let commits = found.commits.unwrap();
        assert_eq!(commits.count, 2);
        assert_eq!(commits.summaries, ["Add the login form", "Remember the last user"]);
        assert_eq!(commits.newest_body.trim(), "So nobody types it twice.");
        assert_eq!(prefill("feature/login", &commits).1, "- Add the login form\n- Remember the last user");

        // Without a remote HEAD, main; the other way round, nothing to merge.
        git(&fx.dir, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        let found = compare(&repo, "origin", Branch::local("main"), None, names(&["trunk"]), names(&["main"]));
        assert_eq!(found.base, "main");
        assert_eq!(found.commits.unwrap().count, 0);

        // A base the remote has wins over the local branch of that name.
        git(&fx.dir, &["update-ref", "refs/remotes/origin/main", "feature/login"]);
        let found = compare(&repo, "origin", Branch::local("feature/login"), Some("main".to_owned()), vec![], vec![]);
        assert_eq!(found.commits.unwrap().count, 0);
        assert!(!found.defaulted);

        // Branches that do not exist are said to be so.
        let found = compare(&repo, "origin", Branch::local("gone"), Some("main".to_owned()), vec![], vec![]);
        assert_eq!(found.commits.unwrap_err(), "gone is not a branch here");
        let found = compare(&repo, "origin", Branch::local("main"), Some("nowhere".to_owned()), vec![], vec![]);
        assert_eq!(found.commits.unwrap_err(), "nowhere is not a branch here");
    }
}
