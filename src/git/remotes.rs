//! Adding, editing and removing remotes: the `git remote` commands, and what
//! a remote has (its URLs, push URLs and fetch refspecs), read from the
//! configuration.
//!
//! Removing a remote can be undone. `git remote remove` deletes the remote's
//! section of `.git/config`, the upstream of every branch tracking it
//! (`branch.<b>.remote` and `branch.<b>.merge`), `branch.<b>.pushRemote` and
//! `remote.pushDefault` where they name it, and its remote-tracking branches.
//! [`read`] notes those settings beforehand; undo adds the remote back and
//! writes every one of them again. The remote-tracking branches come back
//! with the next fetch.

use super::ops::{self, GitCmd, Step, UndoKind};
use super::repo::lossy;

/// What a remote has, as the configuration says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Details {
    pub name: String,
    /// `remote.<name>.url`, in order: git fetches from the first, and pushes
    /// to all of them unless there are push URLs.
    pub urls: Vec<String>,
    /// `remote.<name>.pushurl`.
    pub push_urls: Vec<String>,
    /// `remote.<name>.fetch`.
    pub refspecs: Vec<String>,
    /// Local branches whose upstream is on this remote.
    pub tracking: Vec<String>,
    /// The settings in the repository's own configuration file that
    /// removing the remote takes away, as (key, value) in file order. What
    /// undo writes back.
    saved: Vec<Saved>,
}

/// A setting as (key, value).
type Saved = (String, String);

impl Details {
    /// The fetch URL (the first, if there are several).
    pub fn url(&self) -> &str {
        self.urls.first().map_or("", String::as_str)
    }

    /// The separate push URL, or "" when pushes go to the fetch URL.
    pub fn push_url(&self) -> &str {
        self.push_urls.first().map_or("", String::as_str)
    }

    /// Whether the remote fetches what `git remote add` sets it up to: every
    /// branch, into `refs/remotes/<name>/`.
    pub fn default_refspecs(&self) -> bool {
        self.refspecs == [default_refspec(&self.name)]
    }
}

/// The refspec `git remote add` gives a remote.
fn default_refspec(name: &str) -> String {
    format!("+refs/heads/*:refs/remotes/{name}/*")
}

/// One configuration entry: key as libgit2 gives it (section and variable
/// in lower case), value, and whether it sits in the repository's own
/// configuration file rather than the global one or an included file.
struct Setting {
    key: String,
    value: String,
    own: bool,
}

/// Reads what the remote `name` has. For the reader thread
/// (`tab.spawn_job`).
pub fn read(repo: &git2::Repository, name: &str) -> Result<Details, String> {
    let message = |e: git2::Error| e.message().to_owned();
    if repo.find_remote(name).is_err() {
        return Err(format!("There is no remote named {name}."));
    }
    let snapshot = repo.config().and_then(|mut config| config.snapshot()).map_err(message)?;
    let mut entries = snapshot.entries(None).map_err(message)?;
    let mut settings = Vec::new();
    while let Some(entry) = entries.next() {
        let entry = entry.map_err(message)?;
        // A key written without a value (`mirror`) means true.
        let value = if entry.has_value() { lossy(entry.value_bytes()) } else { "true".to_owned() };
        settings.push(Setting {
            key: lossy(entry.name_bytes()),
            value,
            own: entry.level() == git2::ConfigLevel::Local && entry.include_depth() == 0,
        });
    }
    Ok(details(name, &settings))
}

/// `section.sub.section.variable` as (section, subsection, variable). The
/// subsection, where remote and branch names go, may hold dots itself.
fn split_key(key: &str) -> Option<(&str, &str, &str)> {
    let (section, rest) = key.split_once('.')?;
    Some(match rest.rsplit_once('.') {
        Some((sub, var)) => (section, sub, var),
        None => (section, "", rest),
    })
}

