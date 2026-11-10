//! Preferences > General: fetching, submodules, new repositories, line endings, tabs.
//!
//! Shown by `crate::ui::prefs::general`. The auto-fetch interval itself is
//! the older top-level `Settings::auto_fetch_minutes` (0 turns it off); this
//! module keeps the interval to come back to and the switches below.
//!
//! Git commands are built and run where `Settings` is out of reach (the
//! operation builders, the operator thread), so the two switches they need
//! are copied into atomics by [`apply`] and read with [`auto_prune`] and
//! [`update_submodules`]: one atomic load per command.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::git::ops::GitCmd;
use crate::state::Settings;

/// Held by `Settings::general`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct General {
    /// Fetches pass `--prune`, removing remote branches deleted on the remote.
    pub auto_prune: bool,
    /// Runs `git submodule update --init --recursive` after an operation that
    /// moved HEAD, in repositories with submodules.
    pub update_submodules: bool,
    /// Reopens last session's tabs at start.
    pub remember_tabs: bool,
    /// The auto-fetch interval turning it back on restores: the last one in
    /// use, in minutes.
    pub resume_fetch_minutes: u32,
}

impl Default for General {
    fn default() -> Self {
        General { auto_prune: true, update_submodules: false, remember_tabs: true, resume_fetch_minutes: 5 }
    }
}

/// The largest interval the page offers, in minutes.
pub const MAX_FETCH_MINUTES: u32 = 60;

static AUTO_PRUNE: AtomicBool = AtomicBool::new(true);
static UPDATE_SUBMODULES: AtomicBool = AtomicBool::new(false);

/// Puts these preferences into effect; see [`crate::prefs::apply`]. Two
/// atomic stores, so there is nothing to compare first.
pub fn apply(_ctx: &egui::Context, settings: &Settings) {
    AUTO_PRUNE.store(settings.general.auto_prune, Ordering::Relaxed);
    UPDATE_SUBMODULES.store(settings.general.update_submodules, Ordering::Relaxed);
}

/// Whether fetches prune (Auto-Prune).
pub fn auto_prune() -> bool {
    AUTO_PRUNE.load(Ordering::Relaxed)
}

/// Whether operations that move HEAD update the submodules afterwards.
pub fn update_submodules() -> bool {
    UPDATE_SUBMODULES.load(Ordering::Relaxed)
}

// ---- auto-fetch interval ---------------------------------------------------

/// The interval in use, or the one turning auto-fetch on would use.
fn interval(settings: &Settings) -> u32 {
    match settings.auto_fetch_minutes {
        0 => match settings.general.resume_fetch_minutes {
            0 => General::default().resume_fetch_minutes,
            minutes => minutes,
        },
        minutes => minutes,
    }
}

/// "Fetch automatically every 5 minutes", with the real interval: for the
/// View menu and the command palette.
pub fn auto_fetch_label(settings: &Settings) -> String {
    match interval(settings) {
        1 => "Fetch automatically every minute".to_owned(),
        minutes => format!("Fetch automatically every {minutes} minutes"),
    }
}

/// Sets the interval (0 turns auto-fetch off), remembering the last one in
/// use for [`set_auto_fetch`].
pub fn set_fetch_interval(settings: &mut Settings, minutes: u32) {
    if settings.auto_fetch_minutes > 0 {
        settings.general.resume_fetch_minutes = settings.auto_fetch_minutes;
    }
    if minutes > 0 {
        settings.general.resume_fetch_minutes = minutes;
    }
    settings.auto_fetch_minutes = minutes;
}

/// Turns auto-fetch on at the last interval in use, or off.
pub fn set_auto_fetch(settings: &mut Settings, on: bool) {
    set_fetch_interval(settings, if on { interval(settings) } else { 0 });
}

/// Flips auto-fetch (the palette's toggle).
pub fn toggle_auto_fetch(settings: &mut Settings) {
    set_auto_fetch(settings, settings.auto_fetch_minutes == 0);
}

/// Brings the next automatic fetch forward when the interval was shortened
/// since it was scheduled, so a new interval counts from now instead of
/// waiting out the old one. A longer interval takes effect after the fetch
/// already scheduled.
pub fn reschedule(next: &mut Instant, now: Instant, minutes: u32) {
    let latest = now + Duration::from_secs(u64::from(minutes) * 60);
    if *next > latest {
        *next = latest;
    }
}

// ---- submodules -----------------------------------------------------------

