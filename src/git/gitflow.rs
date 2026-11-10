//! Gitflow done with plain git commands: the branch model of git-flow (AVH
//! edition), set up the same way (`gitflow.*` in the repository's own
//! configuration) so the git-flow command line and other clients see the
//! same setup. No git-flow program is needed.
//!
//! - [`read`] reads the setup (on the reader thread, with every snapshot).
//! - [`init`] and [`start`] build the commands that set Gitflow up and start
//!   a feature, release or hotfix.
//! - [`plan_finish`] looks at the repository (reader thread) and works out
//!   what finishing a branch still takes; [`finish`] turns that into one
//!   command. Like git-flow, finishing picks up where it stopped: after a
//!   merge conflict is resolved, planning again skips the merges and the tag
//!   that are already done.

use super::ops::{GitCmd, RefMove, UndoKind};
use super::Oid;

/// The configuration keys, as git-flow AVH writes them.
pub mod keys {
    pub const MASTER: &str = "gitflow.branch.master";
    pub const DEVELOP: &str = "gitflow.branch.develop";
    pub const FEATURE: &str = "gitflow.prefix.feature";
    pub const BUGFIX: &str = "gitflow.prefix.bugfix";
    pub const RELEASE: &str = "gitflow.prefix.release";
    pub const HOTFIX: &str = "gitflow.prefix.hotfix";
    pub const SUPPORT: &str = "gitflow.prefix.support";
    pub const VERSION_TAG: &str = "gitflow.prefix.versiontag";
}

/// The label of [`init`]'s command, to follow how it ended.
pub const INIT_LABEL: &str = "Initialize Gitflow";

/// A repository's Gitflow setup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The production branch.
    pub master: String,
    pub develop: String,
    pub feature: String,
    pub bugfix: String,
    pub release: String,
    pub hotfix: String,
    pub support: String,
    /// Put before a version to name its tag (`v` makes `v1.2`); often empty.
    pub version_tag: String,
}

/// The three kinds of branch GitKrill starts and finishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Feature,
    Release,
    Hotfix,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Feature, Kind::Release, Kind::Hotfix];

    /// "feature", as in "Start feature".
    pub fn word(self) -> &'static str {
        match self {
            Kind::Feature => "feature",
            Kind::Release => "release",
            Kind::Hotfix => "hotfix",
        }
    }

    /// "Feature", for titles and tag messages.
    pub fn title(self) -> &'static str {
        match self {
            Kind::Feature => "Feature",
            Kind::Release => "Release",
            Kind::Hotfix => "Hotfix",
        }
    }

    /// Releases and hotfixes are named by a version, and tagged with it.
    pub fn is_versioned(self) -> bool {
        self != Kind::Feature
    }
}

/// The production branch to suggest: `main` when the repository has it and
/// no `master`, otherwise `master` (git-flow's own default).
pub fn default_master<'a>(branches: impl IntoIterator<Item = &'a str>) -> &'static str {
    let (mut main, mut master) = (false, false);
    for branch in branches {
        main |= branch == "main";
        master |= branch == "master";
    }
    if main && !master { "main" } else { "master" }
}

impl Config {
    /// git-flow's defaults, with `master` as the production branch.
    pub fn defaults(master: &str) -> Self {
        Config {
            master: master.to_owned(),
            develop: "develop".to_owned(),
            feature: "feature/".to_owned(),
            bugfix: "bugfix/".to_owned(),
            release: "release/".to_owned(),
            hotfix: "hotfix/".to_owned(),
            support: "support/".to_owned(),
            version_tag: String::new(),
        }
    }

