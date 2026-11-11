//! `git bisect`: where a bisect stands, whether it was started here or in a
//! terminal, and the commands that move it on.
//!
//! Git keeps a bisect in files of the git directory (`BISECT_START`, what to
//! check out again at the end; `BISECT_TERMS`, the words for bad and good,
//! which `--term-new` / `--term-old` change; `BISECT_NAMES`, the paths it is
//! limited to; `BISECT_FIRST_PARENT`) and in refs under `refs/bisect/`: the
//! bad commit, and one ref per good and per skipped commit. [`read`] reads
//! them all back and works out what is left, as git would.

use std::collections::HashSet;
use std::path::Path;

use git2::Repository;

use super::Oid;
use super::cli::{self, Options};
use super::ops::GitCmd;

/// The words a bisect marks commits with. Git's are "bad" (the commit has
/// the change looked for) and "good"; `git bisect start --term-new=broken
/// --term-old=fixed` picks others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terms {
    pub bad: String,
    pub good: String,
}

impl Default for Terms {
    fn default() -> Self {
        Terms { bad: "bad".to_owned(), good: "good".to_owned() }
    }
}

/// How far a bisect has come.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// A bad and a good commit are both needed; this one is not known yet.
    Needs { bad: bool, good: bool },
    /// Commits are left to test; roughly this many more answers find the
    /// first bad commit (the checked-out commit's included).
    Testing { left: usize, steps: usize },
    /// Git named the first bad commit.
    Found(Oid),
    /// Only skipped commits are left to test: the first bad commit is one of
    /// these (the bad commit last).
    OnlySkipped(Vec<Oid>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub terms: Terms,
    /// The commit marked bad: the oldest known to have the change.
    pub bad: Option<Oid>,
    pub good: Vec<Oid>,
    pub skipped: Vec<Oid>,
    /// What stopping checks out again: a branch name, or a commit.
    pub start: String,
    pub progress: Progress,
}

impl State {
    /// Whether `oid` is the commit git named as the first bad one.
    pub fn is_found(&self, oid: Oid) -> bool {
        self.progress == Progress::Found(oid)
    }

    /// What stopping goes back to, for the user: a branch name, or a short
    /// SHA.
    pub fn start_label(&self) -> String {
        match Oid::from_str(&self.start) {
            Ok(oid) if self.start.len() == 40 => super::short(oid),
            _ => self.start.clone(),
        }
    }
}

/// Reads the bisect in progress in `repo` (run on the reader thread: it
/// starts `git for-each-ref` and `git rev-list`). `None` when there is none.
pub fn read(repo: &Repository) -> Result<Option<State>, String> {
    let gitdir = repo.path();
    let Ok(start) = std::fs::read_to_string(gitdir.join("BISECT_START")) else { return Ok(None) };
    let workdir = repo.workdir().ok_or("Repository has no working directory")?;
    let terms = read_terms(gitdir);

    let listed = run(workdir, &["for-each-ref", "--format=%(objectname) %(refname)", "refs/bisect/"], None)?;
    let (mut bad, mut good, mut skipped) = (None, Vec::new(), Vec::new());
    let bad_ref = format!("refs/bisect/{}", terms.bad);
    let good_prefix = format!("refs/bisect/{}-", terms.good);
    for line in listed.lines() {
        let Some((sha, name)) = line.split_once(' ') else { continue };
        let Ok(oid) = Oid::from_str(sha) else { continue };
        if name == bad_ref {
            bad = Some(oid);
        } else if name.starts_with(&good_prefix) {
            good.push(oid);
        } else if name.starts_with("refs/bisect/skip-") {
            skipped.push(oid);
        }
    }

    let progress = match bad {
        Some(bad) if !good.is_empty() => {
            // What git still considers: reachable from the bad commit and
            // not from a good one, along first parents only and touching
            // the bisect's paths if it was started so.
            let mut args = vec!["rev-list".to_owned(), "--stdin".to_owned()];
            if gitdir.join("BISECT_FIRST_PARENT").exists() {
                args.push("--first-parent".to_owned());
            }
            args.push("--".to_owned());
            args.extend(read_paths(gitdir));
            let mut input = format!("{bad}\n");
            for oid in &good {
                input.push_str(&format!("^{oid}\n"));
            }
            let listed = cli::run(workdir, &args, Options { stdin: Some(input.as_bytes()), ..Default::default() })?;
            let mut candidates: Vec<Oid> = listed.lines().filter_map(|l| Oid::from_str(l.trim()).ok()).collect();
            if !candidates.contains(&bad) {
                candidates.insert(0, bad);
            }
            progress(bad, &candidates, &skipped)
        }
        _ => Progress::Needs { bad: bad.is_none(), good: good.is_empty() },
    };
    Ok(Some(State { terms, bad, good, skipped, start: start.trim().to_owned(), progress }))
}

