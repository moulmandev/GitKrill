//! Cloning, with the options of the clone dialog (a branch, a shallow
//! history, one branch only, submodules), and listing a remote's branches
//! before there is a repository.

use std::path::{Path, PathBuf};

use super::cli;

/// How to clone; the default is a plain `git clone`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloneOptions {
    /// The branch to check out; `None` is the remote's default branch.
    pub branch: Option<String>,
    /// Only the last this many commits of history (`--depth`).
    pub depth: Option<u32>,
    /// Only the history of that one branch (`--single-branch`). Without it a
    /// shallow clone still fetches every branch, which git would not do on
    /// its own.
    pub single_branch: bool,
    /// Clone the submodules too (`--recurse-submodules`).
    pub submodules: bool,
}

/// The folder a clone of `url` gets: its last part without `.git`.
pub fn folder_name(url: &str) -> String {
    let name = url
        .trim()
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\', ':'])
        .next()
        .unwrap_or("")
        .trim_end_matches(".git")
        .to_owned();
    if name.is_empty() { "repository".to_owned() } else { name }
}

/// Why `url` cannot be cloned or listed, if so. One that starts with a dash
/// would be read by git as an option.
pub fn url_problem(url: &str) -> Option<&'static str> {
    let url = url.trim();
    if url.is_empty() {
        Some("Enter the address of the repository.")
    } else if url.starts_with('-') {
        Some("A repository address cannot start with a dash.")
    } else {
        None
    }
}

/// `git clone` with `options`, of `url` into `target`.
pub fn args(url: &str, target: &Path, options: &CloneOptions) -> Vec<String> {
    let mut args = vec!["clone".to_owned()];
    if let Some(branch) = &options.branch {
        args.extend(["--branch".to_owned(), branch.clone()]);
    }
    if let Some(depth) = options.depth {
        args.push(format!("--depth={depth}"));
    }
    if options.single_branch {
        args.push("--single-branch".to_owned());
    } else if options.depth.is_some() {
        args.push("--no-single-branch".to_owned());
    }
    if options.submodules {
        args.push("--recurse-submodules".to_owned());
    }
    args.extend(["--".to_owned(), url.trim().to_owned(), target.display().to_string()]);
    args
}

/// Clones `url` into a new folder in `dest` and returns that folder.
pub fn run(dest: &Path, url: &str, options: &CloneOptions) -> Result<PathBuf, String> {
    if let Some(problem) = url_problem(url) {
        return Err(problem.to_owned());
    }
    let target = dest.join(folder_name(url));
    let url = transport(dest, url.trim(), options);
    cli::run(dest, &args(&url, &target, options), cli::Options::default()).map(|_| target)
}

/// git makes a shallow clone only over a transport and ignores the depth
/// for a folder on this machine: that is given as a `file://` address then.
fn transport(dest: &Path, url: &str, options: &CloneOptions) -> String {
    let folder = dest.join(url);
    if options.depth.is_none() || url.contains("://") || !folder.is_dir() {
        return url.to_owned();
    }
    let path = folder.display().to_string().replace('\\', "/");
    format!("file://{}{path}", if path.starts_with('/') { "" } else { "/" })
}

/// A remote's branches, read before cloning it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Heads {
    /// The branch its HEAD points at, which a clone checks out.
    pub default: Option<String>,
    /// Every branch, in git's order (by name).
    pub branches: Vec<String>,
}

/// `git ls-remote --symref <url> HEAD refs/heads/*`, run in `dir`. It never
/// asks for a password: a remote that needs one answers with an error.
pub fn list_heads(dir: &Path, url: &str) -> Result<Heads, String> {
    if let Some(problem) = url_problem(url) {
        return Err(problem.to_owned());
    }
    let args = ["ls-remote", "--symref", url.trim(), "HEAD", "refs/heads/*"].map(str::to_owned);
    let out = cli::run(dir, &args, cli::Options { background: true, ..Default::default() })?;
    Ok(parse_heads(&out))
}