    /// The setup found through `get` (a configuration lookup), or `None`
    /// when Gitflow is not set up: both branches must be named, and
    /// differently. Missing prefixes take git-flow's defaults.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let master = get(keys::MASTER).filter(|v| !v.trim().is_empty())?;
        let develop = get(keys::DEVELOP).filter(|v| !v.trim().is_empty())?;
        if master == develop {
            return None;
        }
        let defaults = Config::defaults(&master);
        let or = |key: &str, default: String| get(key).unwrap_or(default);
        Some(Config {
            feature: or(keys::FEATURE, defaults.feature),
            bugfix: or(keys::BUGFIX, defaults.bugfix),
            release: or(keys::RELEASE, defaults.release),
            hotfix: or(keys::HOTFIX, defaults.hotfix),
            support: or(keys::SUPPORT, defaults.support),
            version_tag: or(keys::VERSION_TAG, defaults.version_tag),
            master,
            develop,
        })
    }

    /// Every key and its value, as [`init`] writes them.
    pub fn entries(&self) -> [(&'static str, &str); 8] {
        [
            (keys::MASTER, &self.master),
            (keys::DEVELOP, &self.develop),
            (keys::FEATURE, &self.feature),
            (keys::BUGFIX, &self.bugfix),
            (keys::RELEASE, &self.release),
            (keys::HOTFIX, &self.hotfix),
            (keys::SUPPORT, &self.support),
            (keys::VERSION_TAG, &self.version_tag),
        ]
    }

    pub fn prefix(&self, kind: Kind) -> &str {
        match kind {
            Kind::Feature => &self.feature,
            Kind::Release => &self.release,
            Kind::Hotfix => &self.hotfix,
        }
    }

    /// Where a branch of this kind starts, and where finishing merges it
    /// first: develop for features, the production branch otherwise.
    pub fn base(&self, kind: Kind) -> &str {
        match kind {
            Kind::Feature | Kind::Release => &self.develop,
            Kind::Hotfix => &self.master,
        }
    }

    /// Where finishing merges the branch first.
    pub fn target(&self, kind: Kind) -> &str {
        match kind {
            Kind::Feature => &self.develop,
            Kind::Release | Kind::Hotfix => &self.master,
        }
    }

    /// `feature/login` for a feature named `login`.
    pub fn branch(&self, kind: Kind, name: &str) -> String {
        format!("{}{name}", self.prefix(kind))
    }

    /// The tag of a release or hotfix version.
    pub fn tag(&self, version: &str) -> String {
        format!("{}{version}", self.version_tag)
    }

    /// The kind of `branch` and its name after the prefix, when it is a
    /// feature, release or hotfix branch. The longest matching prefix wins.
    pub fn kind_of<'a>(&self, branch: &'a str) -> Option<(Kind, &'a str)> {
        Kind::ALL
            .into_iter()
            .filter(|kind| !self.prefix(*kind).is_empty())
            .filter_map(|kind| Some((kind, branch.strip_prefix(self.prefix(kind))?)))
            .filter(|(_, name)| !name.is_empty())
            .max_by_key(|(kind, _)| self.prefix(*kind).len())
    }
}

/// The repository's Gitflow setup, or `None`. Cheap: a few lookups in the
/// configuration libgit2 already has open.
pub fn read(repo: &git2::Repository) -> Option<Config> {
    let config = repo.config().ok()?.snapshot().ok()?;
    Config::from_lookup(|key| config.get_string(key).ok())
}

/// What [`init`] needs to know about the repository's branches.
#[derive(Clone, Debug, Default)]
pub struct Existing {
    /// The repository has no commit yet.
    pub unborn: bool,
    pub master_local: bool,
    /// The remote branch to start from (`origin/main`) when only a remote
    /// has the production branch.
    pub master_remote: Option<String>,
    pub develop_local: bool,
    pub develop_remote: Option<String>,
}

