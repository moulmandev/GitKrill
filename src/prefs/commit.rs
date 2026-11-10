//! Preferences > Commit (repository-specific): what the commit button does
//! by default, and the commit message template.
//!
//! The template is git's own `commit.template`, so the terminal and other
//! tools use the same one: the effective value (the repository's, else the
//! user's) is read, and saving writes that file when it lives inside the
//! repository's `.git` folder. Otherwise the template goes to
//! `.git/gitkrill-commit-template` and the repository's `commit.template`
//! is pointed at it. Nothing is ever written to the working tree.
//!
//! Shown by `crate::ui::prefs::commit`; the commit panel applies it
//! (`crate::ui::commit_button`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::git::cli;

/// What the commit panel's main button does.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CommitAction {
    #[default]
    Commit,
    /// Push the branch once the commit is made.
    CommitAndPush,
    /// Open the "Create pull request" dialog once the commit is made.
    CommitAndPullRequest,
}

impl CommitAction {
    pub const ALL: [CommitAction; 3] =
        [CommitAction::Commit, CommitAction::CommitAndPush, CommitAction::CommitAndPullRequest];

    /// `pull` is the hosting service's word ("pull request", "merge request").
    pub fn title(self, pull: &str) -> String {
        match self {
            CommitAction::Commit => "Commit".to_owned(),
            CommitAction::CommitAndPush => "Commit and push".to_owned(),
            CommitAction::CommitAndPullRequest => format!("Commit and create {pull}"),
        }
    }
}

/// Held by `RepoPrefs::commit`, for one repository.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct RepoCommit {
    /// What the commit panel's main button does; the others are in its menu.
    pub action: CommitAction,
    /// Fill an empty commit message from `commit.template`, as git does.
    pub apply_template: bool,
    /// Commit with `--cleanup=strip`: lines starting with git's comment
    /// character (`#`, or `core.commentChar`) are left out of the message.
    pub strip_comments: bool,
}

impl Default for RepoCommit {
    fn default() -> Self {
        // Git itself starts every new message from the template.
        RepoCommit { action: CommitAction::Commit, apply_template: true, strip_comments: false }
    }
}

/// Recommended longest summary line; the counters count down from it.
pub const SUMMARY_LIMIT: usize = 72;

/// What our own template file is called, inside the `.git` folder.
pub const OWN_FILE: &str = "gitkrill-commit-template";

/// A commit message template, split the way the commit panel shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Template {
    pub summary: String,
    pub description: String,
}

impl Template {
    /// Splits a message: its first line is the summary, the rest (after
    /// the blank line that usually follows) the description. Spaces at the
    /// end of the summary stay ("feat: " leaves room to type).
    pub fn parse(text: &str) -> Self {
        let text = text.replace("\r\n", "\n");
        let (summary, rest) = text.split_once('\n').unwrap_or((&text, ""));
        Template {
            summary: summary.to_owned(),
            description: rest.trim_start_matches('\n').trim_end().to_owned(),
        }
    }

    /// The file content: summary, a blank line, description.
    pub fn to_text(&self) -> String {
        let summary = self.summary.as_str();
        let description = self.description.trim_end();
        match (summary.trim().is_empty(), description.is_empty()) {
            (true, true) => String::new(),
            (_, true) => format!("{summary}\n"),
            _ => format!("{summary}\n\n{description}\n"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.summary.trim().is_empty() && self.description.trim().is_empty()
    }
}

/// The repository's template as git sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemplateFile {
    /// Where `commit.template` points, made absolute; `None` when unset.
    pub path: Option<PathBuf>,
    pub template: Template,
    /// Why the file could not be read (it may have been moved).
    pub problem: Option<String>,
}

/// Where git looks for its configuration in [`read_template`] and
/// [`save_template`]. `Default` is the real one; tests name files.
#[derive(Clone, Debug, Default)]
pub struct ConfigEnv {
    /// Stands in for the user's global file (`GIT_CONFIG_GLOBAL`), and turns
    /// off the system one, so a test never sees the machine's settings.
    pub global: Option<PathBuf>,
}

impl ConfigEnv {
    fn vars(&self) -> Vec<(String, String)> {
        match &self.global {
            Some(file) => vec![
                ("GIT_CONFIG_GLOBAL".to_owned(), file.display().to_string()),
                ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ],
            None => Vec::new(),
        }
    }
}

/// The effective `commit.template` of the repository at `workdir`: its own
/// value first, then the user's. Relative paths are relative to the working
/// tree, where git runs `commit` from. Blocks: call it in the background.
pub fn template_path(workdir: &Path, env: &ConfigEnv) -> Result<Option<PathBuf>, String> {
    let args: Vec<String> = ["config", "--path", "--get", "commit.template"].map(str::to_owned).to_vec();
    let vars = env.vars();
    match cli::run(workdir, &args, cli::Options { env: &vars, ..Default::default() }) {
        Ok(out) => {
            let value = out.trim_end_matches(['\r', '\n']);
            if value.trim().is_empty() {
                return Ok(None);
            }
            let path = PathBuf::from(value);
            Ok(Some(if path.is_relative() { workdir.join(path) } else { path }))
        }
        // `git config --get` exits with 1, printing nothing, when the key is
        // not set ("exit code: 1" on Windows, "exit status: 1" elsewhere).
        Err(e) if e.ends_with("code: 1)") || e.ends_with("status: 1)") => Ok(None),
        Err(e) => Err(e),
    }
}

/// Reads the template git would start a commit message from. Blocks.
pub fn read_template(workdir: &Path, env: &ConfigEnv) -> TemplateFile {
    let path = match template_path(workdir, env) {
        Ok(path) => path,
        Err(e) => return TemplateFile { problem: Some(e.trim().to_owned()), ..Default::default() },
    };
    let Some(path) = path else { return TemplateFile::default() };
    match std::fs::read(&path) {
        Ok(bytes) => TemplateFile {
            template: Template::parse(&String::from_utf8_lossy(&bytes)),
            path: Some(path),
            problem: None,
        },
        Err(e) => TemplateFile {
            problem: Some(format!("commit.template points to {}, which cannot be read ({e}).", path.display())),
            path: Some(path),
            template: Template::default(),
        },
    }
}

/// Whether `path` is inside `dir` once both are made absolute and links
/// resolved (a file that does not exist yet is judged by its folder).
pub fn is_inside(path: &Path, dir: &Path) -> bool {
    fn canonical(path: &Path) -> Option<PathBuf> {
        path.canonicalize().ok().or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)))
    }
    match (canonical(path), canonical(dir)) {
        (Some(path), Some(dir)) => path.starts_with(dir),
        _ => false,
    }
}