fn details(name: &str, settings: &[Setting]) -> Details {
    let mut found = Details { name: name.to_owned(), ..Details::default() };
    fn in_section<'a>(setting: &'a Setting, section: &str) -> Option<(&'a str, &'a str)> {
        split_key(&setting.key).filter(|(sec, _, _)| sec.eq_ignore_ascii_case(section)).map(|(_, sub, var)| (sub, var))
    }
    // Branches whose upstream is on the remote, wherever that is set.
    let mut tracking: Vec<&str> = Vec::new();
    for setting in settings {
        if let Some((branch, "remote")) = in_section(setting, "branch")
            && setting.value == name
            && !tracking.contains(&branch)
        {
            tracking.push(branch);
        }
    }
    for setting in settings {
        let removed = match in_section(setting, "remote") {
            Some((sub, var)) if sub == name => {
                match var {
                    "url" => found.urls.push(setting.value.clone()),
                    "pushurl" => found.push_urls.push(setting.value.clone()),
                    "fetch" => found.refspecs.push(setting.value.clone()),
                    _ => {}
                }
                true
            }
            Some(("", "pushdefault")) => setting.value == name,
            _ => match in_section(setting, "branch") {
                Some((branch, "remote" | "merge")) => tracking.contains(&branch),
                Some((_, "pushremote")) => setting.value == name,
                _ => false,
            },
        };
        if removed && setting.own {
            found.saved.push((setting.key.clone(), setting.value.clone()));
        }
    }
    found.tracking = tracking.into_iter().map(str::to_owned).collect();
    found
}

/// Why `name` cannot name a remote, or `Ok` when it can. Git's rule:
/// `refs/remotes/<name>/…` must be a valid ref name (`git check-ref-format`).
/// A leading dash is refused too, though git would take it after `--`: the
/// remote could not be named on the command line afterwards.
pub fn check_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Give the remote a name.");
    }
    if name.starts_with('-') {
        return Err("A remote name cannot start with a dash.");
    }
    if name.contains(' ') {
        return Err("A remote name cannot contain spaces.");
    }
    if name.chars().any(|c| c.is_ascii_control() || "~^:?*[\\".contains(c)) {
        return Err("A remote name cannot contain ~ ^ : ? * [ or \\.");
    }
    if name.contains("..") {
        return Err("A remote name cannot contain \"..\".");
    }
    if name.contains("@{") {
        return Err("A remote name cannot contain \"@{\".");
    }
    for part in name.split('/') {
        if part.is_empty() {
            return Err("A remote name cannot start or end with a slash, or have two in a row.");
        }
        if part.starts_with('.') {
            return Err("No part of a remote name can start with a dot.");
        }
        if part.ends_with(".lock") {
            return Err("No part of a remote name can end with \".lock\".");
        }
    }
    Ok(())
}

/// The remote among `existing` that `name` would clash with: the same name,
/// or one that differs only in case, since remote-tracking branches are
/// files and Windows and macOS do not tell those names apart.
pub fn clash<'a>(name: &str, existing: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    existing.into_iter().find(|other| other.to_lowercase() == name.to_lowercase())
}

/// Why `url` cannot be a remote's URL, or `Ok`.
pub fn check_url(url: &str) -> Result<(), &'static str> {
    if url.trim().is_empty() {
        return Err("Enter the URL to fetch from.");
    }
    if url.trim().starts_with('-') {
        return Err("A URL cannot start with a dash.");
    }
    Ok(())
}

/// Adds the remote `name`, fetching from `url` and pushing to `push_url`
/// (or to `url` when it is empty), then fetches it if asked. Undo removes it
/// again.
pub fn add(name: &str, url: &str, push_url: &str, fetch: bool) -> GitCmd {
    let mut cmd = GitCmd::new(format!("Add remote {name}"), &["remote", "add", "--", name, url]);
    if !push_url.is_empty() {
        cmd = cmd.step(&["remote", "set-url", "--push", "--", name, push_url]);
    }
    if fetch {
        cmd = cmd.step(&ops::fetch(&[name]));
    }
    let undo = GitCmd::new(format!("Remove remote {name}"), &["remote", "remove", "--", name]);
    cmd.done(format!("Added remote {name}")).undoable(UndoKind::Inverse(Box::new(undo)))
}

/// Fetches one remote (pruning unless Auto-Prune is off).
pub fn fetch(name: &str) -> GitCmd {
    GitCmd::new(format!("Fetch {name}"), &ops::fetch(&[name])).done(format!("Fetched {name}"))
}

/// Deletes the remote-tracking branches whose branch is gone from the
/// remote, without fetching anything.
pub fn prune(name: &str) -> GitCmd {
    GitCmd::new(format!("Prune {name}"), &["remote", "prune", "--", name]).done(format!("Pruned {name}"))
}

