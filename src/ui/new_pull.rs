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
//! * Forks: the pull request can go into the repository's parent when it is
//!   a fork ([`targets`]: the parent the service names, else the one a
//!   remote called `upstream` points at), and its branch can be on another
//!   remote of the same server, a fork of the repository. [`route`] works
//!   out which remote the branch is on or pushed to, and the dialog asks
//!   the service for a pull request across the two (`owner:branch`).

use std::path::PathBuf;
use std::time::Instant;

use eframe::egui::{self, Align, Button, ComboBox, Key, Layout, RichText, TextEdit, Ui, vec2};

use crate::git::ops;
use crate::git::repo::lossy;
use crate::git::{BranchInfo, RemoteBranch, RepoSnapshot};
use crate::hosting::{self, Across, NewPull, Parent, PullRequest, RepoId, Servers};
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
    let related = hosting::related(&snap.remotes, &shared.settings.hosting_servers, &shown.repo);
    let hosted = hosted_remotes(&shown.remote, &related);
    let head = branch
        .filter(|branch| usable_head(&snap, &hosted, branch))
        .or_else(|| snap.head.branch.as_deref().map(Branch::local))
        .or_else(|| snap.branches.first().map(|branch| Branch::local(&branch.name)));
    let Some(head) = head else {
        shared.toast(ToastKind::Error, format!("No branch to open a {} from", shown.repo.service.pull_word()), "");
        return;
    };

    let targets = targets(&shown.remote, &shown.repo, shown.view.parent.as_ref(), &related);
    let mut form = NewPullForm {
        repo_path: tab.path.clone(),
        remote: shown.remote.clone(),
        repo: shown.repo.clone(),
        related,
        targets,
        into: 0,
        into_chosen: false,
        head,
        base: String::new(),
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
    form.base = form.first_base(&snap);
    form.title = readable(&form.head_name());
    form.compare(tab, true);
    shared.show(Dialog::NewPull(Box::new(form)));
}

/// The remote shown first, then the others of `related`.
fn hosted_remotes(remote: &str, related: &[(String, RepoId)]) -> Vec<String> {
    std::iter::once(remote.to_owned()).chain(related.iter().map(|(name, _)| name.clone())).collect()
}

/// The remotes a pull request's branch can be on: the one the hosting
/// sidebar shows, and those pointing at other repositories of the same
/// service and server (forks of it, or its parent).
pub fn head_remotes(snap: &RepoSnapshot, servers: &Servers, remote: &str, repo: &RepoId) -> Vec<String> {
    hosted_remotes(remote, &hosting::related(&snap.remotes, servers, repo))
}

/// Whether a pull request can come from `branch`: a local branch, or a
/// branch on one of the `hosted` remotes.
fn usable_head(snap: &RepoSnapshot, hosted: &[String], branch: &Branch) -> bool {
    match branch {
        Branch::Local(name) => snap.branches.iter().any(|known| &known.name == name),
        Branch::Remote { remote: owner, name } => hosted.contains(owner) && name != "HEAD",
    }
}

/// Whether dropping `branch` on the pull request list can open the dialog.
pub fn accepts(snap: &RepoSnapshot, remote: &str, branch: &Branch) -> bool {
    usable_head(snap, &[remote.to_owned()], branch)
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

/// A repository a pull request can be opened in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub repo: RepoId,
    /// A remote pointing at it: its branches are those to merge into. `None`
    /// for a parent that no remote points at.
    pub remote: Option<String>,
    /// Its default branch, when the service said.
    pub default_branch: Option<String>,
    /// The repository shown is a fork of it.
    pub parent: bool,
}