/// Where a bisect stands, from the commits still possible as the first bad
/// one (`candidates`, the bad one among them) and the skipped commits.
fn progress(bad: Oid, candidates: &[Oid], skipped: &[Oid]) -> Progress {
    let skipped: HashSet<Oid> = skipped.iter().copied().collect();
    let untested = candidates.iter().filter(|oid| **oid != bad && !skipped.contains(oid)).count();
    if untested > 0 {
        let all = candidates.iter().filter(|oid| !skipped.contains(oid)).count();
        return Progress::Testing { left: untested, steps: estimate_steps(all) + 1 };
    }
    let mut possible: Vec<Oid> = candidates.iter().copied().filter(|oid| *oid != bad).collect();
    if possible.is_empty() {
        return Progress::Found(bad);
    }
    possible.push(bad);
    Progress::OnlySkipped(possible)
}

/// Git's estimate of the answers still needed after the next one, among
/// `all` commits (bisect.c, `estimate_bisect_steps`).
fn estimate_steps(all: usize) -> usize {
    if all < 3 {
        return 0;
    }
    let n = usize::BITS - 1 - all.leading_zeros();
    let e = 1usize << n;
    let x = all - e;
    let n = n as usize;
    if e < 3 * x { n } else { n - 1 }
}

fn read_terms(gitdir: &Path) -> Terms {
    let text = std::fs::read_to_string(gitdir.join("BISECT_TERMS")).unwrap_or_default();
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    match (lines.next(), lines.next()) {
        (Some(bad), Some(good)) => Terms { bad: bad.to_owned(), good: good.to_owned() },
        _ => Terms::default(),
    }
}

/// The paths `git bisect start -- <paths>` limited the bisect to, kept
/// shell-quoted (`'--' 'a b.txt'`) in `BISECT_NAMES`.
fn read_paths(gitdir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(gitdir.join("BISECT_NAMES")).unwrap_or_default();
    let mut words = dequote(&text);
    if words.first().is_some_and(|w| w == "--") {
        words.remove(0);
    }
    words
}

/// Undoes git's `sq_quote`: words in single quotes, `'\''` for a quote.
fn dequote(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return words;
        }
        let mut word = String::new();
        while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
            match c {
                '\'' => word.extend(chars.by_ref().take_while(|c| *c != '\'')),
                '\\' => word.extend(chars.next()),
                c => word.push(c),
            }
        }
        words.push(word);
    }
}

fn run(workdir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    cli::run(workdir, &args, Options { stdin, ..Default::default() })
}

/// Starts a bisect with `oid` marked bad (or good). Git then waits for the
/// other kind before it checks anything out.
pub fn start(oid: Oid, bad: bool) -> GitCmd {
    let term = if bad { "bad" } else { "good" };
    GitCmd::new("Start bisect", &["bisect", "start"]).step(&["bisect", term, &oid.to_string()]).quiet()
}