/// Changes what differs between `current` and the name, fetch URL and push
/// URL given (an empty push URL: push to the fetch URL). `None` when nothing
/// does. Renaming moves the remote-tracking branches, and the upstream of
/// branches tracking the remote; git does both. Undo puts everything back.
///
/// With several fetch or push URLs, the first is the one changed.
pub fn edit(current: &Details, name: &str, url: &str, push_url: &str) -> Option<GitCmd> {
    let old = current.name.as_str();
    let renamed = name != old;
    let new_url = url != current.url();
    let new_push = push_url != current.push_url();
    if !(renamed || new_url || new_push) {
        return None;
    }
    let pushurl_key = |remote: &str| format!("remote.{remote}.pushurl");

    let mut cmd = GitCmd::empty(format!("Edit remote {old}"));
    // Undo runs in the opposite order, on the remote's new name.
    let mut undo = GitCmd::empty(format!("Edit remote {name}"));
    if renamed {
        cmd = cmd.step(&["remote", "rename", "--", old, name]);
    }
    if new_url {
        cmd = with(cmd, set_url(name, url, current.urls.len(), current.url(), false));
    }
    if new_push {
        // Undo writes the push URLs back exactly as they were.
        if !push_url.is_empty() {
            undo = undo.step(&["config", "--unset-all", &pushurl_key(name)]);
        }
        for push in &current.push_urls {
            undo = undo.step(&["config", "--add", &pushurl_key(name), push]);
        }
        cmd = if push_url.is_empty() {
            cmd.step(&["config", "--unset-all", &pushurl_key(name)])
        } else {
            with(cmd, set_url(name, push_url, current.push_urls.len(), current.push_url(), true))
        };
    }
    if new_url {
        undo = with(undo, set_url(name, current.url(), current.urls.len(), url, false));
    }
    if renamed {
        undo = undo.step(&["remote", "rename", "--", name, old]);
    }

    let urls = match (new_url, new_push) {
        (true, false) => "URL",
        (false, true) => "push URL",
        _ => "URLs",
    };
    let done = match (renamed, new_url || new_push) {
        (true, false) => format!("Renamed {old} to {name}"),
        (true, true) => format!("Renamed {old} to {name} and changed its {urls}"),
        (false, _) => format!("Changed the {urls} of {name}"),
    };
    Some(cmd.done(done).undoable(UndoKind::Inverse(Box::new(undo))))
}

/// `cmd` with one more step.
fn with(mut cmd: GitCmd, args: Vec<String>) -> GitCmd {
    cmd.steps.push(Step::new(args, None));
    cmd
}

/// `git remote set-url [--push] <remote> <new>`: replaces the URL when there
/// is at most one (adds it when there is none), and only the one that is
/// `old` when there are `count` of them.
fn set_url(remote: &str, new: &str, count: usize, old: &str, push: bool) -> Vec<String> {
    let mut args = vec!["remote".to_owned(), "set-url".to_owned()];
    if push {
        args.push("--push".to_owned());
    }
    args.extend(["--".to_owned(), remote.to_owned(), new.to_owned()]);
    if count > 1 {
        args.push(format!("^{}$", escape_regex(old)));
    }
    args
}

/// `text` as a POSIX extended regular expression matching only itself.
fn escape_regex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.[](){}*+?^$|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Removes the remote `details` describes. Undo adds it back, with every
/// setting removing it takes away; see the module documentation.
pub fn remove(details: &Details) -> GitCmd {
    let name = &details.name;
    let cmd = GitCmd::new(format!("Remove remote {name}"), &["remote", "remove", "--", name])
        .done(format!("Removed remote {name}"));
    match restore(details) {
        Some(undo) => cmd.undoable(UndoKind::Inverse(Box::new(undo))),
        None => cmd,
    }
}

/// Whether removing the remote can be undone: it has a fetch URL in the
/// repository's own configuration to add it back with.
pub fn can_restore(details: &Details) -> bool {
    restore(details).is_some()
}