/// Saves `config` in the repository's configuration and creates the
/// branches that are missing, as `git flow init` does: the production
/// branch from the remote that has it (or as an empty first commit in a new
/// repository), develop from its remote twin or from the production branch.
pub fn init(config: &Config, existing: &Existing) -> Result<GitCmd, String> {
    let mut cmd = GitCmd::empty(INIT_LABEL);
    for (key, value) in config.entries() {
        cmd = cmd.step(&["config", "--local", key, value]);
    }
    let (master, develop) = (config.master.as_str(), config.develop.as_str());
    let mut created = Vec::new();
    if existing.unborn {
        cmd = cmd
            .step(&["symbolic-ref", "HEAD", &format!("refs/heads/{master}")])
            .step(&["commit", "--allow-empty", "-q", "-m", "Initial commit"]);
        created.push(master);
    } else if !existing.master_local {
        let Some(remote) = &existing.master_remote else {
            return Err(format!("There is no branch named {master}."));
        };
        cmd = cmd.step(&["branch", master, remote]);
        created.push(master);
    }
    if !existing.develop_local {
        cmd = match (&existing.develop_remote, existing.unborn) {
            (Some(remote), false) => cmd.step(&["branch", develop, remote]),
            _ => cmd.step(&["branch", "--no-track", develop, master]),
        };
        created.push(develop);
    }
    let done = match created.as_slice() {
        [] => "Gitflow initialized".to_owned(),
        [one] => format!("Gitflow initialized, created {one}"),
        [one, two] => format!("Gitflow initialized, created {one} and {two}"),
        _ => unreachable!(),
    };
    Ok(cmd.done(done))
}

/// The label of [`start`]'s command.
pub fn start_label(kind: Kind, name: &str) -> String {
    format!("Start {} {name}", kind.word())
}

/// Creates `<prefix><name>` from the kind's base branch and checks it out.
pub fn start(config: &Config, kind: Kind, name: &str) -> GitCmd {
    let branch = config.branch(kind, name);
    GitCmd::new(start_label(kind, name), &["checkout", "-b", &branch, config.base(kind)])
        .undoable(UndoKind::BranchCreate { name: branch.clone(), checkout: true })
        .done(format!("Started {branch}"))
}

/// What finishing one branch still takes, worked out by [`plan_finish`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finish {
    pub kind: Kind,
    pub branch: String,
    /// The name after the prefix: the version of a release or hotfix.
    pub name: String,
    /// Merge the branch into [`Config::target`]; false once it is in.
    pub merge: bool,
    /// Releases and hotfixes: the tag, and whether it is still to be made.
    pub tag: Option<String>,
    pub create_tag: bool,
    /// Releases and hotfixes: merge the tag into develop; false once
    /// develop has it.
    pub back_merge: bool,
    /// Why it cannot be finished now; nothing runs while this is set.
    pub blocked: Option<String>,
}

impl Finish {
    /// The steps still to do, in words, for the dialog.
    pub fn steps(&self, config: &Config, delete: bool) -> Vec<String> {
        let mut steps = Vec::new();
        if self.merge {
            steps.push(format!("Merge {} into {}", self.branch, config.target(self.kind)));
        }
        if let Some(tag) = self.tag.as_ref().filter(|_| self.create_tag) {
            steps.push(format!("Tag {} as {tag}", config.master));
        }
        if self.back_merge
            && let Some(tag) = &self.tag
        {
            steps.push(format!("Merge tag {tag} into {}", config.develop));
        }
        if delete {
            steps.push(format!("Delete {}", self.branch));
        }
        steps
    }

    /// What was done already by an earlier finish that stopped, in words.
    pub fn done_already(&self, config: &Config) -> Vec<String> {
        let mut done = Vec::new();
        if self.blocked.is_some() {
            return done;
        }
        if !self.merge {
            done.push(format!("{} is merged into {}", self.branch, config.target(self.kind)));
        }
        if let Some(tag) = self.tag.as_ref().filter(|_| !self.create_tag) {
            done.push(format!("{tag} is tagged"));
        }
        if self.kind.is_versioned() && !self.back_merge {
            done.push(format!("{} has the release", config.develop));
        }
        done
    }
}