/// Whether `cmd` moves HEAD to another commit: checkout, pull, merge,
/// rebase, reset to a commit, cherry-pick and revert (with their abort and
/// continue), in any step. Not commits, `reset --soft` or forms that name
/// paths (discarding or unstaging files): the submodules' recorded commits
/// stay as they are there, and updating would throw away a submodule the
/// user moved but has not staged.
pub fn moves_head(cmd: &GitCmd) -> bool {
    cmd.steps.iter().any(|step| {
        let mut args = step.args.as_slice();
        // Past `-c key=value` options, to the subcommand.
        while let [first, _, rest @ ..] = args
            && first == "-c"
        {
            args = rest;
        }
        let Some((subcommand, rest)) = args.split_first() else { return false };
        let names_paths = step.literal_paths
            || rest.iter().any(|a| a == "--" || a.starts_with("--pathspec-from-file"));
        match subcommand.as_str() {
            "checkout" | "switch" => !names_paths,
            "reset" => !names_paths && !rest.iter().any(|a| a == "--soft") && rest.iter().any(|a| !a.starts_with('-')),
            "pull" | "merge" | "rebase" | "cherry-pick" | "revert" => true,
            _ => false,
        }
    })
}

/// The command that brings the submodules of the repository at `workdir` to
/// the commits HEAD records, if one is due after `cmd` succeeded: `enabled`
/// (Keep submodules up to date), `cmd` moved HEAD and the repository has a
/// `.gitmodules` file. Repositories without one cost a file lookup, never a
/// process.
pub fn submodule_update(workdir: &Path, cmd: &GitCmd, enabled: bool) -> Option<GitCmd> {
    if !enabled || !moves_head(cmd) || !workdir.join(".gitmodules").is_file() {
        return None;
    }
    let mut update = GitCmd::new("Update submodules", &["submodule", "update", "--init", "--recursive"]).quiet();
    update.background = cmd.background;
    Some(update)
}