/// Adds the remote back as [`read`] found it.
fn restore(details: &Details) -> Option<GitCmd> {
    let name = details.name.as_str();
    let is_own = |key: &str| {
        split_key(key).is_some_and(|(section, sub, _)| section.eq_ignore_ascii_case("remote") && sub == name)
    };
    let (own, others): (Vec<&Saved>, Vec<&Saved>) = details.saved.iter().partition(|(key, _)| is_own(key));
    let url_key = format!("remote.{name}.url");
    let url = own.iter().find(|(key, _)| key.eq_ignore_ascii_case(&url_key)).map(|(_, value)| value)?;

    // `git remote add` refuses a name in use again, which keeps undo from
    // merging into a remote added since. It writes the URL and the default
    // refspec; anything else and the section is written again as it was.
    let mut cmd = GitCmd::new(format!("Add remote {name} back"), &["remote", "add", "--", name, url]);
    let plain = [(url_key.clone(), url.clone()), (format!("remote.{name}.fetch"), default_refspec(name))];
    let is_plain = own.len() == plain.len()
        && own.iter().zip(&plain).all(|((key, value), (want_key, want))| key.eq_ignore_ascii_case(want_key) && value == want);
    if !is_plain {
        cmd = cmd.step(&["config", "--remove-section", &format!("remote.{name}")]);
        cmd = write_back(cmd, &own);
    }
    Some(write_back(cmd, &others).done(format!("Added {name} back: fetch it to get its branches")))
}

