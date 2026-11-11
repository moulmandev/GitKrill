//! Hands files and repositories to other programs: the merge tool, diff
//! tool, editor and terminal chosen in Preferences > External Tools
//! ([`crate::prefs::tools`]).
//!
//! - [`detect`] finds the tools installed on this computer, on PATH and in
//!   the usual install folders. The preferences page runs it on a
//!   background thread when it opens. Nothing looks for tools at start or
//!   when one is started: the path of the chosen one is saved with it.
//! - [`merge`] builds `git mergetool` as a [`GitCmd`] that runs like any
//!   other command on the operator thread. It waits while the tool is open,
//!   and the refresh afterwards shows the file resolved if the tool saved it.
//! - [`diff`], [`edit`] and [`terminal`] start their program on a
//!   short-lived thread and do not wait for it. `git difftool` is waited for
//!   on that thread, so that a failure can be reported. A program that
//!   fails comes back as an error toast through [`poll`].
//! - Under a scripted screenshot run (`GITKRILL_SHOT`) nothing is started:
//!   the command line is printed instead, so runs never open windows.
//!
//! Tools git knows (`git mergetool --tool-help`) run under git's own name,
//! with the program found here (`-c mergetool.<tool>.path=...`). For the
//! others the command line comes from here
//! (`-c mergetool.gitkrill-<id>.cmd=...`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use eframe::egui;

use crate::git::cli;
use crate::git::ops::{GitCmd, Step};
use crate::prefs::gitconfig;
use crate::prefs::tools::{DiffTool, Editor, MergeTool, Program, Terminal, Tools};
use crate::state::{Shared, ToastKind};

// ---- the tools GitKrill knows ---------------------------------------------

/// A tool GitKrill knows how to find and start.
pub struct Known {
    /// Saved in the settings; never change one.
    pub id: &'static str,
    pub name: &'static str,
    /// Resolves conflicts and compares two files.
    pub compare: bool,
    pub editor: bool,
    pub terminal: bool,
    /// git's own name for it, when git knows it.
    git: Option<&'static str>,
    /// For a tool git does not know: its commands for git's shell, `{exe}`
    /// standing for the program.
    merge_cmd: &'static str,
    diff_cmd: &'static str,
    /// Whether its exit status says if the merge succeeded. When it does
    /// not, git looks at whether the file was saved instead.
    trust_exit_code: bool,
    /// When the program found is the application itself (`Code.exe`), the
    /// launcher git should run instead, from the program's folder: the
    /// application returns at once, the launcher can wait for the window.
    cli: Option<&'static str>,
    /// The command line that opens a file in the editor or a terminal in a
    /// folder: `{exe}` is the program, `{path}` the file or folder.
    run: &'static [&'static str],
    /// The same for a folder, when it differs.
    run_folder: &'static [&'static str],
    /// A Windows console program, which needs a console window of its own.
    console: bool,
    /// Where to look, in order. `path:name` is a program on PATH; `{pf}`
    /// the Program Files folders, `{programs}` `%LOCALAPPDATA%\Programs`,
    /// `{local}` `%LOCALAPPDATA%`, `{system}` `System32`, `{home}` the home
    /// folder. A `*` in one part of a path matches any name there (the last
    /// in sort order wins, which is usually the newest version).
    windows: &'static [&'static str],
    macos: &'static [&'static str],
    linux: &'static [&'static str],
}

const DEFAULT: Known = Known {
    id: "",
    name: "",
    compare: false,
    editor: false,
    terminal: false,
    git: None,
    merge_cmd: "",
    diff_cmd: "",
    trust_exit_code: true,
    cli: None,
    run: &["{exe}", "{path}"],
    run_folder: &[],
    console: false,
    windows: &[],
    macos: &[],
    linux: &[],
};

/// An IDE of the IntelliJ family: `idea64.exe merge` and `idea64.exe diff`.
/// Their launchers' exit status is not documented as meaningful, so git
/// checks whether the file was saved.
macro_rules! jetbrains {
    ($id:literal, $name:literal, $folder:literal, $exe:literal, $app:literal, $bin:literal) => {
        Known {
            id: $id,
            name: $name,
            compare: true,
            editor: true,
            merge_cmd: "{exe} merge \"$LOCAL\" \"$REMOTE\" \"$BASE\" \"$MERGED\"",
            diff_cmd: "{exe} diff \"$LOCAL\" \"$REMOTE\"",
            trust_exit_code: false,
            windows: &[
                concat!("{pf}/JetBrains/", $folder, "*/bin/", $exe),
                concat!("{programs}/", $folder, "*/bin/", $exe),
                concat!("{local}/JetBrains/Toolbox/scripts/", $bin, ".cmd"),
            ],
            macos: &[
                concat!("/Applications/", $app, "*.app/Contents/MacOS/", $bin),
                concat!("{home}/Applications/", $app, "*.app/Contents/MacOS/", $bin),
                concat!("path:", $bin),
            ],
            linux: &[
                concat!("path:", $bin),
                concat!("{home}/.local/share/JetBrains/Toolbox/scripts/", $bin),
                concat!("path:", $bin, ".sh"),
            ],
            ..DEFAULT
        }
    };
}