/// Marks `oid` (the checked-out commit when `None`) with `term`: one of
/// [`Terms`], or "skip". Git checks out the next commit to test.
pub fn mark(term: &str, oid: Option<Oid>) -> GitCmd {
    let mut args = vec!["bisect".to_owned(), term.to_owned()];
    args.extend(oid.map(|oid| oid.to_string()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let label = if term == "skip" { "Bisect: skip".to_owned() } else { format!("Bisect: mark {term}") };
    GitCmd::new(label, &args).quiet()
}

/// Ends the bisect and checks out again what was checked out before it.
pub fn stop() -> GitCmd {
    GitCmd::new("Stop bisect", &["bisect", "reset"]).done("Bisect stopped")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RepoState;
    use crate::tests::{Fixture, git};

    /// Ten commits on main, `c1` to `c10`; returns their SHAs, oldest first.
    fn line_of_commits(fixture: &Fixture) -> Vec<Oid> {
        for i in 1..=10 {
            fixture.write("story file.txt", &format!("{}\n", "line ".repeat(i)));
            if i == 6 {
                fixture.write("bügs/ünï.txt", "the bug\n");
            }
            fixture.commit_all(&format!("c{i}"));
        }
        let shas = git(&fixture.dir, &["rev-list", "--reverse", "HEAD"]);
        shas.lines().map(|l| Oid::from_str(l).unwrap()).collect()
    }

    fn state(fixture: &Fixture) -> Option<State> {
        fixture.job(|loader| read(loader.repo())).unwrap()
    }

    fn head(fixture: &Fixture) -> Oid {
        Oid::from_str(&git(&fixture.dir, &["rev-parse", "HEAD"])).unwrap()
    }

    #[test]
    fn steps_follow_git() {
        // Git's own estimates (bisect.c) for a few sizes.
        assert_eq!([1, 2, 3, 4, 5, 8, 9, 16, 100, 1024].map(estimate_steps), [0, 0, 1, 1, 1, 2, 2, 3, 6, 9]);
    }

    #[test]
    fn quoted_paths_come_back() {
        assert_eq!(dequote(" '--' 'ünï.txt' 'a b.txt'\n"), ["--", "ünï.txt", "a b.txt"]);
        assert_eq!(dequote("'it'\\''s'"), ["it's"]);
        assert!(dequote("\n").is_empty());
    }

    #[test]
    fn a_bisect_from_start_to_the_first_bad_commit() {
        let fixture = Fixture::new("bisect-run");
        let commits = line_of_commits(&fixture);
        assert_eq!(state(&fixture), None, "no bisect yet");

        // Bad at the tip: git waits for a good commit.
        let done = fixture.run(start(commits[9], true));
        assert_eq!(done.snap.state, RepoState::Bisect);
        let now = state(&fixture).unwrap();
        assert_eq!(now.progress, Progress::Needs { bad: false, good: true });
        assert_eq!((now.bad, now.start.as_str()), (Some(commits[9]), "main"));

        // Good at the root: nine commits left, git checks one out.
        fixture.run(mark("good", Some(commits[0])));
        let now = state(&fixture).unwrap();
        assert_eq!(now.progress, Progress::Testing { left: 8, steps: 3 });
        assert_eq!(now.good, [commits[0]]);
        assert!(commits[1..9].contains(&head(&fixture)), "a commit between them is checked out");

        // Answer as if c6 brought the bug: bad from c6 on.
        let mut rounds = 0;
        while let Progress::Testing { .. } = state(&fixture).unwrap().progress {
            let at = head(&fixture);
            let index = commits.iter().position(|c| *c == at).unwrap();
            let term = if index >= 5 { "bad" } else { "good" };
            fixture.run(mark(term, None));
            rounds += 1;
            assert!(rounds < 10);
        }
        let now = state(&fixture).unwrap();
        assert_eq!(now.progress, Progress::Found(commits[5]));
        assert!(now.is_found(commits[5]) && now.bad == Some(commits[5]));
        assert!(rounds <= 4, "{rounds} rounds");

        let done = fixture.run(stop());
        assert_eq!(done.snap.state, RepoState::Clean);
        assert_eq!(done.snap.head.branch.as_deref(), Some("main"));
        assert_eq!(state(&fixture), None);
    }

    #[test]
    fn skipping_everything_left_says_so() {
        let fixture = Fixture::new("bisect-skip");
        let commits = line_of_commits(&fixture);
        fixture.run(start(commits[9], true));
        fixture.run(mark("good", Some(commits[7])));
        // Two commits left between them: c9 and the bad c10.
        assert_eq!(state(&fixture).unwrap().progress, Progress::Testing { left: 1, steps: 1 });
        // Git stops with an error then: nothing is left that it may test.
        let done = fixture.run_as(mark("skip", None), crate::worker::Origin::User);
        assert!(done.result.is_err());
        let now = state(&fixture).unwrap();
        assert_eq!(now.progress, Progress::OnlySkipped(vec![commits[8], commits[9]]));
        assert_eq!(now.skipped, [commits[8]]);
    }

    #[test]
    fn a_bisect_started_in_a_terminal_with_its_own_words_and_paths() {
        let fixture = Fixture::new("bisect-terminal");
        let commits = line_of_commits(&fixture);
        git(&fixture.dir, &["checkout", "-q", "--detach", "HEAD~1"]);
        git(&fixture.dir, &["bisect", "start", "--term-new=broken", "--term-old=fixed"]);
        let now = state(&fixture).unwrap();
        assert_eq!(now.terms, Terms { bad: "broken".to_owned(), good: "fixed".to_owned() });
        assert_eq!(now.progress, Progress::Needs { bad: true, good: true });
        assert_eq!(now.start_label(), crate::git::short(commits[8]), "started from a detached HEAD");

        fixture.run(mark("broken", Some(commits[9])));
        fixture.run(mark("fixed", Some(commits[0])));
        let now = state(&fixture).unwrap();
        assert_eq!((now.bad, now.good.clone()), (Some(commits[9]), vec![commits[0]]));
        assert!(matches!(now.progress, Progress::Testing { left: 8, .. }));
        git(&fixture.dir, &["bisect", "reset"]);

        // Limited to a path: only c6 touched it, so it is the one commit
        // left to test.
        git(&fixture.dir, &["bisect", "start", &commits[9].to_string(), &commits[0].to_string(), "--", "bügs/ünï.txt"]);
        assert_eq!(state(&fixture).unwrap().progress, Progress::Testing { left: 1, steps: 1 });
        assert_eq!(head(&fixture), commits[5]);
        fixture.run(mark("bad", None));
        assert_eq!(state(&fixture).unwrap().progress, Progress::Found(commits[5]));
    }

    #[test]
    fn git_refusing_a_mark_is_an_error() {
        let fixture = Fixture::new("bisect-refuse");
        let commits = line_of_commits(&fixture);
        fixture.run(start(commits[0], true));
        // A good commit that is not an ancestor of the bad one.
        let done = fixture.run_as(mark("good", Some(commits[9])), crate::worker::Origin::User);
        assert!(done.result.is_err());
    }
}
