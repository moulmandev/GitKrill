//! Git LFS (Preferences > LFS): whether it is installed, whether a
//! repository uses it, the patterns it tracks, and the commands that change
//! that. Everything here that runs `git lfs` blocks: call it in the
//! background. The commands are [`GitCmd`]s for the operator thread.

use std::path::Path;

use super::cli;
use super::ops::GitCmd;

/// `git lfs version`'s answer ("git-lfs/3.7.1 (GitHub; windows amd64; ...)"),
/// or why there is none: git-lfs is not installed.
pub fn version(dir: &Path) -> Result<String, String> {
    // Lets a scripted run show the page as it looks without git-lfs.
    if std::env::var_os("GITKRILL_SHOT").is_some() && std::env::var_os("GITKRILL_SHOT_NO_LFS").is_some() {
        return Err("git: 'lfs' is not a git command.".to_owned());
    }
    let out = cli::run(dir, &args(&["lfs", "version"]), cli::Options::default())?;
    Ok(out.lines().next().unwrap_or_default().trim().to_owned())
}

/// One pattern LFS tracks, as `git lfs track` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    /// Relative to the working tree root (`*.psd`, `art/*.png`), spaces
    /// written as `[[:space:]]` the way `.gitattributes` stores them.
    pub pattern: String,
    /// The `.gitattributes` it comes from, relative to the root, with `/`.
    pub source: String,
}

impl Pattern {
    /// The folder holding [`Pattern::source`], `""` for the root.
    pub fn folder(&self) -> &str {
        self.source.rsplit_once('/').map_or("", |(folder, _)| folder)
    }
}

/// Reads `git lfs track`'s listing:
///
/// ```text
/// Listing tracked patterns
///     *.psd (.gitattributes)
///     sub/*.zip (sub\.gitattributes)
/// Listing excluded patterns
///     *.txt (.gitattributes)
/// ```
///
/// Only the first section counts. Headings are told apart from patterns by
/// their indentation, not their words, which git-lfs translates.
pub fn parse_track(out: &str) -> Vec<Pattern> {
    let mut patterns = Vec::new();
    let mut sections = 0;
    for line in out.lines() {
        if !line.starts_with(char::is_whitespace) {
            if !line.trim().is_empty() {
                sections += 1;
            }
            continue;
        }
        if sections > 1 {
            break;
        }
        let line = line.trim();
        let Some((pattern, source)) = line.strip_suffix(')').and_then(|rest| rest.rsplit_once(" (")) else {
            continue;
        };
        patterns.push(Pattern { pattern: pattern.trim().to_owned(), source: source.replace('\\', "/") });
    }
    patterns
}

/// The tracked patterns of the repository at `workdir`. Only for one that
/// uses LFS: like most git-lfs commands, listing installs LFS's hooks and
/// writes `lfs.repositoryformatversion` to `.git/config` when they are not
/// there yet.
pub fn patterns(workdir: &Path) -> Result<Vec<Pattern>, String> {
    Ok(parse_track(&cli::run(workdir, &args(&["lfs", "track"]), cli::Options::default())?))
}

/// What the LFS page shows about one repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// `git lfs version`, or why git-lfs could not be run.
    pub version: Result<String, String>,
    /// LFS is set up here: its hooks, its filter in the repository's own
    /// configuration, or patterns in `.gitattributes`.
    pub initialized: bool,
    pub patterns: Vec<Pattern>,
    /// Listing the patterns failed.
    pub problem: Option<String>,
}