static KNOWN: &[Known] = &[
    // Merge and diff tools (the editors among them also open files).
    Known {
        id: "vscode",
        name: "Visual Studio Code",
        compare: true,
        editor: true,
        git: Some("vscode"),
        cli: Some("bin/code"),
        windows: &["{programs}/Microsoft VS Code/Code.exe", "{pf}/Microsoft VS Code/Code.exe", "path:code"],
        macos: &[
            "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
            "{home}/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
            "path:code",
        ],
        linux: &["path:code", "/usr/share/code/bin/code", "/snap/bin/code"],
        ..DEFAULT
    },
    Known {
        id: "vscode-insiders",
        name: "Visual Studio Code Insiders",
        compare: true,
        editor: true,
        git: Some("vscode"),
        cli: Some("bin/code-insiders"),
        windows: &[
            "{programs}/Microsoft VS Code Insiders/Code - Insiders.exe",
            "{pf}/Microsoft VS Code Insiders/Code - Insiders.exe",
            "path:code-insiders",
        ],
        macos: &["/Applications/Visual Studio Code - Insiders.app/Contents/Resources/app/bin/code", "path:code-insiders"],
        linux: &["path:code-insiders", "/usr/share/code-insiders/bin/code-insiders", "/snap/bin/code-insiders"],
        ..DEFAULT
    },
    Known {
        id: "vscodium",
        name: "VSCodium",
        compare: true,
        editor: true,
        git: Some("vscode"),
        cli: Some("bin/codium"),
        windows: &["{programs}/VSCodium/VSCodium.exe", "{pf}/VSCodium/VSCodium.exe", "path:codium"],
        macos: &["/Applications/VSCodium.app/Contents/Resources/app/bin/codium", "path:codium"],
        linux: &["path:codium", "/usr/share/codium/bin/codium", "/snap/bin/codium"],
        ..DEFAULT
    },
    Known {
        id: "bc",
        name: "Beyond Compare",
        compare: true,
        git: Some("bc"),
        windows: &[
            "{pf}/Beyond Compare 5/BComp.exe",
            "{pf}/Beyond Compare 4/BComp.exe",
            "{pf}/Beyond Compare 3/BComp.exe",
            "{programs}/Beyond Compare 5/BComp.exe",
            "{programs}/Beyond Compare 4/BComp.exe",
            "path:bcomp",
        ],
        macos: &["/Applications/Beyond Compare.app/Contents/MacOS/bcomp", "path:bcomp"],
        linux: &["path:bcompare", "path:bcomp"],
        ..DEFAULT
    },
    Known {
        id: "kdiff3",
        name: "KDiff3",
        compare: true,
        git: Some("kdiff3"),
        windows: &["{pf}/KDiff3/kdiff3.exe", "{pf}/KDiff3/bin/kdiff3.exe", "path:kdiff3"],
        macos: &["/Applications/kdiff3.app/Contents/MacOS/kdiff3", "path:kdiff3"],
        linux: &["path:kdiff3"],
        ..DEFAULT
    },
    Known {
        id: "meld",
        name: "Meld",
        compare: true,
        git: Some("meld"),
        windows: &["{pf}/Meld/Meld.exe", "{pf}/Meld/meld/meld.exe", "path:meld"],
        macos: &["/Applications/Meld.app/Contents/MacOS/Meld", "path:meld"],
        linux: &["path:meld"],
        ..DEFAULT
    },
    Known {
        id: "p4merge",
        name: "P4Merge",
        compare: true,
        git: Some("p4merge"),
        windows: &["{pf}/Perforce/p4merge.exe", "path:p4merge"],
        macos: &["/Applications/p4merge.app/Contents/Resources/launchp4merge", "path:p4merge"],
        linux: &["path:p4merge"],
        ..DEFAULT
    },
    Known {
        id: "winmerge",
        name: "WinMerge",
        compare: true,
        git: Some("winmerge"),
        windows: &["{pf}/WinMerge/WinMergeU.exe", "{programs}/WinMerge/WinMergeU.exe", "path:WinMergeU"],
        ..DEFAULT
    },
    Known {
        id: "smerge",
        name: "Sublime Merge",
        compare: true,
        git: Some("smerge"),
        windows: &["{pf}/Sublime Merge/smerge.exe", "path:smerge"],
        macos: &["/Applications/Sublime Merge.app/Contents/SharedSupport/bin/smerge", "path:smerge"],
        linux: &["path:smerge", "/opt/sublime_merge/sublime_merge"],
        ..DEFAULT
    },
    Known {
        id: "tortoisemerge",
        name: "TortoiseGitMerge",
        compare: true,
        git: Some("tortoisemerge"),
        windows: &["{pf}/TortoiseGit/bin/TortoiseGitMerge.exe", "{pf}/TortoiseSVN/bin/TortoiseMerge.exe"],
        ..DEFAULT
    },
    Known {
        id: "opendiff",
        name: "FileMerge",
        compare: true,
        git: Some("opendiff"),
        macos: &["/Applications/Xcode.app/Contents/Developer/usr/bin/opendiff", "path:opendiff"],
        ..DEFAULT
    },
    Known {
        id: "gvimdiff",
        name: "gVim",
        compare: true,
        git: Some("gvimdiff"),
        windows: &["{pf}/Vim/vim*/gvim.exe", "path:gvim"],
        macos: &["path:mvim", "/Applications/MacVim.app/Contents/bin/mvim"],
        linux: &["path:gvim"],
        ..DEFAULT
    },
    jetbrains!("idea", "IntelliJ IDEA", "IntelliJ IDEA", "idea64.exe", "IntelliJ IDEA", "idea"),
    jetbrains!("pycharm", "PyCharm", "PyCharm", "pycharm64.exe", "PyCharm", "pycharm"),
    jetbrains!("webstorm", "WebStorm", "WebStorm", "webstorm64.exe", "WebStorm", "webstorm"),
    jetbrains!("phpstorm", "PhpStorm", "PhpStorm", "phpstorm64.exe", "PhpStorm", "phpstorm"),
    jetbrains!("rider", "Rider", "JetBrains Rider", "rider64.exe", "Rider", "rider"),
    jetbrains!("clion", "CLion", "CLion", "clion64.exe", "CLion", "clion"),
    jetbrains!("goland", "GoLand", "GoLand", "goland64.exe", "GoLand", "goland"),
    jetbrains!("rustrover", "RustRover", "RustRover", "rustrover64.exe", "RustRover", "rustrover"),
    jetbrains!("android-studio", "Android Studio", "Android Studio", "studio64.exe", "Android Studio", "studio"),
    // Editors.
    Known {
        id: "sublime-text",
        name: "Sublime Text",
        editor: true,
        windows: &["{pf}/Sublime Text/subl.exe", "{pf}/Sublime Text 3/subl.exe", "path:subl"],
        macos: &["/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl", "path:subl"],
        linux: &["path:subl", "/opt/sublime_text/sublime_text"],
        ..DEFAULT
    },
    Known {
        id: "notepad++",
        name: "Notepad++",
        editor: true,
        run_folder: &["{exe}", "-openFoldersAsWorkspace", "{path}"],
        windows: &["{pf}/Notepad++/notepad++.exe", "path:notepad++"],
        ..DEFAULT
    },
    Known {
        id: "zed",
        name: "Zed",
        editor: true,
        windows: &["{programs}/Zed/Zed.exe", "path:zed"],
        macos: &["/Applications/Zed.app/Contents/MacOS/cli", "path:zed"],
        linux: &["path:zed", "{home}/.local/bin/zed", "path:zeditor"],
        ..DEFAULT
    },
    Known {
        id: "textmate",
        name: "TextMate",
        editor: true,
        macos: &["/Applications/TextMate.app/Contents/MacOS/mate", "path:mate"],
        ..DEFAULT
    },
    Known {
        id: "bbedit",
        name: "BBEdit",
        editor: true,
        macos: &["/Applications/BBEdit.app/Contents/Helpers/bbedit_tool", "path:bbedit"],
        ..DEFAULT
    },
    // Terminals. Most start in the folder they are started in; the others
    // are told it.
    Known {
        id: "windows-terminal",
        name: "Windows Terminal",
        terminal: true,
        run: &["{exe}", "-d", "{path}"],
        windows: &["{local}/Microsoft/WindowsApps/wt.exe", "path:wt"],
        ..DEFAULT
    },
    Known {
        id: "pwsh",
        name: "PowerShell 7",
        terminal: true,
        run: &["{exe}", "-NoLogo"],
        console: true,
        windows: &["{pf}/PowerShell/7/pwsh.exe", "path:pwsh"],
        ..DEFAULT
    },
    Known {
        id: "powershell",
        name: "Windows PowerShell",
        terminal: true,
        run: &["{exe}", "-NoLogo"],
        console: true,
        windows: &["{system}/WindowsPowerShell/v1.0/powershell.exe", "path:powershell"],
        ..DEFAULT
    },
    Known {
        id: "cmd",
        name: "Command Prompt",
        terminal: true,
        run: &["{exe}"],
        console: true,
        windows: &["{system}/cmd.exe"],
        ..DEFAULT
    },
    Known {
        id: "git-bash",
        name: "Git Bash",
        terminal: true,
        run: &["{exe}", "--cd={path}"],
        windows: &["{pf}/Git/git-bash.exe", "{programs}/Git/git-bash.exe", "git-bash"],
        ..DEFAULT
    },
    Known {
        id: "terminal-app",
        name: "Terminal",
        terminal: true,
        run: &["open", "-a", "{exe}", "{path}"],
        macos: &["/System/Applications/Utilities/Terminal.app", "/Applications/Utilities/Terminal.app"],
        ..DEFAULT
    },
    Known {
        id: "iterm",
        name: "iTerm",
        terminal: true,
        run: &["open", "-a", "{exe}", "{path}"],
        macos: &["/Applications/iTerm.app", "{home}/Applications/iTerm.app"],
        ..DEFAULT
    },
    Known {
        id: "gnome-terminal",
        name: "GNOME Terminal",
        terminal: true,
        run: &["{exe}", "--working-directory={path}"],
        linux: &["path:gnome-terminal"],
        ..DEFAULT
    },
    Known {
        id: "konsole",
        name: "Konsole",
        terminal: true,
        run: &["{exe}", "--workdir", "{path}"],
        linux: &["path:konsole"],
        ..DEFAULT
    },
    Known {
        id: "xfce4-terminal",
        name: "Xfce Terminal",
        terminal: true,
        run: &["{exe}", "--working-directory={path}"],
        linux: &["path:xfce4-terminal"],
        ..DEFAULT
    },
    Known {
        id: "alacritty",
        name: "Alacritty",
        terminal: true,
        run: &["{exe}", "--working-directory", "{path}"],
        linux: &["path:alacritty"],
        ..DEFAULT
    },
    Known {
        id: "kitty",
        name: "kitty",
        terminal: true,
        run: &["{exe}", "--directory", "{path}"],
        linux: &["path:kitty"],
        ..DEFAULT
    },
    Known {
        id: "wezterm",
        name: "WezTerm",
        terminal: true,
        run: &["{exe}", "start", "--cwd", "{path}"],
        linux: &["path:wezterm"],
        ..DEFAULT
    },
    Known { id: "xterm", name: "XTerm", terminal: true, run: &["{exe}"], linux: &["path:xterm"], ..DEFAULT },
];