/// Where a pull request can be opened, the default first: the parent the
/// service names when the repository shown is a fork, else the repository
/// a remote called `upstream` points at; then the repository shown itself.
/// `related` are the other remotes on the same server
/// ([`hosting::related`]).
pub fn targets(remote: &str, repo: &RepoId, parent: Option<&Parent>, related: &[(String, RepoId)]) -> Vec<Target> {
    let remote_for = |target: &RepoId| related.iter().find(|(_, other)| other.same(target)).map(|(name, _)| name.clone());
    let parent = match parent {
        Some(parent) => Some(Target {
            repo: parent.repo.clone(),
            remote: remote_for(&parent.repo),
            default_branch: Some(parent.default_branch.clone()).filter(|branch| !branch.is_empty()),
            parent: true,
        }),
        None => related.iter().find(|(name, _)| name == "upstream").map(|(name, upstream)| Target {
            repo: upstream.clone(),
            remote: Some(name.clone()),
            default_branch: None,
            parent: true,
        }),
    };
    let own = Target { repo: repo.clone(), remote: Some(remote.to_owned()), default_branch: None, parent: false };
    parent.into_iter().chain([own]).collect()
}

/// Where a pull request's branch is and where the pull request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// The remote the branch is on, or is pushed to first.
    pub push: String,
    /// The repository that remote points at.
    pub from: RepoId,
    pub into: RepoId,
    /// The remote whose branches are those to merge into: the target's, or
    /// the one shown when no remote points at the target.
    pub base_remote: String,
}

impl Route {
    /// The pull request is between two repositories rather than within
    /// the one shown: what the service is asked for then.
    pub fn across(&self, shown: &RepoId) -> Option<Across> {
        let within = self.from.same(shown) && self.into.same(shown);
        (!within).then(|| Across { from: self.from.clone(), into: self.into.clone() })
    }

    /// The branch `name` as the service shows it across repositories:
    /// `owner:branch` when it is in another than the target.
    pub fn head_label(&self, name: &str) -> String {
        if self.from.same(&self.into) { name.to_owned() } else { format!("{}:{name}", self.from.owner) }
    }
}

/// The route of a pull request from `head` into `target`. A branch of a
/// remote comes from that remote's repository. A local branch goes to the
/// remote it tracks a branch of the same name on, when that is the remote
/// shown or one of `related` (a fork the user pushes to); otherwise to the
/// remote shown, which is the fork when the target is its parent.
pub fn route(
    branches: &[BranchInfo],
    head: &Branch,
    (remote, repo): (&str, &RepoId),
    related: &[(String, RepoId)],
    target: &Target,
) -> Route {
    let hosted = hosted_remotes(remote, related);
    let push = match head {
        Branch::Remote { remote: owner, .. } => owner.clone(),
        Branch::Local(name) => branches
            .iter()
            .find(|branch| &branch.name == name)
            .and_then(|branch| branch.upstream.as_deref())
            .and_then(|upstream| hosted.iter().find(|hosted| upstream == format!("{hosted}/{name}")))
            .cloned()
            .unwrap_or_else(|| remote.to_owned()),
    };
    let from = related.iter().find(|(name, _)| *name == push).map_or_else(|| repo.clone(), |(_, from)| from.clone());
    Route {
        push,
        from,
        into: target.repo.clone(),
        base_remote: target.remote.clone().unwrap_or_else(|| remote.to_owned()),
    }
}

/// The open pull request that already does what `head` into `base` along
/// `route` would, among `open` (the list of the repository shown, so only
/// when that is where it goes).
pub fn existing<'a>(open: &'a [PullRequest], shown: &RepoId, route: &Route, head: &str, base: &str) -> Option<&'a PullRequest> {
    if !route.into.same(shown) {
        return None;
    }
    open.iter().find(|pull| {
        let from = match route.from.same(shown) {
            true => pull.is_from(shown),
            false => pull.head_repo.as_deref().is_some_and(|fork| fork.eq_ignore_ascii_case(&route.from.slug())),
        };
        pull.head_ref == head && pull.base_ref == base && from
    })
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
    /// The remote and hosted repository the sidebar shows.
    remote: String,
    repo: RepoId,
    /// The other remotes on the same server ([`hosting::related`]).
    related: Vec<(String, RepoId)>,
    /// Where the pull request can go ([`targets`]), and the one chosen.
    targets: Vec<Target>,
    into: usize,
    /// The user picked the repository, so a parent learnt later does not
    /// replace it.
    into_chosen: bool,
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
    /// The service has the request; its answer comes back under `id`, about
    /// a pull request in `into`.
    Sending { id: u64, into: RepoId },
}

enum Then {
    Create(NewPull, Option<Box<Across>>),
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