/// Saves `template` for the repository at `workdir`, whose shared git folder
/// is `common_dir` (`.git`, also for linked worktrees). `current` is where
/// `commit.template` points now. Returns the template as read back. Blocks.
pub fn save_template(
    workdir: &Path,
    common_dir: &Path,
    current: Option<&Path>,
    template: &Template,
    env: &ConfigEnv,
) -> Result<TemplateFile, String> {
    let text = template.to_text();
    match current.filter(|path| is_inside(path, common_dir)) {
        Some(path) => {
            std::fs::write(path, text).map_err(|e| format!("Could not write {}: {e}", path.display()))?;
        }
        None => {
            let path = common_dir.join(OWN_FILE);
            std::fs::write(&path, text).map_err(|e| format!("Could not write {}: {e}", path.display()))?;
            // Forward slashes read the same to git on every system and need
            // no escaping in the config file.
            let value = path.display().to_string().replace('\\', "/");
            let args: Vec<String> =
                ["config", "--local", "commit.template", value.as_str()].map(str::to_owned).to_vec();
            let vars = env.vars();
            cli::run(workdir, &args, cli::Options { env: &vars, ..Default::default() })
                .map_err(|e| format!("Could not set commit.template: {}", e.trim()))?;
        }
    }
    Ok(read_template(workdir, env))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::ops;
    use crate::tests::{Fixture, git};

    /// A global configuration file of the test's own, so the machine's
    /// `~/.gitconfig` never leaks in.
    fn env(fx: &Fixture, global: &str) -> ConfigEnv {
        let file = fx.dir.join(".git").join("test-global-config");
        std::fs::write(&file, global).unwrap();
        ConfigEnv { global: Some(file) }
    }

    fn common_dir(fx: &Fixture) -> PathBuf {
        git2::Repository::open(&fx.dir).unwrap().commondir().to_path_buf()
    }

    #[test]
    fn templates_split_into_summary_and_description_and_back() {
        let t = Template::parse("feat: \r\n\r\nWhy:\r\n# say why\r\n");
        assert_eq!(t, Template { summary: "feat: ".to_owned(), description: "Why:\n# say why".to_owned() });
        assert_eq!(t.to_text(), "feat: \n\nWhy:\n# say why\n");
        assert_eq!(Template::parse(&t.to_text()), t);

        // A template that leaves the summary empty, as many do.
        let t = Template::parse("\n# Summary in the imperative\n# Body: what and why\n");
        assert_eq!(t.summary, "");
        assert_eq!(t.description, "# Summary in the imperative\n# Body: what and why");
        assert_eq!(Template::parse(&t.to_text()), t);

        assert_eq!(Template::parse("only a line").to_text(), "only a line\n");
        assert!(Template::default().is_empty());
        assert_eq!(Template::default().to_text(), "");
    }

    #[test]
    fn no_template_reads_as_empty() {
        let fx = Fixture::new("template-none");
        let env = env(&fx, "");
        assert_eq!(read_template(&fx.dir, &env), TemplateFile::default());
    }

    #[test]
    fn the_global_template_is_read_and_saving_moves_the_repository_to_its_own() {
        let fx = Fixture::new("template-global");
        let outside = fx.dir.with_extension("global-template.txt");
        std::fs::write(&outside, "Global summary\n\nGlobal body\n").unwrap();
        let env = env(&fx, &format!("[commit]\n\ttemplate = {}\n", outside.display().to_string().replace('\\', "/")));

        let read = read_template(&fx.dir, &env);
        assert_eq!(read.template.summary, "Global summary");
        assert_eq!(read.template.description, "Global body");
        assert!(!is_inside(read.path.as_deref().unwrap(), &common_dir(&fx)));

        let new = Template { summary: "Repo summary".to_owned(), description: "Repo body".to_owned() };
        let saved = save_template(&fx.dir, &common_dir(&fx), read.path.as_deref(), &new, &env).unwrap();
        assert_eq!(saved.template, new);
        let own = common_dir(&fx).join(OWN_FILE);
        assert!(is_inside(saved.path.as_deref().unwrap(), &common_dir(&fx)));
        assert_eq!(std::fs::read_to_string(&own).unwrap(), "Repo summary\n\nRepo body\n");
        // The user's file is untouched; the repository points at its own.
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "Global summary\n\nGlobal body\n");
        let local = git(&fx.dir, &["config", "--local", "--get", "commit.template"]);
        assert!(local.ends_with(&format!(".git/{OWN_FILE}")), "{local}");
        // Nothing appeared in the working tree.
        assert!(fx.snapshot().status.is_clean());
        let _ = std::fs::remove_file(outside);
    }

    #[test]
    fn a_template_inside_the_git_folder_is_written_in_place() {
        let fx = Fixture::new("template-inside");
        let env = env(&fx, "");
        std::fs::write(fx.dir.join(".git/my-template"), "Old\n").unwrap();
        // Relative to the working tree, as git reads it.
        git(&fx.dir, &["config", "commit.template", ".git/my-template"]);

        let read = read_template(&fx.dir, &env);
        assert_eq!(read.template.summary, "Old");
        let new = Template { summary: "New".to_owned(), description: String::new() };
        let saved = save_template(&fx.dir, &common_dir(&fx), read.path.as_deref(), &new, &env).unwrap();
        assert_eq!(saved.template, new);
        assert_eq!(fx.read(".git/my-template"), "New\n");
        assert!(!fx.dir.join(".git").join(OWN_FILE).exists());
        assert_eq!(git(&fx.dir, &["config", "--local", "--get", "commit.template"]), ".git/my-template");
    }

    #[test]
    fn a_template_in_the_working_tree_is_never_written() {
        let fx = Fixture::new("template-worktree");
        let env = env(&fx, "");
        fx.write(".gitmessage", "Team summary\n");
        fx.commit_all("add the team template");
        git(&fx.dir, &["config", "commit.template", ".gitmessage"]);

        let read = read_template(&fx.dir, &env);
        assert_eq!(read.template.summary, "Team summary");
        let new = Template { summary: "Mine".to_owned(), description: String::new() };
        save_template(&fx.dir, &common_dir(&fx), read.path.as_deref(), &new, &env).unwrap();
        assert_eq!(fx.read(".gitmessage"), "Team summary\n");
        assert_eq!(read_template(&fx.dir, &env).template, new);
        assert!(fx.snapshot().status.is_clean());
    }

    #[test]
    fn a_missing_template_file_is_reported_and_saving_replaces_it() {
        let fx = Fixture::new("template-missing");
        let env = env(&fx, "");
        git(&fx.dir, &["config", "commit.template", "C:/nowhere/gitkrill-missing-template"]);
        let read = read_template(&fx.dir, &env);
        assert!(read.problem.is_some());
        assert!(read.template.is_empty());
        let new = Template { summary: "Back".to_owned(), description: String::new() };
        let saved = save_template(&fx.dir, &common_dir(&fx), read.path.as_deref(), &new, &env).unwrap();
        assert_eq!(saved.problem, None);
        assert_eq!(saved.template, new);
    }

    #[test]
    fn stripping_comments_leaves_hash_lines_out_of_the_commit() {
        let fx = Fixture::new("template-strip");
        fx.write("a.txt", "a\n");
        git(&fx.dir, &["add", "-A"]);
        let message = "Add a\n\n# Explain why\nBecause.\n#123 starts with the comment character too";
        let done = fx.run(ops::commit(message, false).strip_comments());
        assert!(done.result.is_ok(), "{:?}", done.result);
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%B"]), "Add a\n\nBecause.");

        // Without it, git keeps every line of a message given on stdin.
        fx.write("b.txt", "b\n");
        let done = fx.run(ops::commit_everything("Add b\n\n# kept"));
        assert!(done.result.is_ok(), "{:?}", done.result);
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%B"]), "Add b\n\n# kept");
    }
}