// Ids are unique, so a tool is its id.
impl PartialEq for Known {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl std::fmt::Debug for Known {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id)
    }
}

/// The tool saved as `id`, if this version knows it.
pub fn known(id: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|known| known.id == id)
}

impl Known {
    /// Where to look on this operating system.
    fn places(&self) -> &'static [&'static str] {
        if cfg!(windows) {
            self.windows
        } else if cfg!(target_os = "macos") {
            self.macos
        } else {
            self.linux
        }
    }
}

// ---- finding them ----------------------------------------------------------

/// A known tool found on this computer.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub id: &'static str,
    pub name: &'static str,
    pub path: PathBuf,
}

impl Found {
    /// What the settings keep when it is chosen.
    pub fn program(&self) -> Program {
        Program { id: self.id.to_owned(), name: self.name.to_owned(), path: self.path.clone() }
    }
}

/// What [`detect`] found.
#[derive(Default, Debug)]
pub struct Detected {
    /// Merge and diff tools.
    pub compare: Vec<Found>,
    pub editors: Vec<Found>,
    pub terminals: Vec<Found>,
    /// `merge.tool` and `diff.tool` as git sees them for the repository (or
    /// for the user, without one).
    pub git_merge_tool: Option<String>,
    pub git_diff_tool: Option<String>,
    /// Those of the paths given to [`detect`] that no longer exist.
    pub missing: Vec<PathBuf>,
}

impl Detected {
    /// The tool found with this id.
    pub fn find(&self, id: &str) -> Option<&Found> {
        self.compare.iter().chain(&self.editors).chain(&self.terminals).find(|found| found.id == id)
    }
}

/// Looks for every known tool, reads git's `merge.tool` and `diff.tool`,
/// and checks which of `check` (the programs saved in the settings) still
/// exist. Takes a few milliseconds of file lookups and two `git config`
/// runs: call it from a background thread.
pub fn detect(repo: Option<&Path>, check: &[PathBuf]) -> Detected {
    let look = Look::from_env();
    let mut detected = Detected::default();
    for known in KNOWN {
        let Some(path) = look.find(known.places()) else { continue };
        let found = Found { id: known.id, name: known.name, path };
        if known.compare {
            detected.compare.push(found.clone());
        }
        if known.editor {
            detected.editors.push(found.clone());
        }
        if known.terminal {
            detected.terminals.push(found);
        }
    }
    detected.git_merge_tool = gitconfig::effective(repo, "merge.tool").filter(|tool| !tool.is_empty());
    detected.git_diff_tool = gitconfig::effective(repo, "diff.tool").filter(|tool| !tool.is_empty());
    detected.missing = check.iter().filter(|path| !exists(path)).cloned().collect();
    detected
}

/// The folders a search looks in.
struct Look {
    path: Vec<PathBuf>,
    /// The names in each PATH folder, read on the first lookup: one listing
    /// per folder costs far less than trying every name with every
    /// extension in each (tens of thousands of file checks on Windows).
    listed: std::cell::OnceCell<Vec<std::collections::HashSet<String>>>,
    /// Extensions a bare program name may have (Windows `PATHEXT`), with
    /// their dot, in lower case; empty elsewhere.
    exts: Vec<String>,
    program_files: Vec<PathBuf>,
    programs: Option<PathBuf>,
    local: Option<PathBuf>,
    system: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl Look {
    fn from_env() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let mut program_files: Vec<PathBuf> = Vec::new();
        for name in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(dir) = var(name)
                && !program_files.contains(&dir)
            {
                program_files.push(dir);
            }
        }
        let exts = if cfg!(windows) {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
                .split(';')
                .filter(|ext| !ext.is_empty())
                .map(str::to_ascii_lowercase)
                .collect()
        } else {
            Vec::new()
        };
        let local = var("LOCALAPPDATA");
        Look {
            path: std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default(),
            listed: std::cell::OnceCell::new(),
            exts,
            program_files,
            programs: local.as_ref().map(|dir| dir.join("Programs")),
            local,
            system: var("SystemRoot").map(|dir| dir.join("System32")),
            home: dirs::home_dir(),
        }
    }

    /// The first of `places` that exists.
    fn find(&self, places: &[&str]) -> Option<PathBuf> {
        places.iter().find_map(|place| self.place(place))
    }

    fn place(&self, place: &str) -> Option<PathBuf> {
        if let Some(name) = place.strip_prefix("path:") {
            return self.which(name);
        }
        if place == "git-bash" {
            return self.git_bash();
        }
        let (roots, rest): (Vec<&PathBuf>, &str) = match place.split_once('/') {
            Some(("{pf}", rest)) => (self.program_files.iter().collect(), rest),
            Some(("{programs}", rest)) => (self.programs.iter().collect(), rest),
            Some(("{local}", rest)) => (self.local.iter().collect(), rest),
            Some(("{system}", rest)) => (self.system.iter().collect(), rest),
            Some(("{home}", rest)) => (self.home.iter().collect(), rest),
            _ => return glob(Path::new("/"), place.trim_start_matches('/')),
        };
        roots.into_iter().find_map(|root| glob(root, rest))
    }

    /// A program on PATH, the way a shell finds it (with `PATHEXT` on
    /// Windows, so `code` is `code.cmd`).
    fn which(&self, name: &str) -> Option<PathBuf> {
        // Windows compares names without regard to case.
        let windows = !self.exts.is_empty();
        let key = |name: &str| if windows { name.to_ascii_lowercase() } else { name.to_owned() };
        let has_ext = Path::new(name)
            .extension()
            .is_some_and(|ext| self.exts.contains(&format!(".{}", ext.to_string_lossy().to_ascii_lowercase())));
        let mut names = Vec::new();
        if !windows || has_ext {
            names.push(key(name));
        }
        if windows {
            names.extend(self.exts.iter().map(|ext| key(&format!("{name}{ext}"))));
        }
        let listed = self.listed.get_or_init(|| {
            self.path
                .iter()
                .map(|dir| {
                    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
                    entries.map(|entry| key(&entry.file_name().to_string_lossy())).collect()
                })
                .collect()
        });
        for (dir, listed) in self.path.iter().zip(listed) {
            for candidate in &names {
                if listed.contains(candidate) {
                    let path = dir.join(candidate);
                    // A folder of that name is no program.
                    if windows || path.is_file() {
                        return Some(path);
                    }
                }
            }
        }
        None
    }

    /// Git Bash next to the `git` on PATH (`<Git>/cmd/git.exe`).
    fn git_bash(&self) -> Option<PathBuf> {
        let git = self.which("git")?;
        git.ancestors().skip(1).take(3).map(|dir| dir.join("git-bash.exe")).find(|bash| exists(bash))
    }
}

/// True for a file or folder, and for the zero-size app links in
/// `WindowsApps` (`wt.exe`) that `Path::exists` cannot follow.
fn exists(path: &Path) -> bool {
    path.exists() || std::fs::symlink_metadata(path).is_ok()
}

/// `rest` (parts separated by `/`) under `root`, where a part with a `*`
/// matches any name of that shape; the last match in sort order wins.
fn glob(root: &Path, rest: &str) -> Option<PathBuf> {
    let mut current = root.to_path_buf();
    let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
    for (i, part) in parts.iter().enumerate() {
        let Some((prefix, suffix)) = part.split_once('*') else {
            current.push(part);
            continue;
        };
        let lower = |s: &str| s.to_lowercase();
        let (prefix, suffix) = (lower(prefix), lower(suffix));
        let mut names: Vec<String> = std::fs::read_dir(&current)
            .ok()?
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| {
                let name = lower(name);
                name.len() >= prefix.len() + suffix.len() && name.starts_with(&prefix) && name.ends_with(&suffix)
            })
            .collect();
        names.sort();
        let tail = parts[i + 1..].join("/");
        return names.iter().rev().find_map(|name| glob(&current.join(name), &tail));
    }
    exists(&current).then_some(current)
}