/// Looks at the repository at `workdir`. Blocks. Whether LFS is set up
/// is found without git-lfs, which would set itself up while answering
/// (see [`patterns`]); only then does git-lfs list the patterns.
pub fn status(workdir: &Path) -> Status {
    let version = version(workdir);
    let initialized =
        version.is_ok() && (has_local_filter(workdir) || has_hooks(workdir) || has_lfs_attributes(workdir));
    if !initialized {
        return Status { version, initialized, patterns: Vec::new(), problem: None };
    }
    let (patterns, problem) = match patterns(workdir) {
        Ok(patterns) => (patterns, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    Status { version, initialized, patterns, problem }
}

/// A `.gitattributes` in the working tree (committed or not) that sends
/// files through LFS: a cloned LFS repository.
fn has_lfs_attributes(workdir: &Path) -> bool {
    let list = args(&["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", ":(glob)**/.gitattributes"]);
    let listed = cli::run(workdir, &list, cli::Options::default()).unwrap_or_default();
    std::iter::once(".gitattributes")
        .chain(listed.split('\0').filter(|path| !path.is_empty()))
        .any(|path| std::fs::read(workdir.join(path)).is_ok_and(|bytes| uses_lfs(&String::from_utf8_lossy(&bytes))))
}

/// Whether a `.gitattributes` file sets `filter=lfs` on some pattern.
pub fn uses_lfs(attributes: &str) -> bool {
    attributes.lines().map(str::trim).filter(|line| !line.starts_with('#')).any(|line| {
        line.split_whitespace().skip(1).any(|attribute| attribute == "filter=lfs")
    })
}

/// `filter.lfs.*` in the repository's own configuration (what `git lfs
/// install --local` writes); the user's or the system's do not count.
fn has_local_filter(workdir: &Path) -> bool {
    cli::run(workdir, &args(&["config", "--local", "--get-regexp", r"^filter\.lfs\."]), cli::Options::default())
        .is_ok_and(|out| !out.trim().is_empty())
}

/// An LFS hook in the folder git runs hooks from (`core.hooksPath` or
/// `.git/hooks`).
fn has_hooks(workdir: &Path) -> bool {
    let Ok(out) = cli::run(workdir, &args(&["rev-parse", "--git-path", "hooks"]), cli::Options::default()) else {
        return false;
    };
    let hooks = workdir.join(out.trim());
    ["pre-push", "post-checkout", "post-commit", "post-merge"].iter().any(|name| {
        std::fs::read(hooks.join(name)).is_ok_and(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            text.contains("git lfs") || text.contains("git-lfs")
        })
    })
}

/// How many files in the checked-out commit are stored in LFS. Blocks; it
/// reads the whole tree, so it is asked for separately from [`status`].
/// Only for a repository that uses LFS (it writes to `.git/config`, see
/// [`patterns`]).
pub fn count_files(workdir: &Path) -> Result<usize, String> {
    let out = cli::run(workdir, &args(&["lfs", "ls-files"]), cli::Options::default())?;
    Ok(out.lines().filter(|line| !line.trim().is_empty()).count())
}

/// The pattern that tracks every file with the extension of `path`
/// (`art/cover.psd` gives `*.psd`); `None` without an extension.
pub fn extension_pattern(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let (stem, extension) = name.rsplit_once('.')?;
    (!stem.is_empty() && !extension.is_empty() && !extension.contains(char::is_whitespace))
        .then(|| format!("*.{extension}"))
}

// ---- commands ---------------------------------------------------------------

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Sets LFS up in this repository only: its filter in `.git/config`, its
/// hooks in the hooks folder.
pub fn install() -> GitCmd {
    GitCmd::new("Initialize LFS", &["lfs", "install", "--local"]).done("LFS initialized")
}

/// Adds `pattern` to the root `.gitattributes`.
pub fn track(pattern: &str) -> GitCmd {
    GitCmd::new(format!("Track {pattern} with LFS"), &["lfs", "track", pattern])
        .done(format!("Tracking {pattern} with LFS"))
}

/// Removes a pattern from the `.gitattributes` it is in. `git lfs untrack`
/// only edits the one in the folder it runs in, and takes the pattern as
/// written there.
pub fn untrack(pattern: &Pattern) -> GitCmd {
    let label = format!("Stop tracking {} with LFS", pattern.pattern);
    let folder = pattern.folder();
    if folder.is_empty() {
        return GitCmd::new(label, &["lfs", "untrack", &pattern.pattern]);
    }
    let prefix = format!("{folder}/");
    let local = pattern.pattern.strip_prefix(&prefix).unwrap_or(&pattern.pattern);
    GitCmd::new(label, &["-C", folder, "lfs", "untrack", local])
}

/// Downloads the LFS objects of the checked-out commit, without touching
/// the working tree.
pub fn fetch() -> GitCmd {
    GitCmd::new("Fetch LFS objects", &["lfs", "fetch"]).done("Fetched LFS objects")
}

/// Downloads them and puts the files in the working tree.
pub fn pull() -> GitCmd {
    GitCmd::new("Pull LFS objects", &["lfs", "pull"]).done("Pulled LFS objects")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};

    #[test]
    fn track_listing_parses_tracked_patterns_only() {
        let out = "Listing tracked patterns\n    sub/*.zip (sub\\.gitattributes)\n    *.psd (.gitattributes)\n    \
                   my[[:space:]]file (v2).bin (.gitattributes)\nListing excluded patterns\n    *.txt (.gitattributes)\n";
        let patterns = parse_track(out);
        assert_eq!(
            patterns,
            vec![
                Pattern { pattern: "sub/*.zip".to_owned(), source: "sub/.gitattributes".to_owned() },
                Pattern { pattern: "*.psd".to_owned(), source: ".gitattributes".to_owned() },
                Pattern { pattern: "my[[:space:]]file (v2).bin".to_owned(), source: ".gitattributes".to_owned() },
            ]
        );
        assert_eq!(patterns[0].folder(), "sub");
        assert_eq!(patterns[1].folder(), "");
        // Nothing tracked: git-lfs prints nothing at all, or headings only.
        assert_eq!(parse_track(""), Vec::new());
        assert_eq!(parse_track("Listing tracked patterns\nListing excluded patterns\n"), Vec::new());
        // Translated headings are skipped all the same.
        assert_eq!(parse_track("Motifs suivis\n    *.psd (.gitattributes)\n").len(), 1);
    }

    #[test]
    fn commands_name_the_pattern_and_the_folder_it_lives_in() {
        assert_eq!(track("*.psd").steps[0].args, ["lfs", "track", "*.psd"]);
        let root = Pattern { pattern: "*.psd".to_owned(), source: ".gitattributes".to_owned() };
        assert_eq!(untrack(&root).steps[0].args, ["lfs", "untrack", "*.psd"]);
        let nested = Pattern { pattern: "art/raw/*.zip".to_owned(), source: "art/raw/.gitattributes".to_owned() };
        assert_eq!(untrack(&nested).steps[0].args, ["-C", "art/raw", "lfs", "untrack", "*.zip"]);
        assert_eq!(install().steps[0].args, ["lfs", "install", "--local"]);
        assert_eq!(fetch().steps[0].args, ["lfs", "fetch"]);
        assert_eq!(pull().steps[0].args, ["lfs", "pull"]);
    }

    #[test]
    fn extensions_become_patterns() {
        assert_eq!(extension_pattern("art/cover.psd").as_deref(), Some("*.psd"));
        assert_eq!(extension_pattern("archive.tar.gz").as_deref(), Some("*.gz"));
        assert_eq!(extension_pattern("Makefile"), None);
        assert_eq!(extension_pattern(".gitignore"), None);
        assert_eq!(extension_pattern("dir.d/file"), None);
    }

    /// The tests below run git-lfs itself; without it they say so and pass.
    fn lfs_missing(dir: &Path) -> bool {
        match version(dir) {
            Ok(_) => false,
            Err(e) => {
                println!("skipped: git-lfs is not installed ({})", e.trim());
                true
            }
        }
    }

    #[test]
    fn a_repository_goes_from_uninitialized_to_tracking_and_back() {
        let fx = Fixture::new("lfs-track");
        if lfs_missing(&fx.dir) {
            return;
        }
        let before = status(&fx.dir);
        assert!(!before.initialized, "{before:?}");
        assert!(before.patterns.is_empty());
        // Looking changed nothing.
        assert!(!fx.dir.join(".git/hooks/pre-push").exists());
        assert!(!fx.read(".git/config").contains("lfs"));

        fx.run(install());
        assert!(status(&fx.dir).initialized);
        assert!(git(&fx.dir, &["config", "--local", "--get", "filter.lfs.process"]).contains("git-lfs"));

        fx.run(track("*.psd"));
        std::fs::create_dir_all(fx.dir.join("art")).unwrap();
        cli::run(&fx.dir.join("art"), &args(&["lfs", "track", "*.zip"]), cli::Options::default()).unwrap();
        let tracked = status(&fx.dir).patterns;
        assert_eq!(tracked.len(), 2, "{tracked:?}");
        assert!(fx.read(".gitattributes").contains("*.psd filter=lfs"));

        for pattern in &tracked {
            fx.run(untrack(pattern));
        }
        assert_eq!(status(&fx.dir).patterns, Vec::new());
        assert!(!fx.read(".gitattributes").contains("*.psd"));
        assert!(!fx.read("art/.gitattributes").contains("*.zip"));
    }

    #[test]
    fn attributes_tell_a_cloned_lfs_repository() {
        assert!(uses_lfs("*.psd filter=lfs diff=lfs merge=lfs -text
"));
        assert!(uses_lfs("# art
	*.png  filter=lfs -text
"));
        assert!(!uses_lfs("# *.psd filter=lfs
*.txt -filter=lfs
*.rs diff=rust
"));

        let fx = Fixture::new("lfs-attributes");
        if lfs_missing(&fx.dir) {
            return;
        }
        fx.write("art/.gitattributes", "*.psd filter=lfs diff=lfs merge=lfs -text
");
        let status = status(&fx.dir);
        assert!(status.initialized, "{status:?}");
        assert_eq!(status.patterns.len(), 1, "{status:?}");
        assert_eq!(status.patterns[0].folder(), "art");
    }

    #[test]
    fn lfs_files_are_counted() {
        let fx = Fixture::new("lfs-count");
        if lfs_missing(&fx.dir) {
            return;
        }
        fx.run(install());
        fx.run(track("*.bin"));
        fx.write("a.bin", "one");
        fx.write("b.bin", "two");
        fx.write("c.txt", "three");
        fx.commit_all("add files");
        assert_eq!(count_files(&fx.dir), Ok(2));
    }
}