    fn target(&self) -> &Target {
        &self.targets[self.into.min(self.targets.len() - 1)]
    }

    fn route(&self, snap: &RepoSnapshot) -> Route {
        route(&snap.branches, &self.head, (&self.remote, &self.repo), &self.related, self.target())
    }

    /// The branches the pull request can merge into: those of the target's
    /// remote; its default branch alone when no remote points at it; the
    /// local branches when there is nothing else.
    fn bases(&self, snap: &RepoSnapshot) -> Vec<String> {
        let target = self.target();
        let names = match (&target.remote, &target.default_branch) {
            (Some(remote), _) => remote_names(snap, remote),
            (None, Some(default)) => vec![default.clone()],
            (None, None) => remote_names(snap, &self.remote),
        };
        if names.is_empty() { snap.branches.iter().map(|branch| branch.name.clone()).collect() } else { names }
    }

    /// The base to show before the reader has looked up the remote's own
    /// default.
    fn first_base(&self, snap: &RepoSnapshot) -> String {
        if let Some(default) = &self.target().default_branch {
            return default.clone();
        }
        let local: Vec<String> = snap.branches.iter().map(|branch| branch.name.clone()).collect();
        default_base(None, &self.bases(snap), &local).unwrap_or_default()
    }

    /// Takes in the targets as they are now: the service may have named the
    /// parent since the dialog opened. Until the user picks one, the default
    /// follows, and the base with it.
    fn retarget(&mut self, tab: &mut RepoTab, snap: &RepoSnapshot, parent: Option<&Parent>) {
        let targets = targets(&self.remote, &self.repo, parent, &self.related);
        if targets == self.targets {
            return;
        }
        let chosen = self.target().repo.clone();
        self.into = match self.into_chosen {
            true => targets.iter().position(|target| target.repo.same(&chosen)).unwrap_or(0),
            false => 0,
        };
        self.targets = targets;
        if !self.target().repo.same(&chosen) {
            self.base_chosen = false;
            self.base = self.first_base(snap);
            self.compare(tab, self.target().default_branch.is_none());
        }
    }