/// Reads [`list_heads`]' output.
pub fn parse_heads(out: &str) -> Heads {
    let mut heads = Heads::default();
    for line in out.lines() {
        let Some((left, name)) = line.split_once('\t') else { continue };
        if let Some(target) = left.strip_prefix("ref: ") {
            if name == "HEAD" {
                heads.default = target.strip_prefix("refs/heads/").map(str::to_owned);
            }
        } else if let Some(branch) = name.strip_prefix("refs/heads/") {
            heads.branches.push(branch.to_owned());
        }
    }
    heads
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};

    #[test]
    fn folder_names_come_from_the_url() {
        assert_eq!(folder_name("https://github.com/o/app.git"), "app");
        assert_eq!(folder_name("git@github.com:o/app.git"), "app");
        assert_eq!(folder_name("https://example.com/o/app/"), "app");
        assert_eq!(folder_name(r"C:\repos\my repo"), "my repo");
        assert_eq!(folder_name("host:"), "repository");
    }

    #[test]
    fn options_become_flags() {
        let target = Path::new("dest");
        let plain = args("https://x/app.git", target, &CloneOptions::default());
        assert_eq!(plain, ["clone", "--", "https://x/app.git", "dest"]);
        let all = CloneOptions { branch: Some("dev".into()), depth: Some(5), single_branch: true, submodules: true };
        assert_eq!(
            args("u", target, &all),
            ["clone", "--branch", "dev", "--depth=5", "--single-branch", "--recurse-submodules", "--", "u", "dest"]
        );
        // Shallow on its own would fetch only one branch: ask for all.
        let shallow = CloneOptions { depth: Some(1), ..Default::default() };
        assert!(args("u", target, &shallow).contains(&"--no-single-branch".to_owned()));
        assert_eq!(url_problem("--upload-pack=evil"), Some("A repository address cannot start with a dash."));
    }

    #[test]
    fn ls_remote_output_gives_the_default_and_every_branch() {
        let out = "ref: refs/heads/main\tHEAD\n0123\tHEAD\n0123\trefs/heads/feature/x\n0123\trefs/heads/main\n";
        assert_eq!(
            parse_heads(out),
            Heads { default: Some("main".into()), branches: vec!["feature/x".into(), "main".into()] }
        );
    }

    /// A repository with three commits on `main` and one more on
    /// `feature/ünï`, to clone from.
    fn source(name: &str) -> Fixture {
        let fx = Fixture::new(name);
        for n in 1..=3 {
            fx.write("a.txt", &format!("{n}\n"));
            fx.commit_all(&format!("commit {n}"));
        }
        git(&fx.dir, &["checkout", "-q", "-b", "feature/ünï"]);
        fx.write("b.txt", "feature\n");
        fx.commit_all("feature work");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx
    }

    fn fresh(dir: PathBuf) -> PathBuf {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_branches_of_a_remote_are_listed() {
        let fx = source("clone-heads");
        let heads = list_heads(&std::env::temp_dir(), &fx.dir.display().to_string()).expect("listed");
        assert_eq!(heads.default.as_deref(), Some("main"));
        assert_eq!(heads.branches, vec!["feature/ünï", "main"]);
        assert!(list_heads(&std::env::temp_dir(), &fx.dir.join("missing").display().to_string()).is_err());
    }

    #[test]
    fn a_shallow_single_branch_clone_of_another_branch() {
        let fx = source("clone-shallow");
        let dest = fresh(fx.dir.with_extension("dest"));
        // A local path would ignore --depth: git only makes shallow clones
        // over a transport.
        let url = format!("file:///{}", fx.dir.display().to_string().replace('\\', "/").trim_start_matches('/'));
        let options = CloneOptions { branch: Some("feature/ünï".into()), depth: Some(1), single_branch: true, submodules: false };
        let cloned = run(&dest, &url, &options).expect("cloned");
        assert_eq!(cloned.file_name().unwrap(), fx.dir.file_name().unwrap());
        assert_eq!(git(&cloned, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature/ünï");
        assert_eq!(git(&cloned, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(git(&cloned, &["branch", "-r"]), "origin/feature/ünï");
    }

    #[test]
    fn a_shallow_clone_keeps_every_branch_unless_asked() {
        let fx = source("clone-shallow-all");
        let dest = fresh(fx.dir.with_extension("dest"));
        // A folder, which git would clone in full without help.
        let folder = fx.dir.display().to_string();
        let cloned = run(&dest, &folder, &CloneOptions { depth: Some(2), ..Default::default() }).expect("cloned");
        assert_eq!(git(&cloned, &["rev-list", "--count", "HEAD"]), "2");
        assert!(git(&cloned, &["branch", "-r"]).contains("origin/feature/ünï"));
    }

    #[test]
    fn a_clone_into_a_folder_that_has_one_already_fails() {
        let fx = source("clone-exists");
        let dest = fresh(fx.dir.with_extension("dest"));
        let url = fx.dir.display().to_string();
        run(&dest, &url, &CloneOptions::default()).expect("first clone");
        assert!(run(&dest, &url, &CloneOptions::default()).unwrap_err().contains("already exists"));
    }
}