/// Looks at the repository and works out what finishing `branch` takes.
/// Run it on the reader thread.
pub fn plan_finish(repo: &git2::Repository, config: &Config, branch: &str) -> Finish {
    let (kind, name) = config.kind_of(branch).unwrap_or((Kind::Feature, branch));
    let mut plan = Finish {
        kind,
        branch: branch.to_owned(),
        name: name.to_owned(),
        merge: false,
        tag: kind.is_versioned().then(|| config.tag(name)),
        create_tag: false,
        back_merge: false,
        blocked: None,
    };
    if let Err(reason) = fill_plan(repo, config, &mut plan) {
        plan.blocked = Some(reason);
    }
    plan
}

fn fill_plan(repo: &git2::Repository, config: &Config, plan: &mut Finish) -> Result<(), String> {
    if config.kind_of(&plan.branch).is_none() {
        return Err(format!("{} is not a feature, release or hotfix branch.", plan.branch));
    }
    let tip = |name: &str| repo.refname_to_id(&format!("refs/heads/{name}")).ok();
    let Some(branch) = tip(&plan.branch) else {
        return Err(format!("The branch {} no longer exists.", plan.branch));
    };
    let target_name = config.target(plan.kind);
    let target = tip(target_name).ok_or_else(|| format!("There is no branch named {target_name}."))?;
    let develop = tip(&config.develop).ok_or_else(|| format!("There is no branch named {}.", config.develop))?;
    // `a` is in `b` when `b` is `a` or descends from it.
    let contains = |b: Oid, a: Oid| a == b || repo.graph_descendant_of(b, a).unwrap_or(false);

    plan.merge = !contains(target, branch);
    if let Some(tag) = &plan.tag {
        let tagged = repo
            .refname_to_id(&format!("refs/tags/{tag}"))
            .ok()
            .and_then(|oid| repo.find_object(oid, None).ok()?.peel_to_commit().ok())
            .map(|commit| commit.id());
        match tagged {
            // A tag of that name from before this finish would be merged
            // into develop in place of the release.
            Some(_) if plan.merge => return Err(format!("The tag {tag} already exists.")),
            Some(tagged) => plan.back_merge = !contains(develop, tagged),
            None => {
                plan.create_tag = true;
                plan.back_merge = plan.merge || !contains(develop, target);
            }
        }
    }

    match repo.state() {
        git2::RepositoryState::Clean => {}
        git2::RepositoryState::Merge => {
            return Err("A merge is in progress: commit it with Continue (or abort it), then finish again.".to_owned());
        }
        _ => return Err("Another operation (a rebase, cherry-pick or revert) is in progress: finish or abort it first.".to_owned()),
    }
    let mut options = git2::StatusOptions::new();
    options.include_untracked(false).include_ignored(false).exclude_submodules(true);
    let changed = repo.statuses(Some(&mut options)).map(|s| !s.is_empty()).unwrap_or(false);
    if changed {
        return Err("Commit or stash your changes first.".to_owned());
    }
    Ok(())
}

/// How to finish, beside the plan.
#[derive(Clone, Debug, Default)]
pub struct FinishOptions {
    /// Delete the branch once it is merged.
    pub delete: bool,
    /// Push the merged branches (and the tag) to this remote afterwards.
    pub push: Option<String>,
    /// The branch also exists on that remote, and is deleted there too.
    pub remote_branch: bool,
}

pub fn finish_label(kind: Kind, name: &str) -> String {
    format!("Finish {} {name}", kind.word())
}