    /// Asks the reader for the commits of the current pair; `default_base`
    /// lets it pick the base as well.
    fn compare(&mut self, tab: &mut RepoTab, default_base: bool) {
        let Some(snap) = tab.snap.clone() else { return };
        self.compared = None;
        let (remote, head) = (self.route(&snap).base_remote, self.head.clone());
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
        let route = self.route(snap);
        let plan = push_plan(&snap.branches, &snap.remote_branches, &route.push, &self.head);
        if self.base.is_empty() {
            return Some("Pick the branch to merge into.".to_owned());
        }
        if plan.head == self.base && route.from.same(&route.into) {
            return Some(format!("{} cannot be merged into itself: pick another branch.", self.base));
        }
        if let Some(existing) = existing(open, &self.repo, &route, &plan.head, &self.base) {
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
        let route = self.route(snap);
        let plan = push_plan(&snap.branches, &snap.remote_branches, &route.push, &self.head);
        let then = match browse {
            Some(url) => Then::Browse(url),
            None => Then::Create(
                NewPull {
                    head: plan.head.clone(),
                    base: self.base.clone(),
                    title: self.title.trim().to_owned(),
                    body: self.body.trim_end().to_owned(),
                    draft: self.draft,
                },
                route.across(&self.repo).map(Box::new),
            ),
        };
        self.problem = None;
        if plan.push {
            let cmd = ops::push_branch(&route.push, &self.head_name());
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
            Then::Create(new, across) => {
                self.send(tab, new, across);
                false
            }
            Then::Browse(url) => {
                self.step = Step::Editing;
                open_link(&url);
                true
            }
        }
    }

    fn send(&mut self, tab: &mut RepoTab, new: NewPull, across: Option<Box<Across>>) {
        let into = across.as_ref().map_or_else(|| self.repo.clone(), |across| across.into.clone());
        let sent = match across {
            Some(across) => tab.hosting.create_pull_across(new, *across),
            None => tab.hosting.create_pull(new),
        };
        match sent {
            Some(id) => {
                tab.busy.push(BusyOp { label: CREATING.to_owned(), quiet: false, since: Instant::now() });
                self.step = Step::Sending { id, into };
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
                    let remote = tab.snap.as_ref().map_or_else(|| self.remote.clone(), |snap| self.route(snap).push);
                    self.problem =
                        Some(format!("Pushing {} to {remote} failed, so nothing was opened.", self.head_name()));
                }
                None => {}
            },
            Step::Sending { id, into } => {
                let (id, into) = (*id, into.clone());
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
                        let url = into.pull_url(pull.number);
                        let (site, service) = (into.site(), into.service);
                        let mut title = format!("Opened {} {}", service.pull_word(), service.pull_number(pull.number));
                        if !into.same(&self.repo) {
                            title.push_str(&format!(" in {}", into.slug()));
                        }
                        shared.toast_link(ToastKind::Success, title, pull.title.clone(), format!("Open on {site}"), url);
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
    if !form.busy() {
        let parent = shown.as_ref().and_then(|shown| shown.view.parent.clone());
        form.retarget(tab, &snap, parent.as_ref());
    }
    let service = form.repo.site();
    let word = form.repo.service.pull_word();

    ui.set_width(520.0);
    ui.label(RichText::new(format!("Create {word}")).font(theme::bold(16.0)).color(theme::text_bright()));
    ui.label(RichText::new(format!("{} on {service}", form.target().repo.slug())).color(theme::text_dim()));
    ui.add_space(4.0);

    let (head_before, base_before, into_before) = (form.head.clone(), form.base.clone(), form.into);
    ui.add_enabled_ui(!form.busy(), |ui| {
        target_picker(ui, form);
        branch_pickers(ui, form, &snap);
    });
    if form.into != into_before {
        // Another repository: its own branches, from its default one.
        form.into_chosen = true;
        form.base_chosen = false;
        form.base = form.first_base(&snap);
        let unknown = form.target().default_branch.is_none();
        form.compare(tab, unknown);
    } else if form.head != head_before || form.base != base_before {
        if form.base != base_before {
            form.base_chosen = true;
        }
        form.compare(tab, false);
    }

    // What will happen: how many commits, and whether a push comes first.
    let route = form.route(&snap);
    let plan = push_plan(&snap.branches, &snap.remote_branches, &route.push, &form.head);
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
        summary.push_str(&format!(" from {} into {}", route.head_label(&form.head_name()), form.base));
    }
    if plan.head == form.base && route.from.same(&route.into) {
        // The warning below says what is wrong; nothing would happen.
        summary.clear();
    } else if plan.push {
        let push = match plan.ahead {
            0 => format!("{} will be pushed to {} first", form.head_name(), route.push),
            1 => format!("1 new commit will be pushed to {} first", route.push),
            n => format!("{n} new commits will be pushed to {} first", route.push),
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
    let existing = existing(&open_pulls, &form.repo, &route, &plan.head, &form.base);
    let site = form.target().repo.site();
    ui.add_space(2.0);
    let mut check_again = false;
    match &form.step {
        Step::Pushing { .. } => working(ui, &format!("Pushing {} to {}…", form.head_name(), route.push)),
        Step::Sending { .. } => working(ui, &format!("Opening the {word} on {site}…")),
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
                let label = format!("{}  Open {}", icons::ARROW_SQUARE_OUT, form.repo.service.pull_number(pull.number));
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
                    true => format!("{}  Push and open on {site}", icons::ARROW_SQUARE_OUT),
                    false => format!("{}  Open on {site}", icons::ARROW_SQUARE_OUT),
                };
                let tip = format!("Opens the form on {site} in the browser: no sign-in needed here");
                let response = ui.add_enabled(enabled, Button::new(label).min_size(vec2(0.0, 28.0)));
                let response = if enabled { response.on_hover_text(tip) } else { response };
                if response.clicked() {
                    let compare = route.into.compare_url_from(&form.base, &route.from, &plan.head);
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

/// "Repository [owner/name]", when the pull request can go into the
/// repository's parent as well as into the repository itself.
fn target_picker(ui: &mut Ui, form: &mut NewPullForm) {
    if form.targets.len() < 2 {
        return;
    }
    let text = |target: &Target| match target.parent {
        true => format!("{}  {}  (parent)", icons::GIT_FORK, target.repo.slug()),
        false => format!("{}  {}", icons::GIT_BRANCH, target.repo.slug()),
    };
    let selected = text(form.target());
    ui.horizontal(|ui| {
        let word = if form.repo.service.is_gitlab() { "Project" } else { "Repository" };
        ui.label(RichText::new(word).color(theme::text_dim()))
            .on_hover_text(format!("Where the {} is opened", form.repo.service.pull_word()));
        ComboBox::from_id_salt("new-pull-into").selected_text(selected).width(300.0).show_ui(ui, |ui| {
            for (index, target) in form.targets.iter().enumerate() {
                ui.selectable_value(&mut form.into, index, text(target));
            }
        });
    });
}

/// "From [branch] → Into [branch]".
fn branch_pickers(ui: &mut Ui, form: &mut NewPullForm, snap: &RepoSnapshot) {
    let hosted = hosted_remotes(&form.remote, &form.related);
    let bases = form.bases(snap);
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
                // Branches only the remotes have: the one shown, then forks.
                let only_there = |branch: &&RemoteBranch| {
                    branch.name != "HEAD" && !snap.branches.iter().any(|local| local.name == branch.name)
                };
                let remotes = hosted.iter().flat_map(|remote| {
                    snap.remote_branches.iter().filter(move |branch| &branch.remote == remote)
                });
                for branch in remotes.filter(only_there) {
                    let text = format!("{}  {}", icons::CLOUD, branch.full);
                    ui.selectable_value(&mut form.head, Branch::remote(branch), text);
                }
            },
        );
        ui.label(RichText::new(icons::ARROW_RIGHT).color(theme::text_dim()));
        ui.label(RichText::new("Into").color(theme::text_dim()));
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

    // ---- forks ----

    fn hub(owner: &str) -> RepoId {
        RepoId {
            service: crate::hosting::Service::GitHub,
            host: "github.com".to_owned(),
            owner: owner.to_owned(),
            name: "app".to_owned(),
        }
    }

    #[test]
    fn a_fork_offers_its_parent_first_and_itself_second() {
        let fork = hub("ada");
        let parent = Parent { repo: hub("octo"), default_branch: "trunk".to_owned() };
        let related = vec![("upstream".to_owned(), hub("octo")), ("grace".to_owned(), hub("grace"))];

        // Named by the service, with a remote pointing at it.
        let found = targets("origin", &fork, Some(&parent), &related);
        assert_eq!(found.len(), 2);
        assert_eq!(
            (found[0].repo.slug(), found[0].remote.as_deref(), found[0].default_branch.as_deref(), found[0].parent),
            ("octo/app".to_owned(), Some("upstream"), Some("trunk"), true)
        );
        assert_eq!(
            (found[1].repo.slug(), found[1].remote.as_deref(), found[1].parent),
            ("ada/app".to_owned(), Some("origin"), false)
        );
        // Named by the service only: no remote, its default branch.
        let found = targets("origin", &fork, Some(&parent), &[]);
        assert_eq!((found[0].remote.as_deref(), found[0].default_branch.as_deref()), (None, Some("trunk")));
        let unnamed = Parent { default_branch: String::new(), ..parent };
        assert_eq!(targets("origin", &fork, Some(&unnamed), &[])[0].default_branch, None);
        // A remote called upstream alone.
        let found = targets("origin", &fork, None, &related);
        assert_eq!((found[0].repo.slug(), found[0].remote.as_deref()), ("octo/app".to_owned(), Some("upstream")));
        // Neither: the repository itself only. A fork of it is no target.
        let found = targets("origin", &fork, None, &[("grace".to_owned(), hub("grace"))]);
        assert_eq!(found.iter().map(|target| target.repo.slug()).collect::<Vec<_>>(), ["ada/app"]);
    }

    #[test]
    fn the_branch_goes_from_the_remote_it_lives_on() {
        let shown = hub("octo");
        let related = vec![("mine".to_owned(), hub("ada"))];
        let own = Target { repo: shown.clone(), remote: Some("origin".to_owned()), default_branch: None, parent: false };
        let branches = [
            local("feature", 1, Some("mine/feature"), 0),
            local("topic", 1, None, 0),
            local("fix", 1, Some("origin/main"), 0),
            local("work", 1, Some("mine/main"), 0),
        ];
        // Tracking a branch of its own name on a fork: from the fork.
        let found = route(&branches, &Branch::local("feature"), ("origin", &shown), &related, &own);
        assert_eq!((found.push.as_str(), found.from.slug(), found.base_remote.as_str()), ("mine", "ada/app".to_owned(), "origin"));
        assert_eq!(found.head_label("feature"), "ada:feature");
        assert_eq!(found.across(&shown), Some(Across { from: hub("ada"), into: shown.clone() }));
        // Not tracking, or tracking another branch it started from: the
        // remote shown, and nothing across.
        for name in ["topic", "fix", "work"] {
            let found = route(&branches, &Branch::local(name), ("origin", &shown), &related, &own);
            assert_eq!(found.push, "origin", "{name}");
            assert_eq!(found.across(&shown), None);
            assert_eq!(found.head_label(name), name);
        }
        // A branch of a fork's remote.
        let theirs = Branch::Remote { remote: "mine".to_owned(), name: "work".to_owned() };
        let found = route(&branches, &theirs, ("origin", &shown), &related, &own);
        assert_eq!((found.push.as_str(), found.from.slug()), ("mine", "ada/app".to_owned()));

        // The repository shown is the fork, the target its parent that no
        // remote points at: pushed to the fork, compared with its branches.
        let fork = hub("ada");
        let parent = Target { repo: hub("octo"), remote: None, default_branch: Some("trunk".to_owned()), parent: true };
        let found = route(&branches, &Branch::local("topic"), ("origin", &fork), &[], &parent);
        assert_eq!((found.push.as_str(), found.base_remote.as_str()), ("origin", "origin"));
        assert_eq!(found.across(&fork), Some(Across { from: fork.clone(), into: hub("octo") }));
        assert_eq!(found.head_label("topic"), "ada:topic");
        // A branch of the parent itself: within the parent, still across
        // from where the sidebar stands.
        let upstream = vec![("upstream".to_owned(), hub("octo"))];
        let parent = Target { remote: Some("upstream".to_owned()), ..parent };
        let theirs = Branch::Remote { remote: "upstream".to_owned(), name: "next".to_owned() };
        let found = route(&branches, &theirs, ("origin", &fork), &upstream, &parent);
        assert_eq!((found.from.slug(), found.base_remote.as_str()), ("octo/app".to_owned(), "upstream"));
        assert_eq!(found.head_label("next"), "next");
        assert_eq!(found.across(&fork), Some(Across { from: hub("octo"), into: hub("octo") }));
    }

    #[test]
    fn an_open_pull_request_counts_only_along_the_same_route() {
        let shown = hub("octo");
        let pull = |number, head_repo: Option<&str>| PullRequest {
            number,
            head_ref: "feature".to_owned(),
            base_ref: "main".to_owned(),
            head_repo: head_repo.map(str::to_owned),
            ..Default::default()
        };
        let open = [pull(1, Some("octo/app")), pull(2, Some("ada/app")), pull(3, None)];
        let within = Route { push: "origin".to_owned(), from: shown.clone(), into: shown.clone(), base_remote: "origin".to_owned() };
        let number = |found: Option<&PullRequest>| found.map(|pull| pull.number);
        assert_eq!(number(existing(&open, &shown, &within, "feature", "main")), Some(1));
        assert_eq!(number(existing(&open, &shown, &within, "feature", "develop")), None);
        assert_eq!(number(existing(&open, &shown, &within, "other", "main")), None);
        let from_fork = Route { from: hub("Ada"), ..within.clone() };
        assert_eq!(number(existing(&open, &shown, &from_fork, "feature", "main")), Some(2));
        // Into another repository: the list shown says nothing about it.
        let elsewhere = Route { into: hub("upstream"), ..within };
        assert_eq!(number(existing(&open, &shown, &elsewhere, "feature", "main")), None);
    }
}