/// Steps writing `settings` into the repository's configuration in their
/// order, the first value of each key replacing what the key has now.
fn write_back(mut cmd: GitCmd, settings: &[&Saved]) -> GitCmd {
    let mut seen: Vec<&str> = Vec::new();
    for (key, value) in settings {
        let how = if seen.contains(&key.as_str()) { "--add" } else { "--replace-all" };
        seen.push(key);
        cmd = cmd.step(&["config", how, key, value]);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    fn read_in(fx: &Fixture, name: &str) -> Result<Details, String> {
        let name = name.to_owned();
        fx.job(move |loader| read(loader.repo(), &name))
    }

    /// A repository with two commits on main and a feature branch, to be a
    /// remote of others. Its folder name has a space and an accent, so its
    /// path makes an awkward URL.
    fn upstream(name: &str) -> Fixture {
        let up = Fixture::new(name);
        up.write("a.txt", "one\n");
        up.commit_all("first");
        git(&up.dir, &["branch", "feature/x"]);
        up.write("a.txt", "two\n");
        up.commit_all("second");
        up
    }

    /// The same remote, though restoring may have moved its settings
    /// around in the file: each key keeps the order of its values.
    fn assert_same(a: &Details, b: &Details) {
        let sorted = |d: &Details| {
            let mut saved = d.saved.clone();
            saved.sort_by(|x, y| x.0.cmp(&y.0));
            Details { saved, ..d.clone() }
        };
        assert_eq!(sorted(a), sorted(b));
    }

    fn url_of(fx: &Fixture) -> String {
        fx.dir.to_string_lossy().replace('\\', "/")
    }

    fn remote_branches(snap: &crate::git::RepoSnapshot) -> Vec<String> {
        let mut names: Vec<String> = snap.remote_branches.iter().map(|b| b.full.clone()).collect();
        names.sort();
        names
    }

    fn config(fx: &Fixture, pattern: &str) -> String {
        let out = std::process::Command::new("git")
            .current_dir(&fx.dir)
            .args(["config", "--local", "--get-regexp", pattern])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[test]
    fn adding_with_a_push_url_then_fetching_and_undoing() {
        let up = upstream("remotes add ü space");
        let url = url_of(&up);
        assert!(url.contains(' ') && url.contains('ü'));
        let fx = Fixture::new("remotes-add");
        fx.write("b.txt", "local\n");
        fx.commit_all("local");

        let push = "C:/somewhere else/pushes é.git";
        let done = fx.run(add("équipe", &url, push, true));
        let names: Vec<&str> = done.snap.remotes.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["équipe"]);
        assert_eq!(done.snap.remotes[0].url, url);
        assert_eq!(remote_branches(&done.snap), ["équipe/feature/x", "équipe/main"]);

        let details = read_in(&fx, "équipe").unwrap();
        assert_eq!(details.urls, [url.as_str()]);
        assert_eq!(details.push_urls, [push]);
        assert!(details.default_refspecs());
        assert!(details.tracking.is_empty());

        // Undo removes it with its branches; redo adds and fetches again.
        let entry = done.undo.expect("adding is undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert!(undone.snap.remotes.is_empty());
        assert!(undone.snap.remote_branches.is_empty());
        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
        redone.result.expect("redo works");
        assert_eq!(remote_branches(&redone.snap), ["équipe/feature/x", "équipe/main"]);
        assert_eq!(read_in(&fx, "équipe").unwrap().push_urls, [push]);

        // Without fetching, nothing arrives.
        let done = fx.run(add("other", &url, "", false));
        assert_eq!(done.snap.remotes.len(), 2);
        assert!(!remote_branches(&done.snap).iter().any(|b| b.starts_with("other/")));
        assert!(read_in(&fx, "other").unwrap().push_urls.is_empty());
    }

    #[test]
    fn git_refuses_a_name_in_use() {
        let up = upstream("remotes-dup-up");
        let fx = Fixture::new("remotes-dup");
        fx.run(add("origin", &url_of(&up), "", false));
        let again = fx.run_as(add("origin", "E:/elsewhere.git", "", false), Origin::User);
        assert!(again.result.unwrap_err().contains("already exists"));
        assert!(again.undo.is_none());
        assert_eq!(read_in(&fx, "origin").unwrap().urls, [url_of(&up)]);
        assert!(read_in(&fx, "nothere").unwrap_err().contains("no remote named nothere"));
    }

    #[test]
    fn renaming_moves_the_remote_branches_and_the_upstreams() {
        let up = upstream("remotes-rename-up");
        let fx = Fixture::new("remotes-rename");
        fx.run(add("origin", &url_of(&up), "", true));
        git(&fx.dir, &["checkout", "-q", "-b", "main", "--track", "origin/main"]);
        git(&fx.dir, &["branch", "-q", "--track", "feature/x", "origin/feature/x"]);
        git(&fx.dir, &["config", "remote.pushDefault", "origin"]);

        let details = read_in(&fx, "origin").unwrap();
        assert_eq!(details.tracking, ["main", "feature/x"]);
        let cmd = edit(&details, "thé/fork", details.url(), "").expect("the name changed");
        assert_eq!(cmd.success.as_deref(), Some("Renamed origin to thé/fork"));
        let done = fx.run(cmd);
        let snap = &done.snap;
        assert_eq!(snap.remotes.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["thé/fork"]);
        assert_eq!(remote_branches(snap), ["thé/fork/feature/x", "thé/fork/main"]);
        let main = snap.branches.iter().find(|b| b.name == "main").unwrap();
        assert_eq!(main.upstream.as_deref(), Some("thé/fork/main"));
        assert_eq!(git(&fx.dir, &["config", "remote.pushDefault"]), "thé/fork");
        let renamed = read_in(&fx, "thé/fork").unwrap();
        assert!(renamed.default_refspecs(), "git rewrote the refspec: {:?}", renamed.refspecs);

        // Undo renames it back, upstreams included.
        let entry = done.undo.expect("editing is undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(remote_branches(&undone.snap), ["origin/feature/x", "origin/main"]);
        let main = undone.snap.branches.iter().find(|b| b.name == "main").unwrap();
        assert_eq!(main.upstream.as_deref(), Some("origin/main"));
        assert_same(&read_in(&fx, "origin").unwrap(), &details);
    }

    #[test]
    fn urls_change_and_clearing_the_push_url_removes_it() {
        let up = upstream("remotes-url-up");
        let fx = Fixture::new("remotes-url");
        let first = url_of(&up);
        fx.run(add("origin", &first, "E:/push (old).git", false));
        let before = read_in(&fx, "origin").unwrap();
        assert!(edit(&before, "origin", &first, "E:/push (old).git").is_none(), "nothing changed");

        // A new fetch URL and no push URL.
        let other = "https://example.com/über repo.git";
        let cmd = edit(&before, "origin", other, "").unwrap();
        assert_eq!(cmd.success.as_deref(), Some("Changed the URLs of origin"));
        let done = fx.run(cmd);
        assert_eq!(done.snap.remotes[0].url, other);
        let after = read_in(&fx, "origin").unwrap();
        assert_eq!((after.urls.as_slice(), after.push_urls.len()), (&[other.to_owned()][..], 0));
        assert_eq!(config(&fx, "pushurl"), "");

        let entry = done.undo.unwrap();
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert_same(&read_in(&fx, "origin").unwrap(), &before);

        // Only the push URL, then a remote with no push URL getting one.
        let done = fx.run(edit(&before, "origin", &first, "E:/push new.git").unwrap());
        assert_eq!(read_in(&fx, "origin").unwrap().push_urls, ["E:/push new.git"]);
        let entry = done.undo.unwrap();
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert_same(&read_in(&fx, "origin").unwrap(), &before);
        fx.run(edit(&before, "origin", &first, "").unwrap());
        let plain = read_in(&fx, "origin").unwrap();
        let done = fx.run(edit(&plain, "origin", &first, "E:/push again.git").unwrap());
        assert_eq!(read_in(&fx, "origin").unwrap().push_urls, ["E:/push again.git"]);
        let entry = done.undo.unwrap();
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert_same(&read_in(&fx, "origin").unwrap(), &plain);
    }

    #[test]
    fn with_several_urls_only_the_first_changes() {
        let fx = Fixture::new("remotes-multi");
        git(&fx.dir, &["remote", "add", "origin", "E:/one.git"]);
        git(&fx.dir, &["remote", "set-url", "--add", "origin", "E:/two (copy).git"]);
        git(&fx.dir, &["remote", "set-url", "--add", "--push", "origin", "E:/push.one.git"]);
        git(&fx.dir, &["remote", "set-url", "--add", "--push", "origin", "E:/push.two.git"]);
        let before = read_in(&fx, "origin").unwrap();
        assert_eq!(before.urls.len(), 2);

        let done = fx.run(edit(&before, "origin", "E:/three.git", "E:/push.three.git").unwrap());
        let after = read_in(&fx, "origin").unwrap();
        assert_eq!(after.urls, ["E:/three.git", "E:/two (copy).git"]);
        assert_eq!(after.push_urls, ["E:/push.three.git", "E:/push.two.git"]);
        let entry = done.undo.unwrap();
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert_same(&read_in(&fx, "origin").unwrap(), &before);
    }

    #[test]
    fn removing_and_undoing_brings_every_setting_back() {
        let up = upstream("remotes-remove-up");
        let fx = Fixture::new("remotes-remove");
        fx.run(add("origin", &url_of(&up), "", true));
        git(&fx.dir, &["checkout", "-q", "-b", "main", "--track", "origin/main"]);
        git(&fx.dir, &["branch", "-q", "--track", "feature/x", "origin/feature/x"]);
        git(&fx.dir, &["branch", "-q", "solo"]);
        git(&fx.dir, &["config", "branch.solo.pushRemote", "origin"]);
        git(&fx.dir, &["config", "remote.pushDefault", "origin"]);
        // Settings beyond what `git remote add` writes.
        git(&fx.dir, &["config", "remote.origin.tagOpt", "--no-tags"]);
        git(&fx.dir, &["config", "--add", "remote.origin.fetch", "+refs/tags/v*:refs/tags/v*"]);
        git(&fx.dir, &["config", "--add", "remote.origin.pushurl", "E:/push here.git"]);
        let settings = "^(remote|branch)\\.";
        let before_config = config(&fx, settings);
        let before = read_in(&fx, "origin").unwrap();
        assert!(!before.default_refspecs());
        assert!(can_restore(&before));

        let done = fx.run(remove(&before));
        assert!(done.snap.remotes.is_empty() && done.snap.remote_branches.is_empty());
        let main = done.snap.branches.iter().find(|b| b.name == "main").unwrap();
        assert_eq!(main.upstream, None);
        assert_eq!(config(&fx, settings), "");

        let entry = done.undo.expect("removing is undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert_same(&read_in(&fx, "origin").unwrap(), &before);
        // The same settings, keys in lower case as libgit2 wrote them down.
        let mut want: Vec<String> = before_config.lines().map(|l| l.to_lowercase()).collect();
        let mut got: Vec<String> = config(&fx, settings).lines().map(|l| l.to_lowercase()).collect();
        want.sort();
        got.sort();
        assert_eq!(got, want);
        // The branches come back with the next fetch.
        assert!(undone.snap.remote_branches.is_empty());
        let fetched = fx.run(fetch("origin"));
        assert_eq!(remote_branches(&fetched.snap), ["origin/feature/x", "origin/main"]);
        let main = fetched.snap.branches.iter().find(|b| b.name == "main").unwrap();
        assert_eq!(main.upstream.as_deref(), Some("origin/main"));

        // Undo refuses once the name is taken again, and changes nothing.
        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry.clone()));
        redone.result.expect("redo works");
        git(&fx.dir, &["remote", "add", "origin", "E:/someone else.git"]);
        let refused = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        assert!(refused.result.unwrap_err().contains("already exists"));
        assert_eq!(read_in(&fx, "origin").unwrap().urls, ["E:/someone else.git"]);
    }

    #[test]
    fn a_plain_remote_comes_back_in_one_step() {
        let fx = Fixture::new("remotes-plain");
        git(&fx.dir, &["remote", "add", "my.fork", "E:/fork.git"]);
        let details = read_in(&fx, "my.fork").unwrap();
        assert!(details.default_refspecs());
        let UndoKind::Inverse(undo) = remove(&details).undo else { panic!("undoable") };
        assert_eq!(undo.steps.len(), 1, "{:?}", undo.steps);
        assert_eq!(undo.steps[0].args, ["remote", "add", "--", "my.fork", "E:/fork.git"]);
    }

    #[test]
    fn prune_drops_branches_gone_from_the_remote() {
        let up = upstream("remotes-prune-up");
        let fx = Fixture::new("remotes-prune");
        fx.run(add("origin", &url_of(&up), "", true));
        git(&up.dir, &["branch", "-D", "feature/x"]);
        let done = fx.run(prune("origin"));
        assert_eq!(remote_branches(&done.snap), ["origin/main"]);
        assert!(done.undo.is_none());
        // Nothing to prune is not a failure.
        fx.run(prune("origin"));
    }

    #[test]
    fn names_follow_git_check_ref_format() {
        let fx = Fixture::new("remotes-names");
        let names = [
            "origin", "my.fork", "thé", "team/fork", "a@b", "@", "x.", "UPPER", "a-b_c", "日本",
            "", "a b", "a..b", "a/", "/a", "a//b", ".a", "a/.b", "a.lock", "a/b.lock/c", "a~b", "a^b", "a:b",
            "a?b", "a*b", "a[b", "a\\b", "a@{b", "tab\there",
        ];
        for name in names {
            let ours = check_name(name).is_ok();
            let theirs = std::process::Command::new("git")
                .current_dir(&fx.dir)
                .args(["check-ref-format", &format!("refs/remotes/{name}/x")])
                .status()
                .unwrap()
                .success();
            assert_eq!(ours, theirs && !name.is_empty(), "{name:?}: {:?}", check_name(name));
        }
        assert!(check_name("-x").is_err(), "git takes it, but it reads as an option");
        assert_eq!(clash("Origin", ["upstream", "origin"]), Some("origin"));
        assert_eq!(clash("fork", ["upstream", "origin"]), None);
        assert!(check_url("  ").is_err() && check_url("-oProxyCommand=x").is_err());
        assert!(check_url("C:/with space/ü.git").is_ok());
    }

    #[test]
    fn settings_are_sorted_into_what_removing_takes_away() {
        let setting = |key: &str, value: &str, own: bool| Setting { key: key.into(), value: value.into(), own };
        let settings = [
            setting("branch.main.merge", "refs/heads/main", true),
            setting("remote.my.fork.url", "E:/a.git", true),
            setting("branch.main.remote", "my.fork", true),
            setting("remote.my.fork.fetch", "+refs/heads/*:refs/remotes/my.fork/*", true),
            setting("remote.my.url", "E:/other.git", true),
            setting("branch.dev.remote", "my", true),
            setting("branch.dev.merge", "refs/heads/dev", true),
            setting("branch.dev.pushremote", "my.fork", true),
            setting("remote.pushdefault", "my.fork", true),
            setting("remote.my.fork.pushurl", "E:/global.git", false),
        ];
        let found = details("my.fork", &settings);
        assert_eq!(found.urls, ["E:/a.git"]);
        assert_eq!(found.push_urls, ["E:/global.git"], "read wherever it is set");
        assert_eq!(found.tracking, ["main"]);
        let saved: Vec<&str> = found.saved.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            saved,
            [
                "branch.main.merge",
                "remote.my.fork.url",
                "branch.main.remote",
                "remote.my.fork.fetch",
                "branch.dev.pushremote",
                "remote.pushdefault",
            ]
        );
        assert_eq!(escape_regex("E:/a (b).git"), "E:/a \\(b\\)\\.git");
    }
}