/// One command that does what `plan` says is left, ending on develop:
/// merges without fast-forward, an annotated tag ("Release 1.2"), the tag
/// merged back into develop, the branch deleted, and a push when asked.
/// Undoable while nothing was pushed.
pub fn finish(config: &Config, plan: &Finish, options: &FinishOptions) -> GitCmd {
    let (develop, master, branch) = (config.develop.as_str(), config.master.as_str(), plan.branch.as_str());
    let mut cmd = GitCmd::empty(finish_label(plan.kind, &plan.name));
    match plan.tag.as_deref() {
        Some(tag) => {
            if plan.merge {
                cmd = cmd.step(&["checkout", master]).step(&["merge", "--no-ff", "--no-edit", branch]);
            }
            if plan.create_tag {
                let message = format!("{} {}", plan.kind.title(), plan.name);
                cmd = cmd.step(&["tag", "-a", tag, "-m", &message, master]);
            }
            cmd = cmd.step(&["checkout", develop]);
            if plan.back_merge {
                cmd = cmd.step(&["merge", "--no-ff", "--no-edit", tag]);
            }
        }
        None => {
            cmd = cmd.step(&["checkout", develop]);
            if plan.merge {
                cmd = cmd.step(&["merge", "--no-ff", "--no-edit", branch]);
            }
        }
    }
    // By now the branch is in develop (and in the production branch): the
    // steps above merged it, or the plan found it there. `-d` would still
    // refuse a branch whose upstream lacks some of its commits.
    if options.delete {
        cmd = cmd.step(&["branch", "-D", branch]);
    }

    if let Some(remote) = &options.push {
        let mut args = vec!["push", remote.as_str(), develop];
        let tag_ref = plan.tag.as_ref().map(|tag| format!("refs/tags/{tag}"));
        if let Some(tag_ref) = &tag_ref {
            args.extend([master, tag_ref.as_str()]);
        }
        cmd = cmd.step(&args);
        if options.delete && options.remote_branch {
            cmd = cmd.step(&["push", remote, "--delete", branch]);
        }
    } else {
        // Pushed work cannot be taken back, so only an unpushed finish is
        // undoable: every ref it touches goes back where it was.
        let mut refs = vec![RefMove::new(format!("refs/heads/{branch}")), RefMove::new(format!("refs/heads/{develop}"))];
        if plan.kind.is_versioned() {
            refs.push(RefMove::new(format!("refs/heads/{master}")));
            if let Some(tag) = &plan.tag {
                refs.push(RefMove::new(format!("refs/tags/{tag}")));
            }
        }
        cmd = cmd.undoable(UndoKind::Refs(refs));
    }

    let what = format!("{} {}", plan.kind.word(), plan.name);
    cmd.done(format!("Finished {what}")).after_conflict(format!(
        "Once the merge is committed, finish {what} again: it carries on from there."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RepoState;
    use crate::git::ops::{self, UndoEntry};
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    fn oid(fx: &Fixture, rev: &str) -> String {
        git(&fx.dir, &["rev-parse", rev])
    }

    fn parents(fx: &Fixture, rev: &str) -> Vec<String> {
        git(&fx.dir, &["rev-list", "--parents", "-n", "1", rev]).split(' ').skip(1).map(str::to_owned).collect()
    }

    fn exists(fx: &Fixture, refname: &str) -> bool {
        std::process::Command::new("git")
            .current_dir(&fx.dir)
            .args(["show-ref", "--verify", "--quiet", refname])
            .status()
            .unwrap()
            .success()
    }

    /// A repository with one commit on `main`, Gitflow set up with the
    /// defaults (and `version_tag`).
    fn set_up(name: &str, version_tag: &str) -> (Fixture, Config) {
        let fx = Fixture::new(name);
        fx.write("a.txt", "base\n");
        fx.commit_all("first");
        let mut config = Config::defaults("main");
        config.version_tag = version_tag.to_owned();
        let existing = Existing { master_local: true, ..Existing::default() };
        let done = fx.run(init(&config, &existing).unwrap());
        assert_eq!(done.snap.gitflow.as_ref(), Some(&config));
        (fx, config)
    }

    fn plan(fx: &Fixture, config: &Config, branch: &str) -> Finish {
        let (config, branch) = (config.clone(), branch.to_owned());
        fx.job(move |loader| plan_finish(loader.repo(), &config, &branch))
    }

    fn undo(fx: &Fixture, entry: UndoEntry) -> UndoEntry {
        let done = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        done.result.expect("undo works");
        entry
    }

    fn deleting() -> FinishOptions {
        FinishOptions { delete: true, ..FinishOptions::default() }
    }

    #[test]
    fn names_kinds_and_defaults() {
        assert_eq!(default_master(["main", "develop"]), "main");
        assert_eq!(default_master(["main", "master"]), "master");
        assert_eq!(default_master([]), "master");
        let mut config = Config::defaults("main");
        assert_eq!(config.kind_of("feature/login"), Some((Kind::Feature, "login")));
        assert_eq!(config.kind_of("release/1.2"), Some((Kind::Release, "1.2")));
        assert_eq!(config.kind_of("hotfix/1.2.1"), Some((Kind::Hotfix, "1.2.1")));
        assert_eq!(config.kind_of("feature/"), None);
        assert_eq!(config.kind_of("develop"), None);
        // The longest prefix wins when one prefix starts another.
        config.release = "f/r/".to_owned();
        config.feature = "f/".to_owned();
        assert_eq!(config.kind_of("f/r/2.0"), Some((Kind::Release, "2.0")));
        config.version_tag = "v".to_owned();
        assert_eq!(config.tag("2.0"), "v2.0");
        // Not set up without both branches, or with one branch twice.
        let lookup = |pairs: &'static [(&str, &str)]| {
            move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_owned())
        };
        assert_eq!(Config::from_lookup(lookup(&[(keys::MASTER, "main")])), None);
        assert_eq!(Config::from_lookup(lookup(&[(keys::MASTER, "main"), (keys::DEVELOP, "main")])), None);
        let found = Config::from_lookup(lookup(&[(keys::MASTER, "main"), (keys::DEVELOP, "dev")])).unwrap();
        assert_eq!((found.develop.as_str(), found.feature.as_str()), ("dev", "feature/"));
    }

    #[test]
    fn init_writes_the_keys_and_creates_develop() {
        let (fx, _) = set_up("gitflow-init", "");
        let get = |key: &str| git(&fx.dir, &["config", "--local", "--get", key]);
        assert_eq!(get("gitflow.branch.master"), "main");
        assert_eq!(get("gitflow.branch.develop"), "develop");
        assert_eq!(get("gitflow.prefix.feature"), "feature/");
        assert_eq!(get("gitflow.prefix.release"), "release/");
        assert_eq!(get("gitflow.prefix.hotfix"), "hotfix/");
        assert_eq!(get("gitflow.prefix.support"), "support/");
        assert_eq!(get("gitflow.prefix.bugfix"), "bugfix/");
        // Present, and empty: git-flow takes a missing key for "not set up".
        assert_eq!(get("gitflow.prefix.versiontag"), "");
        assert_eq!(oid(&fx, "develop"), oid(&fx, "main"));
        assert_eq!(git(&fx.dir, &["branch", "--show-current"]), "main");

        // In a repository without commits, the production branch starts empty.
        let fresh = Fixture::new("gitflow-init-unborn");
        let config = Config::defaults("main");
        let done = fresh.run(init(&config, &Existing { unborn: true, ..Existing::default() }).unwrap());
        assert!(done.snap.gitflow.is_some());
        assert_eq!(oid(&fresh, "develop"), oid(&fresh, "main"));
        // Without the production branch anywhere, nothing runs.
        assert!(init(&config, &Existing::default()).is_err());
    }

    #[test]
    fn start_and_finish_a_feature_then_undo_and_redo() {
        let (fx, config) = set_up("gitflow-feature", "");
        let done = fx.run(start(&config, Kind::Feature, "login"));
        assert_eq!(done.snap.head.branch.as_deref(), Some("feature/login"));
        assert_eq!(oid(&fx, "feature/login"), oid(&fx, "develop"));
        fx.write("login.txt", "login\n");
        fx.commit_all("Add login");
        let (feature, develop) = (oid(&fx, "feature/login"), oid(&fx, "develop"));

        let plan = plan(&fx, &config, "feature/login");
        assert_eq!((plan.blocked.as_deref(), plan.merge, plan.tag.as_deref()), (None, true, None));
        let done = fx.run(finish(&config, &plan, &deleting()));
        assert_eq!(done.snap.head.branch.as_deref(), Some("develop"));
        assert!(!exists(&fx, "refs/heads/feature/login"));
        // Never a fast-forward: a merge commit of develop and the feature.
        assert_eq!(parents(&fx, "develop"), [develop.clone(), feature.clone()]);
        let finished = oid(&fx, "develop");

        let entry = undo(&fx, done.undo.expect("an unpushed finish is undoable"));
        assert_eq!(git(&fx.dir, &["branch", "--show-current"]), "feature/login");
        assert_eq!((oid(&fx, "feature/login"), oid(&fx, "develop")), (feature, develop));
        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
        redone.result.expect("redo works");
        assert_eq!(oid(&fx, "develop"), finished);
        assert!(!exists(&fx, "refs/heads/feature/login"));
        assert_eq!(git(&fx.dir, &["branch", "--show-current"]), "develop");
    }

    #[test]
    fn finish_a_release_tags_and_merges_into_both_branches() {
        let (fx, config) = set_up("gitflow-release", "");
        git(&fx.dir, &["checkout", "-q", "develop"]);
        fx.write("b.txt", "feature work\n");
        fx.commit_all("Work on develop");
        fx.run(start(&config, Kind::Release, "1.2"));
        fx.write("version.txt", "1.2\n");
        fx.commit_all("Bump version");
        let (release, main, develop) = (oid(&fx, "release/1.2"), oid(&fx, "main"), oid(&fx, "develop"));

        let plan = plan(&fx, &config, "release/1.2");
        assert_eq!(plan.blocked, None);
        assert!(plan.merge && plan.create_tag && plan.back_merge);
        assert_eq!(plan.steps(&config, true).len(), 4);
        let done = fx.run(finish(&config, &plan, &deleting()));

        assert_eq!(parents(&fx, "main"), [main.clone(), release]);
        assert_eq!(oid(&fx, "1.2^{commit}"), oid(&fx, "main"));
        assert_eq!(git(&fx.dir, &["cat-file", "-t", "1.2"]), "tag", "annotated");
        assert_eq!(git(&fx.dir, &["tag", "-l", "--format=%(contents:subject)", "1.2"]), "Release 1.2");
        assert_eq!(parents(&fx, "develop"), [develop.clone(), oid(&fx, "main")]);
        assert!(git(&fx.dir, &["log", "-1", "--format=%s", "develop"]).contains("Merge tag '1.2'"));
        assert!(!exists(&fx, "refs/heads/release/1.2"));
        assert_eq!(done.snap.head.branch.as_deref(), Some("develop"));

        // Undo takes all of it back: both branches, the tag, the release branch.
        undo(&fx, done.undo.expect("undoable"));
        assert_eq!((oid(&fx, "main"), oid(&fx, "develop")), (main, develop));
        assert!(!exists(&fx, "refs/tags/1.2"));
        assert!(exists(&fx, "refs/heads/release/1.2"));
        assert_eq!(git(&fx.dir, &["branch", "--show-current"]), "release/1.2");
    }

    #[test]
    fn finish_a_hotfix_with_a_version_tag_prefix() {
        let (fx, config) = set_up("gitflow-hotfix", "v");
        let done = fx.run(start(&config, Kind::Hotfix, "1.2.1"));
        assert_eq!(done.snap.head.branch.as_deref(), Some("hotfix/1.2.1"));
        assert_eq!(oid(&fx, "hotfix/1.2.1"), oid(&fx, "main"));
        fx.write("a.txt", "fixed\n");
        fx.commit_all("Fix");
        let hotfix = oid(&fx, "hotfix/1.2.1");

        let plan = plan(&fx, &config, "hotfix/1.2.1");
        assert_eq!(plan.tag.as_deref(), Some("v1.2.1"));
        fx.run(finish(&config, &plan, &deleting()));
        assert_eq!(parents(&fx, "main")[1], hotfix);
        assert_eq!(git(&fx.dir, &["tag", "-l", "--format=%(contents:subject)", "v1.2.1"]), "Hotfix 1.2.1");
        assert_eq!(oid(&fx, "v1.2.1^{commit}"), oid(&fx, "main"));
        assert_eq!(git(&fx.dir, &["merge-base", "--is-ancestor", "main", "develop"]), "");
        assert_eq!(fx.read("a.txt"), "fixed\n");
        assert!(!exists(&fx, "refs/heads/hotfix/1.2.1"));
    }

    #[test]
    fn a_conflicting_finish_stops_and_carries_on_after_the_merge() {
        let (fx, config) = set_up("gitflow-conflict", "");
        git(&fx.dir, &["checkout", "-q", "develop"]);
        fx.write("a.txt", "develop\n");
        fx.commit_all("Change on develop");
        fx.run(start(&config, Kind::Hotfix, "1.0.1"));
        fx.write("a.txt", "hotfix\n");
        fx.commit_all("Fix on main");
        let (hotfix, develop) = (oid(&fx, "hotfix/1.0.1"), oid(&fx, "develop"));

        let first = plan(&fx, &config, "hotfix/1.0.1");
        let cmd = finish(&config, &first, &deleting());
        assert!(cmd.conflict_note.as_deref().is_some_and(|note| note.contains("finish hotfix 1.0.1 again")));
        let done = fx.run_as(cmd, Origin::User);
        assert!(done.result.as_ref().is_err_and(|e| e.contains("CONFLICT")), "{:?}", done.result);
        assert!(done.undo.is_none());

        // Stopped in the back-merge: nothing is lost, nothing deleted.
        assert_eq!(done.snap.state, RepoState::Merge);
        assert_eq!(done.snap.head.branch.as_deref(), Some("develop"));
        assert_eq!(oid(&fx, "hotfix/1.0.1"), hotfix);
        assert_eq!(oid(&fx, "develop"), develop);
        assert_eq!(parents(&fx, "main")[1], hotfix);
        assert!(exists(&fx, "refs/tags/1.0.1"));
        assert!(plan(&fx, &config, "hotfix/1.0.1").blocked.is_some(), "not while the merge is open");

        // Resolve and commit the merge, then finish again: only the rest runs.
        fx.write("a.txt", "develop and hotfix\n");
        fx.run(ops::stage(&["a.txt".to_owned()]));
        fx.run(ops::continue_op(RepoState::Merge).unwrap());
        let second = plan(&fx, &config, "hotfix/1.0.1");
        assert_eq!(second.blocked, None);
        assert!(!second.merge && !second.create_tag && !second.back_merge, "{second:?}");
        assert_eq!(second.steps(&config, true), ["Delete hotfix/1.0.1"]);
        assert_eq!(second.done_already(&config).len(), 3);
        fx.run(finish(&config, &second, &deleting()));
        assert!(!exists(&fx, "refs/heads/hotfix/1.0.1"));
        assert_eq!(fx.read("a.txt"), "develop and hotfix\n");
    }

    #[test]
    fn finishing_is_refused_with_changes_or_a_clashing_tag() {
        let (fx, config) = set_up("gitflow-refused", "");
        fx.run(start(&config, Kind::Release, "2.0"));
        fx.write("version.txt", "2.0\n");
        fx.commit_all("Bump version");
        fx.write("a.txt", "uncommitted\n");
        assert!(plan(&fx, &config, "release/2.0").blocked.is_some_and(|b| b.contains("changes")));
        git(&fx.dir, &["checkout", "--", "a.txt"]);
        // Untracked files are no obstacle, as with git-flow.
        fx.write("notes.txt", "scratch\n");
        assert_eq!(plan(&fx, &config, "release/2.0").blocked, None);
        git(&fx.dir, &["tag", "2.0", "main"]);
        assert!(plan(&fx, &config, "release/2.0").blocked.is_some_and(|b| b.contains("tag 2.0")));
        assert!(plan(&fx, &config, "develop").blocked.is_some());
    }
}