/// Runs [`submodule_update`] for the operator thread after `cmd` succeeded.
/// Returns the command and git's message when it failed, to report on its
/// own: the operation itself did what it was asked.
pub fn run_submodule_update(workdir: &Path, cmd: &GitCmd) -> Option<(GitCmd, String)> {
    let update = submodule_update(workdir, cmd, update_submodules())?;
    let options = crate::git::cli::Options { background: update.background, ..Default::default() };
    match crate::git::cli::run(workdir, &update.steps[0].args, options) {
        Ok(_) => None,
        Err(e) => Some((update, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::ops::{self, PullMode, ResetMode};
    use crate::tests::{Fixture, git};

    fn args(cmd: &GitCmd) -> Vec<&str> {
        cmd.steps[0].args.iter().map(String::as_str).collect()
    }

    #[test]
    fn auto_prune_decides_whether_fetches_prune() {
        // The only test that changes the switch; it puts it back.
        AUTO_PRUNE.store(false, Ordering::Relaxed);
        let (all, auto) = (ops::fetch_all(), ops::auto_fetch());
        AUTO_PRUNE.store(true, Ordering::Relaxed);
        assert_eq!(args(&all), ["fetch", "--all"]);
        assert_eq!(args(&auto), ["fetch", "--all"]);
        assert_eq!(args(&ops::fetch_all()), ["fetch", "--prune", "--all"]);
        assert_eq!(args(&ops::auto_fetch()), ["fetch", "--prune", "--all"]);
    }

    #[test]
    fn the_interval_comes_back_after_turning_auto_fetch_off_and_on() {
        let mut settings = Settings::default();
        assert_eq!(auto_fetch_label(&settings), "Fetch automatically every 5 minutes");
        set_fetch_interval(&mut settings, 12);
        assert_eq!(auto_fetch_label(&settings), "Fetch automatically every 12 minutes");

        toggle_auto_fetch(&mut settings);
        assert_eq!(settings.auto_fetch_minutes, 0);
        // Off, the label says what turning it on does.
        assert_eq!(auto_fetch_label(&settings), "Fetch automatically every 12 minutes");
        toggle_auto_fetch(&mut settings);
        assert_eq!(settings.auto_fetch_minutes, 12);

        // Typing 0 on the page is the same as turning it off.
        set_fetch_interval(&mut settings, 0);
        set_auto_fetch(&mut settings, true);
        assert_eq!(settings.auto_fetch_minutes, 12);

        set_fetch_interval(&mut settings, 1);
        assert_eq!(auto_fetch_label(&settings), "Fetch automatically every minute");

        // An older settings file without the remembered interval.
        settings.auto_fetch_minutes = 0;
        settings.general.resume_fetch_minutes = 0;
        set_auto_fetch(&mut settings, true);
        assert_eq!(settings.auto_fetch_minutes, 5);
    }

    #[test]
    fn a_shorter_interval_brings_the_next_fetch_forward() {
        let now = Instant::now();
        let mut next = now + Duration::from_secs(60 * 60);
        reschedule(&mut next, now, 60);
        assert_eq!(next, now + Duration::from_secs(60 * 60));
        reschedule(&mut next, now, 1);
        assert_eq!(next, now + Duration::from_secs(60));
        // Longer again: the fetch already due is kept.
        reschedule(&mut next, now, 30);
        assert_eq!(next, now + Duration::from_secs(60));
    }

    #[test]
    fn operations_that_move_head_are_told_apart() {
        let oid = crate::git::Oid::ZERO_SHA1;
        let moving = [
            ops::checkout("main"),
            ops::checkout_remote("origin/topic", "topic", false),
            ops::pull(PullMode::Rebase),
            ops::merge("topic"),
            ops::rebase("main"),
            ops::cherry_pick(oid, false),
            ops::revert(oid, false),
            ops::reset("HEAD~1", ResetMode::Hard),
            ops::reset("HEAD~1", ResetMode::Mixed),
            GitCmd::new("Undo", &["reset", "--keep", "abc123"]),
            GitCmd::new("Abort merge", &["merge", "--abort"]),
            GitCmd::new("Rebase", &["-c", "core.editor=true", "rebase", "-i", "main"]),
        ];
        for cmd in &moving {
            assert!(moves_head(cmd), "{:?} moves HEAD", cmd.steps[0].args);
        }
        let staying = [
            ops::commit("message", false),
            ops::reset("HEAD~1", ResetMode::Soft),
            ops::unstage(&["a.txt".to_owned()], false),
            ops::unstage_all(false),
            ops::discard_all(),
            ops::stage_all(),
            ops::fetch_all(),
            ops::create_branch("topic", None, false),
            ops::stash_pop(0),
            GitCmd::new("Discard", &["checkout", "--", "a.txt"]),
        ];
        for cmd in &staying {
            assert!(!moves_head(cmd), "{:?} leaves HEAD", cmd.steps[0].args);
        }
    }

    #[test]
    fn submodules_follow_head_only_when_asked_and_present() {
        let fx = Fixture::new("prefs-submodules-check");
        let checkout = ops::checkout("main");
        // No .gitmodules: nothing to run, whatever the switch says.
        assert!(submodule_update(&fx.dir, &checkout, true).is_none());
        fx.write(".gitmodules", "");
        assert!(submodule_update(&fx.dir, &checkout, false).is_none());
        assert!(submodule_update(&fx.dir, &ops::commit("m", false), true).is_none());
        let update = submodule_update(&fx.dir, &checkout, true).expect("an update is due");
        assert_eq!(args(&update), ["submodule", "update", "--init", "--recursive"]);
    }

    #[test]
    fn checkout_brings_the_submodule_to_the_recorded_commit() {
        // The submodule's own repository, with two commits.
        let lib = Fixture::new("prefs-submodule-lib");
        lib.write("lib.txt", "one\n");
        lib.commit_all("lib one");
        let first = git(&lib.dir, &["rev-parse", "HEAD"]);
        lib.write("lib.txt", "two\n");
        lib.commit_all("lib two");
        let second = git(&lib.dir, &["rev-parse", "HEAD"]);

        // The superproject records `first` on main and `second` on topic.
        let fx = Fixture::new("prefs-submodule-super");
        fx.write("app.txt", "app\n");
        fx.commit_all("app");
        let url = lib.dir.to_string_lossy().replace('\\', "/");
        git(&fx.dir, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", &url, "lib"]);
        git(&fx.dir.join("lib"), &["checkout", "-q", &first]);
        fx.commit_all("add lib at one");
        git(&fx.dir, &["checkout", "-q", "-b", "topic"]);
        git(&fx.dir.join("lib"), &["checkout", "-q", &second]);
        fx.commit_all("lib at two");
        git(&fx.dir, &["checkout", "-q", "main"]);
        // Plain git leaves the submodule where it was.
        assert_eq!(git(&fx.dir.join("lib"), &["rev-parse", "HEAD"]), second);
        git(&fx.dir.join("lib"), &["checkout", "-q", &first]);

        // The clone in .git/modules has the commits already, so the update
        // needs no fetch; allow the file protocol for it anyway.
        git(&fx.dir, &["config", "protocol.file.allow", "always"]);
        UPDATE_SUBMODULES.store(true, Ordering::Relaxed);
        let to_topic = fx.run_as(ops::checkout("topic"), crate::worker::Origin::User);
        let lib_at_topic = git(&fx.dir.join("lib"), &["rev-parse", "HEAD"]);
        let to_main = fx.run_as(ops::checkout("main"), crate::worker::Origin::User);
        let lib_at_main = git(&fx.dir.join("lib"), &["rev-parse", "HEAD"]);
        UPDATE_SUBMODULES.store(false, Ordering::Relaxed);

        to_topic.result.expect("checkout topic");
        to_main.result.expect("checkout main");
        assert_eq!(lib_at_topic, second);
        assert_eq!(lib_at_main, first);
        // The undo entry survives the extra step.
        assert!(to_main.undo.is_some());
    }
}