// ---- command lines ---------------------------------------------------------

/// What two versions of a file to compare.
#[derive(Clone, Debug, PartialEq)]
pub enum Compare {
    /// The working tree against the index.
    Unstaged,
    /// The index against HEAD.
    Staged,
    /// A commit against its first parent; `None` for a first commit, which
    /// is compared with the empty tree.
    Commit { commit: String, parent: Option<String> },
    /// A commit against the working tree (`git difftool <commit>`).
    WorkingTree { commit: String },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Merge,
    Diff,
}

/// How a tool is named to `git mergetool` or `git difftool`.
#[derive(Clone, Debug, PartialEq)]
enum GitTool {
    /// git's own choice, from its configuration.
    Configured,
    /// A tool by its name in git's configuration.
    Named(String),
    /// A known tool at the path found.
    Program { known: &'static Known, path: PathBuf },
}

fn program_tool(program: &Program) -> Result<GitTool, String> {
    match known(&program.id) {
        Some(known) if known.compare => Ok(GitTool::Program { known, path: program.path.clone() }),
        _ => Err(format!("This version of GitKrill cannot use \"{}\" as a merge or diff tool.", program.name)),
    }
}

/// The `-c` options naming the tool to git, and the `--tool` option.
fn tool_config(mode: Mode, tool: &GitTool) -> (Vec<String>, Option<String>) {
    let section = match mode {
        Mode::Merge => "mergetool",
        Mode::Diff => "difftool",
    };
    match tool {
        GitTool::Configured => (Vec::new(), None),
        GitTool::Named(name) => (Vec::new(), Some(format!("--tool={name}"))),
        GitTool::Program { known, path } => {
            let exe = git_program(known, path);
            match known.git {
                Some(name) => (
                    vec!["-c".to_owned(), format!("{section}.{name}.path={exe}")],
                    Some(format!("--tool={name}")),
                ),
                None => {
                    let name = format!("gitkrill-{}", known.id);
                    let template = match mode {
                        Mode::Merge => known.merge_cmd,
                        Mode::Diff => known.diff_cmd,
                    };
                    let mut config =
                        vec!["-c".to_owned(), format!("{section}.{name}.cmd={}", template.replace("{exe}", &sh_quote(&exe)))];
                    if mode == Mode::Merge {
                        config.extend(["-c".to_owned(), format!("mergetool.{name}.trustExitCode={}", known.trust_exit_code)]);
                    }
                    (config, Some(format!("--tool={name}")))
                }
            }
        }
    }
}

/// The program git's shell should run for a tool found at `path`, written
/// the way that shell reads it: the launcher that waits rather than the
/// application (`Code.exe` is `bin/code`, `code.cmd` is `code`), with
/// forward slashes on Windows.
fn git_program(known: &Known, path: &Path) -> String {
    let text = path.to_string_lossy();
    let lower = text.to_ascii_lowercase();
    let program = if let Some(stem) = lower.strip_suffix(".cmd").or_else(|| lower.strip_suffix(".bat")) {
        text[..stem.len()].to_owned()
    } else if let (Some(cli), true) = (known.cli, lower.ends_with(".exe")) {
        match path.parent() {
            Some(dir) => dir.join(cli).to_string_lossy().into_owned(),
            None => text.into_owned(),
        }
    } else {
        text.into_owned()
    };
    if cfg!(windows) { program.replace('\\', "/") } else { program }
}

/// Quotes a word for a POSIX shell: inside single quotes nothing is special
/// except the single quote itself.
fn sh_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The one-off alias every merge runs through (a shell function git runs).
const MERGE_ALIAS: &str = "gitkrill-mergetool";

/// `git mergetool` inside a shell function. What it prints on standard
/// output (the file, "Normal merge conflict for...") is dropped, so the
/// success toast does not end on it; errors go to standard error and still
/// show. With `guard`, it refuses to start when git has no merge tool
/// configured: git would otherwise guess one, possibly a terminal program
/// with no terminal to run in.
fn mergetool_alias(guard: bool) -> String {
    let check = if guard {
        "git config merge.tool >/dev/null || git config merge.guitool >/dev/null || \
         { echo 'No merge tool is set. Choose one in Preferences > External Tools, or set merge.tool in your Git config.' >&2; exit 1; }; "
    } else {
        ""
    };
    format!("alias.{MERGE_ALIAS}=!f() {{ {check}git mergetool --no-prompt \"$@\" >/dev/null; }}; f")
}

/// The arguments of `git` that resolve `file` (relative to the top of the
/// working tree) in the merge tool.
fn merge_args(tools: &Tools, file: &str) -> Result<Vec<String>, String> {
    let tool = match &tools.merge {
        MergeTool::GitConfig => GitTool::Configured,
        MergeTool::Program(program) => program_tool(program)?,
    };
    let (config, flag) = tool_config(Mode::Merge, &tool);
    let mut args = vec!["-c".to_owned(), mergetool_alias(tool == GitTool::Configured)];
    args.extend(config);
    args.push(MERGE_ALIAS.to_owned());
    args.extend(flag);
    args.extend(["--".to_owned(), file.to_owned()]);
    Ok(args)
}

/// The label of the command that resolves `file` in the merge tool.
pub fn merge_label(tools: &Tools, file: &str) -> String {
    let name = file.rsplit('/').next().unwrap_or(file);
    match &tools.merge {
        MergeTool::Program(program) => format!("Resolve {name} in {}", program.name),
        MergeTool::GitConfig => format!("Resolve {name} in the merge tool"),
    }
}

/// `git mergetool` on `file` as a command for `tab.run`.
pub fn merge_command(tools: &Tools, file: &str) -> Result<GitCmd, String> {
    let args = merge_args(tools, file)?;
    let name = file.rsplit('/').next().unwrap_or(file);
    let mut cmd = GitCmd::empty(merge_label(tools, file)).done(format!("Resolved {name}"));
    // The path is a file name, never a pattern.
    cmd.steps.push(Step::with_paths(args, None));
    Ok(cmd)
}

/// Picks the tool `git difftool` should use. `config` reads git's
/// configuration for the repository.
fn diff_tool(tools: &Tools, config: &dyn Fn(&str) -> Option<String>) -> Result<GitTool, String> {
    let configured = |key: &str| config(key).filter(|value| !value.trim().is_empty());
    match (&tools.diff, &tools.merge) {
        (DiffTool::Program(program), _) | (DiffTool::SameAsMerge, MergeTool::Program(program)) => {
            program_tool(program)
        }
        (DiffTool::SameAsMerge, MergeTool::GitConfig) => match configured("merge.tool") {
            Some(name) => Ok(GitTool::Named(name)),
            None => Err("No merge tool is set, and the diff tool is the merge tool. Choose one in \
                         Preferences > External Tools, or set merge.tool in your Git config."
                .to_owned()),
        },
        // git falls back to merge.tool on its own.
        (DiffTool::GitConfig, _) => match configured("diff.tool").or_else(|| configured("merge.tool")) {
            Some(_) => Ok(GitTool::Configured),
            None => Err("No diff tool is set. Choose one in Preferences > External Tools, or set \
                         diff.tool in your Git config."
                .to_owned()),
        },
    }
}

/// The arguments of `git` that open `paths` (the old name first for a
/// rename) in the diff tool. `empty_tree` is the empty tree's id, for a
/// first commit.
fn diff_args(
    tools: &Tools,
    compare: &Compare,
    paths: &[&str],
    empty_tree: Option<&str>,
    config: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<String>, String> {
    let tool = diff_tool(tools, config)?;
    let (mut args, flag) = tool_config(Mode::Diff, &tool);
    args.extend(["difftool".to_owned(), "--no-prompt".to_owned()]);
    args.extend(flag);
    match compare {
        Compare::Unstaged => {}
        Compare::Staged => args.push("--cached".to_owned()),
        Compare::Commit { commit, parent } => {
            let base = match (parent, empty_tree) {
                (Some(parent), _) => parent.clone(),
                (None, Some(empty)) => empty.to_owned(),
                (None, None) => return Err("The empty tree is needed for a first commit.".to_owned()),
            };
            args.extend([base, commit.clone()]);
        }
        Compare::WorkingTree { commit } => args.push(commit.clone()),
    }
    args.push("--".to_owned());
    args.extend(paths.iter().map(|path| (*path).to_owned()));
    Ok(args)
}

/// Fills `{exe}` and `{path}` in a known tool's command line.
fn fill(template: &[&str], exe: &Path, path: &str) -> Vec<String> {
    let exe = exe.to_string_lossy();
    template.iter().map(|part| part.replace("{exe}", &exe).replace("{path}", path)).collect()
}

/// Splits a command line typed in the preferences into words: spaces
/// separate them, and "double" or 'single' quotes keep spaces inside one.
/// Backslashes are ordinary characters, so Windows paths need no escaping.
pub fn split_command(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            None => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// The placeholder custom commands use for the file or folder.
pub const PLACEHOLDER: &str = "{path}";

/// A custom command line with `{path}` filled in. Without a placeholder,
/// `append` decides whether the path is added at the end.
fn custom(line: &str, path: &str, append: bool) -> Result<Vec<String>, String> {
    let mut words = split_command(line);
    if words.is_empty() {
        return Err("The custom command is empty.".to_owned());
    }
    let placed = words.iter().any(|word| word.contains(PLACEHOLDER));
    for word in &mut words {
        *word = word.replace(PLACEHOLDER, path);
    }
    if !placed && append {
        words.push(path.to_owned());
    }
    Ok(words)
}

/// The command line that opens `target` (a file, or a folder when
/// `folder`) in the editor.
fn editor_argv(tools: &Tools, target: &str, folder: bool) -> Result<Vec<String>, String> {
    match &tools.editor {
        Editor::None => Err("No editor is set. Choose one in Preferences > External Tools.".to_owned()),
        Editor::Custom => custom(&tools.editor_command, target, true),
        Editor::Program(program) => {
            let known = known(&program.id)
                .filter(|known| known.editor)
                .ok_or_else(|| format!("This version of GitKrill cannot use \"{}\" as an editor.", program.name))?;
            let template = if folder && !known.run_folder.is_empty() { known.run_folder } else { known.run };
            Ok(fill(template, &program.path, target))
        }
    }
}

/// The command line that opens a terminal in `dir`, and whether it needs a
/// console window of its own; `None` for the system's default terminal.
fn terminal_argv(tools: &Tools, dir: &str) -> Result<Option<(Vec<String>, bool)>, String> {
    if tools.custom_terminal && !tools.terminal_command.trim().is_empty() {
        // Terminals start in the folder they are started from, so the path
        // is only passed where the command asks for it.
        return Ok(Some((custom(&tools.terminal_command, dir, false)?, true)));
    }
    match &tools.terminal {
        Terminal::System => Ok(None),
        Terminal::Program(program) => {
            let known = known(&program.id)
                .filter(|known| known.terminal)
                .ok_or_else(|| format!("This version of GitKrill cannot use \"{}\" as a terminal.", program.name))?;
            Ok(Some((fill(known.run, &program.path, dir), known.console)))
        }
    }
}

// ---- starting them ---------------------------------------------------------

/// A scripted screenshot run, which prints commands instead of starting them.
fn scripted() -> bool {
    std::env::var_os("GITKRILL_SHOT").is_some()
}

/// A command line as a person would type it, for printing.
fn shown(argv: &[String]) -> String {
    argv.iter()
        .map(|word| if word.is_empty() || word.contains([' ', '"', '\'']) { format!("\"{word}\"") } else { word.clone() })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Set by [`poll`], so threads can wake the UI when something failed.
static CTX: OnceLock<egui::Context> = OnceLock::new();
static FAILED: AtomicBool = AtomicBool::new(false);
static FAILURES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Reports a failure from any thread; [`poll`] turns it into a toast.
fn fail(title: impl Into<String>, body: impl Into<String>) {
    if let Ok(mut list) = FAILURES.lock() {
        list.push((title.into(), body.into()));
    }
    FAILED.store(true, Ordering::Release);
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// Shows the failures of programs started from here. Called once a frame:
/// an atomic load unless something failed.
pub fn poll(ctx: &egui::Context, shared: &mut Shared) {
    CTX.get_or_init(|| ctx.clone());
    if !FAILED.swap(false, Ordering::Acquire) {
        return;
    }
    let failures = FAILURES.lock().map(|mut list| std::mem::take(&mut *list)).unwrap_or_default();
    for (title, body) in failures {
        shared.toast(ToastKind::Error, title, body);
    }
}

/// A program to start.
struct Launch {
    /// For the error toast: "Could not start <what>".
    what: String,
    argv: Vec<String>,
    dir: Option<PathBuf>,
    /// Give it a console window of its own (Windows console terminals).
    /// Everything else runs with none, so nothing flashes.
    console: bool,
}

impl Launch {
    /// Starts the program, from a background thread.
    fn spawn(&self) -> Result<std::process::Child, String> {
        let Some(first) = self.argv.first() else { return Err("The command is empty.".to_owned()) };
        let program = resolve(first);
        let mut cmd = Command::new(&program);
        cmd.args(&self.argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(dir) = &self.dir {
            cmd.current_dir(dir);
        }
        if self.console {
            new_console(&mut cmd);
        } else {
            cli::hide_window(&mut cmd);
        }
        cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("{} was not found.", program.display()),
            _ => format!("{}: {e}", program.display()),
        })
    }

    /// Starts it and reports a failure, or prints it in a scripted run.
    /// Looks the program up on PATH: call it from a background thread.
    fn run(self) {
        if scripted() {
            match &self.dir {
                Some(dir) => eprintln!("would run: {}  (in {})", shown(&self.argv), dir.display()),
                None => eprintln!("would run: {}", shown(&self.argv)),
            }
            return;
        }
        if let Err(e) = self.spawn() {
            fail(format!("Could not start {}", self.what), e);
        }
    }

    /// [`Launch::run`] on a short-lived thread.
    fn start(self) {
        std::thread::spawn(move || self.run());
    }
}

/// A bare program name found on PATH (with `PATHEXT` on Windows, which
/// `Command` does not try), or the program as given.
fn resolve(program: &str) -> PathBuf {
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        return path.to_path_buf();
    }
    Look::from_env().which(program).unwrap_or_else(|| path.to_path_buf())
}

#[cfg(windows)]
fn new_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    cmd.creation_flags(CREATE_NEW_CONSOLE);
}

#[cfg(not(windows))]
fn new_console(_cmd: &mut Command) {}

/// The command that resolves `file` (relative to the top of the working
/// tree) in the merge tool, for `tab.run`. `None` when there is nothing to
/// run: the settings name a tool this version does not know (an error toast
/// says so), or a scripted run, which prints the command instead.
pub fn merge(tools: &Tools, file: &str) -> Option<GitCmd> {
    match merge_command(tools, file) {
        Ok(cmd) if scripted() => {
            eprintln!("would run: git {}", shown(&cmd.steps[0].args));
            None
        }
        Ok(cmd) => Some(cmd),
        Err(e) => {
            fail("Could not open the merge tool", e);
            None
        }
    }
}

/// Opens a changed file in the diff tool: `path` in the working tree of
/// `workdir`, with its old name for a rename. Not waited for; a failure
/// comes back as a toast.
pub fn diff(tools: &Tools, workdir: &Path, compare: Compare, path: &str, old_path: Option<&str>) {
    let tools = tools.clone();
    let workdir = workdir.to_path_buf();
    let paths: Vec<String> = old_path.into_iter().chain([path]).map(str::to_owned).collect();
    std::thread::spawn(move || {
        if let Err(e) = run_difftool(&tools, &workdir, &compare, &paths) {
            fail("Could not open the diff tool", e);
        }
    });
}

fn run_difftool(tools: &Tools, workdir: &Path, compare: &Compare, paths: &[String]) -> Result<(), String> {
    let empty_tree = match compare {
        Compare::Commit { parent: None, .. } => Some(empty_tree(workdir)?),
        _ => None,
    };
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    let config = |key: &str| gitconfig::effective(Some(workdir), key);
    let args = diff_args(tools, compare, &paths, empty_tree.as_deref(), &config)?;
    if scripted() {
        eprintln!("would run: git {}  (in {})", shown(&args), workdir.display());
        return Ok(());
    }
    let out = cli::command(workdir)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Err(if stderr.is_empty() { format!("git difftool failed ({})", out.status) } else { stderr })
}

/// The id of the empty tree in this repository's object format.
fn empty_tree(workdir: &Path) -> Result<String, String> {
    let args = ["hash-object", "-t", "tree", "--stdin"].map(str::to_owned);
    cli::run(workdir, &args, cli::Options { stdin: Some(b""), ..Default::default() }).map(|id| id.trim().to_owned())
}

/// Opens a file or folder in the editor. Not waited for.
pub fn edit(tools: &Tools, target: &Path) {
    let tools = tools.clone();
    let target = target.to_path_buf();
    // Whether the target is a folder is a question for the disk.
    std::thread::spawn(move || match editor_argv(&tools, &native(&target), target.is_dir()) {
        Ok(argv) => {
            let what = match &tools.editor {
                Editor::Program(program) => program.name.clone(),
                _ => "the editor".to_owned(),
            };
            Launch { what, argv, dir: None, console: false }.run()
        }
        Err(e) => fail("Could not open the editor", e),
    });
}

/// Opens a terminal in `dir`. Not waited for.
pub fn terminal(tools: &Tools, dir: &Path) {
    match terminal_argv(tools, &native(dir)) {
        Ok(Some((argv, console))) => {
            let what = if tools.custom_terminal { "the terminal".to_owned() } else { terminal_name(tools) };
            Launch { what, argv, dir: Some(dir.to_path_buf()), console }.start();
        }
        Ok(None) if scripted() => eprintln!("would open the system terminal in {}", dir.display()),
        Ok(None) => {
            let dir = dir.to_path_buf();
            std::thread::spawn(move || crate::platform::terminal(&dir));
        }
        Err(e) => fail("Could not open a terminal", e),
    }
}

/// A path written with this system's separator. Paths from git use `/`,
/// which some Windows programs do not take.
fn native(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) { text.replace('/', "\\") } else { text.into_owned() }
}

fn terminal_name(tools: &Tools) -> String {
    match &tools.terminal {
        Terminal::Program(program) => program.name.clone(),
        Terminal::System => "the terminal".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(id: &str, path: &str) -> Program {
        Program { id: id.to_owned(), name: known(id).unwrap().name.to_owned(), path: PathBuf::from(path) }
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn no_config(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn every_known_tool_has_a_unique_id_and_somewhere_to_look() {
        for (i, tool) in KNOWN.iter().enumerate() {
            assert!(KNOWN[..i].iter().all(|other| other.id != tool.id), "{} twice", tool.id);
            assert!(!(tool.windows.is_empty() && tool.macos.is_empty() && tool.linux.is_empty()), "{}", tool.id);
            assert!(tool.compare || tool.editor || tool.terminal, "{}", tool.id);
            if tool.compare && tool.git.is_none() {
                assert!(tool.merge_cmd.contains("$MERGED") && tool.diff_cmd.contains("$REMOTE"), "{}", tool.id);
            }
        }
    }

    #[test]
    fn command_lines_split_on_spaces_and_keep_quoted_ones() {
        assert_eq!(split_command("code --wait {path}"), strings(&["code", "--wait", "{path}"]));
        assert_eq!(
            split_command(r#""C:\Program Files\Editor\ed.exe"  -n "{path}" ''"#),
            strings(&[r"C:\Program Files\Editor\ed.exe", "-n", "{path}", ""])
        );
        assert_eq!(split_command("vim '+set nu' a"), strings(&["vim", "+set nu", "a"]));
        assert_eq!(split_command("   "), Vec::<String>::new());
    }

    #[test]
    fn shell_quoting_survives_single_quotes() {
        assert_eq!(sh_quote("C:/Program Files/x.exe"), "'C:/Program Files/x.exe'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn git_runs_the_launcher_that_waits() {
        let code = known("vscode").unwrap();
        if cfg!(windows) {
            assert_eq!(
                git_program(code, Path::new(r"C:\Users\Ada\AppData\Local\Programs\Microsoft VS Code\Code.exe")),
                "C:/Users/Ada/AppData/Local/Programs/Microsoft VS Code/bin/code"
            );
            assert_eq!(git_program(code, Path::new(r"C:\VS Code\bin\code.CMD")), "C:/VS Code/bin/code");
        }
        assert_eq!(
            git_program(code, Path::new("/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code")),
            "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"
        );
        let bc = known("bc").unwrap();
        assert_eq!(git_program(bc, Path::new("/usr/bin/bcompare")), "/usr/bin/bcompare");
    }

    #[test]
    fn merge_uses_gits_own_tool_with_the_path_found() {
        let tools = Tools { merge: MergeTool::Program(program("bc", "/opt/bc/bcomp")), ..Tools::default() };
        assert_eq!(
            merge_args(&tools, "src/a b.rs").unwrap(),
            vec![
                "-c".to_owned(),
                mergetool_alias(false),
                "-c".to_owned(),
                "mergetool.bc.path=/opt/bc/bcomp".to_owned(),
                "gitkrill-mergetool".to_owned(),
                "--tool=bc".to_owned(),
                "--".to_owned(),
                "src/a b.rs".to_owned(),
            ]
        );
        assert_eq!(merge_label(&tools, "src/a b.rs"), "Resolve a b.rs in Beyond Compare");
        assert!(!mergetool_alias(false).contains("merge.tool"));
        assert!(mergetool_alias(false).contains("git mergetool --no-prompt \"$@\" >/dev/null"));
    }

    #[test]
    fn merge_supplies_the_command_of_a_tool_git_does_not_know() {
        let tools = Tools { merge: MergeTool::Program(program("idea", "/opt/idea/bin/idea")), ..Tools::default() };
        let args = merge_args(&tools, "a.txt").unwrap();
        assert_eq!(
            args[2..],
            strings(&[
                "-c",
                "mergetool.gitkrill-idea.cmd='/opt/idea/bin/idea' merge \"$LOCAL\" \"$REMOTE\" \"$BASE\" \"$MERGED\"",
                "-c",
                "mergetool.gitkrill-idea.trustExitCode=false",
                "gitkrill-mergetool",
                "--tool=gitkrill-idea",
                "--",
                "a.txt",
            ])
        );
    }

    #[test]
    fn merge_with_gits_default_refuses_to_guess_a_tool() {
        let args = merge_args(&Tools::default(), "a.txt").unwrap();
        assert_eq!(args, vec!["-c".to_owned(), mergetool_alias(true), "gitkrill-mergetool".into(), "--".into(), "a.txt".into()]);
        assert!(args[1].contains("git config merge.tool >/dev/null"));
        assert_eq!(merge_label(&Tools::default(), "dir/a.txt"), "Resolve a.txt in the merge tool");
    }

    #[test]
    fn diff_compares_the_right_versions() {
        let tools = Tools { diff: DiffTool::Program(program("meld", "/usr/bin/meld")), ..Tools::default() };
        let args = |compare: Compare, paths: &[&str], empty: Option<&str>| {
            diff_args(&tools, &compare, paths, empty, &no_config).unwrap()
        };
        let head = strings(&["-c", "difftool.meld.path=/usr/bin/meld", "difftool", "--no-prompt", "--tool=meld"]);
        let with = |rest: &[&str]| [head.clone(), strings(rest)].concat();
        assert_eq!(args(Compare::Unstaged, &["a.txt"], None), with(&["--", "a.txt"]));
        assert_eq!(args(Compare::Staged, &["a.txt"], None), with(&["--cached", "--", "a.txt"]));
        assert_eq!(args(Compare::WorkingTree { commit: "c0ffee".into() }, &["a.txt"], None), with(&["c0ffee", "--", "a.txt"]));
        let commit = Compare::Commit { commit: "c0ffee".into(), parent: Some("beef".into()) };
        assert_eq!(args(commit, &["old.txt", "new.txt"], None), with(&["beef", "c0ffee", "--", "old.txt", "new.txt"]));
        let first = Compare::Commit { commit: "c0ffee".into(), parent: None };
        assert_eq!(args(first.clone(), &["a.txt"], Some("4b825d")), with(&["4b825d", "c0ffee", "--", "a.txt"]));
        assert!(diff_args(&tools, &first, &["a.txt"], None, &no_config).is_err());
    }

    #[test]
    fn diff_follows_the_merge_tool_or_gits_configuration() {
        // The default: the diff tool is the merge tool.
        let merge = Tools { merge: MergeTool::Program(program("kdiff3", "/usr/bin/kdiff3")), ..Tools::default() };
        assert_eq!(
            diff_args(&merge, &Compare::Unstaged, &["a"], None, &no_config).unwrap()[..5],
            strings(&["-c", "difftool.kdiff3.path=/usr/bin/kdiff3", "difftool", "--no-prompt", "--tool=kdiff3"])
        );
        // The merge tool is git's: its name is passed, so diff.tool does not win.
        let config = |key: &str| (key == "merge.tool").then(|| "p4merge".to_owned());
        assert_eq!(
            diff_args(&Tools::default(), &Compare::Unstaged, &["a"], None, &config).unwrap(),
            strings(&["difftool", "--no-prompt", "--tool=p4merge", "--", "a"])
        );
        let message = diff_args(&Tools::default(), &Compare::Unstaged, &["a"], None, &no_config).unwrap_err();
        assert!(message.contains("No merge tool is set"), "{message}");
        // git's own diff tool: git picks it (falling back to merge.tool).
        let git = Tools { diff: DiffTool::GitConfig, ..Tools::default() };
        assert_eq!(
            diff_args(&git, &Compare::Unstaged, &["a"], None, &config).unwrap(),
            strings(&["difftool", "--no-prompt", "--", "a"])
        );
        let message = diff_args(&git, &Compare::Unstaged, &["a"], None, &no_config).unwrap_err();
        assert!(message.contains("No diff tool is set"), "{message}");
    }

    #[test]
    fn editors_open_files_and_folders() {
        let code = Tools { editor: Editor::Program(program("vscode", "/bin/code")), ..Tools::default() };
        assert_eq!(editor_argv(&code, "/r/a b.txt", false).unwrap(), strings(&["/bin/code", "/r/a b.txt"]));
        assert_eq!(editor_argv(&code, "/r", true).unwrap(), strings(&["/bin/code", "/r"]));
        let npp = Tools { editor: Editor::Program(program("notepad++", "C:/npp/notepad++.exe")), ..Tools::default() };
        assert_eq!(editor_argv(&npp, "C:/r/a.txt", false).unwrap(), strings(&["C:/npp/notepad++.exe", "C:/r/a.txt"]));
        assert_eq!(
            editor_argv(&npp, "C:/r", true).unwrap(),
            strings(&["C:/npp/notepad++.exe", "-openFoldersAsWorkspace", "C:/r"])
        );
        assert!(editor_argv(&Tools::default(), "/r", true).unwrap_err().contains("No editor"));
    }

    #[test]
    fn custom_commands_put_the_path_where_asked() {
        let editor = |command: &str| Tools { editor: Editor::Custom, editor_command: command.to_owned(), ..Tools::default() };
        assert_eq!(
            editor_argv(&editor(r#""C:\My Editor\ed.exe" --goto "{path}:1""#), r"C:\repo\a b.txt", false).unwrap(),
            strings(&[r"C:\My Editor\ed.exe", "--goto", r"C:\repo\a b.txt:1"])
        );
        // Without the placeholder the path goes at the end.
        assert_eq!(editor_argv(&editor("subl -n"), "/r/a.txt", false).unwrap(), strings(&["subl", "-n", "/r/a.txt"]));
        assert!(editor_argv(&editor("  "), "/r/a.txt", false).is_err());

        let terminal = |command: &str| Tools { custom_terminal: true, terminal_command: command.to_owned(), ..Tools::default() };
        assert_eq!(terminal_argv(&terminal("wt -d {path}"), "/r").unwrap(), Some((strings(&["wt", "-d", "/r"]), true)));
        // A terminal is started in the folder: no path unless asked for.
        assert_eq!(terminal_argv(&terminal("alacritty"), "/r").unwrap(), Some((strings(&["alacritty"]), true)));
        // An empty command falls back to the terminal chosen above it.
        assert_eq!(terminal_argv(&terminal(""), "/r").unwrap(), None);
    }

    #[test]
    fn terminals_open_in_the_repository() {
        let pick = |id: &str, path: &str| Tools { terminal: Terminal::Program(program(id, path)), ..Tools::default() };
        assert_eq!(terminal_argv(&Tools::default(), "C:/r").unwrap(), None);
        assert_eq!(
            terminal_argv(&pick("windows-terminal", "C:/wt.exe"), "C:/my repo").unwrap(),
            Some((strings(&["C:/wt.exe", "-d", "C:/my repo"]), false))
        );
        assert_eq!(terminal_argv(&pick("pwsh", "C:/pwsh.exe"), "C:/r").unwrap(), Some((strings(&["C:/pwsh.exe", "-NoLogo"]), true)));
        assert_eq!(terminal_argv(&pick("cmd", "C:/cmd.exe"), "C:/r").unwrap(), Some((strings(&["C:/cmd.exe"]), true)));
        assert_eq!(
            terminal_argv(&pick("git-bash", "C:/Git/git-bash.exe"), "C:/my repo").unwrap(),
            Some((strings(&["C:/Git/git-bash.exe", "--cd=C:/my repo"]), false))
        );
        assert_eq!(
            terminal_argv(&pick("terminal-app", "/System/Applications/Utilities/Terminal.app"), "/r").unwrap(),
            Some((strings(&["open", "-a", "/System/Applications/Utilities/Terminal.app", "/r"]), false))
        );
        assert_eq!(
            terminal_argv(&pick("gnome-terminal", "/usr/bin/gnome-terminal"), "/my repo").unwrap(),
            Some((strings(&["/usr/bin/gnome-terminal", "--working-directory=/my repo"]), false))
        );
        assert_eq!(
            terminal_argv(&pick("wezterm", "/usr/bin/wezterm"), "/r").unwrap(),
            Some((strings(&["/usr/bin/wezterm", "start", "--cwd", "/r"]), false))
        );
    }

    #[test]
    fn detection_looks_in_install_folders_and_on_path() {
        let root = std::env::temp_dir().join(format!("gitkrill-external-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let touch = |rel: &str| {
            let file = root.join(rel);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "").unwrap();
        };
        touch("pf/JetBrains/IntelliJ IDEA 2024.1/bin/idea64.exe");
        touch("pf/JetBrains/IntelliJ IDEA 2024.2/bin/idea64.exe");
        touch("programs/Microsoft VS Code/Code.exe");
        touch("bin/tool.cmd");
        touch("bin/plain");
        let look = Look {
            path: vec![root.join("nowhere"), root.join("bin")],
            listed: std::cell::OnceCell::new(),
            exts: strings(&[".exe", ".cmd"]),
            program_files: vec![root.join("pf")],
            programs: Some(root.join("programs")),
            local: None,
            system: None,
            home: None,
        };
        assert_eq!(
            look.find(&["{pf}/JetBrains/IntelliJ IDEA*/bin/idea64.exe"]),
            Some(root.join("pf/JetBrains/IntelliJ IDEA 2024.2/bin/idea64.exe"))
        );
        assert_eq!(
            look.find(&["{pf}/Microsoft VS Code/Code.exe", "{programs}/Microsoft VS Code/Code.exe"]),
            Some(root.join("programs/Microsoft VS Code/Code.exe"))
        );
        assert_eq!(look.find(&["{local}/x.exe", "path:tool"]), Some(root.join("bin/tool.cmd")));
        // With PATHEXT, a file without an extension is no program.
        assert_eq!(look.find(&["path:plain"]), None);
        let unix = Look { exts: Vec::new(), listed: std::cell::OnceCell::new(), ..look };
        assert_eq!(unix.find(&["path:plain"]), Some(root.join("bin/plain")));
        assert_eq!(unix.find(&["{pf}/Nothing*/x"]), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_program_that_cannot_start_says_which_and_why() {
        let launch = Launch {
            what: "the editor".to_owned(),
            argv: strings(&["gitkrill-no-such-program", "a.txt"]),
            dir: None,
            console: false,
        };
        let message = launch.spawn().unwrap_err();
        assert_eq!(message, "gitkrill-no-such-program was not found.");
        assert!(Launch { argv: Vec::new(), ..launch }.spawn().is_err());
    }

    // ---- against real repositories -----------------------------------------

    use crate::git::ChangeKind;
    use crate::tests::{Fixture, git};

    /// A conflict on `a.txt`: main and topic both changed its one line.
    fn conflicted(name: &str) -> Fixture {
        let fx = Fixture::new(name);
        fx.write("a.txt", "base\n");
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "topic"]);
        fx.write("a.txt", "topic\n");
        fx.commit_all("topic");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("a.txt", "main\n");
        fx.commit_all("main");
        let _ = Command::new("git").current_dir(&fx.dir).args(["merge", "-q", "topic"]).output();
        fx
    }

    /// A shell script standing in for a GUI tool, in a folder whose name has
    /// a space. As a merge tool it writes "resolved" to its output (after
    /// `-o`, or its last argument); as a diff tool it copies the two files it
    /// is given into `$FAKE_OUT`.
    fn fake_tool(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("fake tools {name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake tool.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             if [ -n \"$FAKE_OUT\" ]; then cat \"$1\" > \"$FAKE_OUT/left\"; cat \"$2\" > \"$FAKE_OUT/right\"; exit 0; fi\n\
             out=''\n\
             while [ $# -gt 0 ]; do\n\
               if [ \"$1\" = -o ]; then out=\"$2\"; fi\n\
               last=\"$1\"; shift\n\
             done\n\
             [ -n \"$out\" ] || out=\"$last\"\n\
             sleep 1\n\
             printf 'resolved\\n' > \"$out\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    /// Keeps the user's own git configuration out of a test.
    fn isolated(fx: &Fixture, mut cmd: GitCmd) -> GitCmd {
        let global = fx.dir.join(".git").join("test-global-config");
        std::fs::write(&global, "").unwrap();
        for step in &mut cmd.steps {
            step.env.push(("GIT_CONFIG_GLOBAL".to_owned(), global.display().to_string()));
            step.env.push(("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()));
        }
        cmd
    }

    fn conflict_state(fx: &Fixture) -> Option<ChangeKind> {
        let snap = fx.snapshot();
        snap.status.unstaged.iter().chain(&snap.status.staged).find(|f| f.path == "a.txt").map(|f| f.kind)
    }

    #[test]
    fn a_merge_tool_resolves_the_conflict() {
        let fx = conflicted("mergetool");
        let tool = fake_tool("merge");
        assert_eq!(conflict_state(&fx), Some(ChangeKind::Conflicted));

        // A tool git knows, at the path found (git's own kdiff3 script).
        let tools = Tools {
            merge: MergeTool::Program(Program { id: "kdiff3".into(), name: "KDiff3".into(), path: tool.clone() }),
            ..Tools::default()
        };
        let done = fx.run(isolated(&fx, merge_command(&tools, "a.txt").unwrap()));
        assert_eq!(done.result.unwrap(), "");
        assert_eq!(fx.read("a.txt"), "resolved\n");
        assert_eq!(conflict_state(&fx), Some(ChangeKind::Modified));
        // git keeps the merged file as it was, unless told otherwise.
        assert!(fx.dir.join("a.txt.orig").exists());
        let _ = std::fs::remove_dir_all(tool.parent().unwrap());
    }

    #[test]
    fn a_tool_git_does_not_know_gets_its_command_line() {
        let fx = conflicted("mergetool-custom");
        let tool = fake_tool("custom");
        git(&fx.dir, &["config", "mergetool.keepBackup", "false"]);
        let tools = Tools {
            merge: MergeTool::Program(Program { id: "idea".into(), name: "IntelliJ IDEA".into(), path: tool.clone() }),
            ..Tools::default()
        };
        fx.run(isolated(&fx, merge_command(&tools, "a.txt").unwrap()));
        assert_eq!(fx.read("a.txt"), "resolved\n");
        assert_eq!(conflict_state(&fx), Some(ChangeKind::Modified));
        assert!(!fx.dir.join("a.txt.orig").exists(), "keepBackup=false deletes the backup");
        let _ = std::fs::remove_dir_all(tool.parent().unwrap());
    }

    #[test]
    fn gits_default_without_a_merge_tool_says_so() {
        let fx = conflicted("mergetool-unset");
        let done = fx.run_as(isolated(&fx, merge_command(&Tools::default(), "a.txt").unwrap()), crate::worker::Origin::User);
        let message = done.result.unwrap_err();
        assert!(message.starts_with("No merge tool is set."), "{message}");
        assert_eq!(conflict_state(&fx), Some(ChangeKind::Conflicted));
    }

    #[test]
    fn a_diff_tool_gets_both_versions() {
        let fx = Fixture::new("difftool");
        fx.write("a.txt", "one\n");
        fx.commit_all("first");
        fx.write("a.txt", "two\n");
        fx.commit_all("second");
        fx.write("a.txt", "three\n");
        git(&fx.dir, &["add", "a.txt"]);
        fx.write("a.txt", "four\n");
        let tool = fake_tool("diff");
        let out = tool.parent().unwrap().to_path_buf();
        let tools = Tools {
            diff: DiffTool::Program(Program { id: "bc".into(), name: "Beyond Compare".into(), path: tool.clone() }),
            ..Tools::default()
        };
        let global = fx.dir.join(".git").join("test-global-config");
        std::fs::write(&global, "").unwrap();
        let env = [
            ("FAKE_OUT".to_owned(), out.display().to_string()),
            ("GIT_CONFIG_GLOBAL".to_owned(), global.display().to_string()),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        ];
        let compare = |compare: Compare| {
            let empty = matches!(compare, Compare::Commit { parent: None, .. }).then(|| empty_tree(&fx.dir).unwrap());
            let args = diff_args(&tools, &compare, &["a.txt"], empty.as_deref(), &no_config).unwrap();
            cli::run(&fx.dir, &args, cli::Options { env: &env, ..Default::default() }).unwrap();
            let read = |side| std::fs::read_to_string(out.join(side)).unwrap();
            (read("left"), read("right"))
        };
        assert_eq!(compare(Compare::Unstaged), ("three\n".into(), "four\n".into()));
        assert_eq!(compare(Compare::Staged), ("two\n".into(), "three\n".into()));
        let head = git(&fx.dir, &["rev-parse", "HEAD"]);
        let first = git(&fx.dir, &["rev-parse", "HEAD~1"]);
        assert_eq!(compare(Compare::Commit { commit: head, parent: Some(first.clone()) }), ("one\n".into(), "two\n".into()));
        assert_eq!(compare(Compare::Commit { commit: first, parent: None }), ("".into(), "one\n".into()));
        let _ = std::fs::remove_dir_all(&out);
    }
}
