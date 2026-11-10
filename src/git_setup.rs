//! Git itself: whether it is installed, and getting it when it is not.
//!
//! GitKrill reads repositories through libgit2 and needs no Git for that,
//! but everything that changes a repository runs the `git` program. Shortly
//! after start a background thread looks for it (`git::cli::locate`); when
//! there is none, the user is offered to get it:
//!
//! - on Windows, GitKrill downloads the latest Git for Windows installer
//!   from its GitHub releases, checks it against the SHA-256 published with
//!   it, and runs it silently for the current user only: no administrator
//!   rights, into `%LOCALAPPDATA%\Programs\Git`, with Git's own defaults;
//! - on macOS, Apple's Command Line Tools (which include Git) or Homebrew;
//! - on Linux, the distribution's package manager.
//!
//! The Windows installer offers the same download while GitKrill installs
//! (`installer/gitkrill.iss`).

use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::git::cli;
use crate::update::{self, Net, Release, Source, Wire};

/// Where Git for Windows is described to the user and can be fetched by hand.
pub const DOWNLOAD_PAGE: &str = "https://git-scm.com/downloads";

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Not looked for yet, or being looked for.
    Checking,
    /// `git --version`'s answer.
    Found(String),
    /// Why there is no usable git.
    Missing(String),
    /// Fetching Git for Windows (Windows only).
    Downloading { received: u64, total: Option<u64> },
    /// Its installer is running.
    Installing,
    /// Getting Git failed; git is still missing.
    Failed(String),
}

impl State {
    /// Git cannot be used, and nothing is being done about it.
    pub fn is_missing(&self) -> bool {
        matches!(self, State::Missing(_) | State::Failed(_))
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, State::Checking | State::Downloading { .. } | State::Installing)
    }
}

/// How an install the user started ended: the new version, or why not.
pub type Outcome = Result<String, String>;

struct Inner {
    state: State,
    /// An install that finished, to tell the user once.
    finished: Option<Outcome>,
}

pub struct GitSetup {
    inner: Arc<Mutex<Inner>>,
    started: bool,
    /// The user was asked once this run.
    pub prompted: bool,
    source: Source,
}

impl GitSetup {
    pub fn new() -> Self {
        GitSetup {
            inner: Arc::new(Mutex::new(Inner { state: State::Checking, finished: None })),
            started: false,
            prompted: false,
            source: Source::git_for_windows(),
        }
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state.clone()
    }

    /// An install that ended since the last call.
    pub fn take_finished(&mut self) -> Option<Outcome> {
        self.inner.lock().unwrap().finished.take()
    }

    /// Looks for git the first time it is called.
    pub fn start(&mut self, ctx: &egui::Context) {
        if !self.started {
            self.started = true;
            self.check(ctx);
        }
    }

    /// Looks for git again (the user installed it by hand, or the window
    /// came back to the front).
    pub fn check(&mut self, ctx: &egui::Context) {
        if matches!(self.state(), State::Downloading { .. } | State::Installing) {
            return;
        }
        self.inner.lock().unwrap().state = State::Checking;
        let (inner, ctx) = (self.inner.clone(), ctx.clone());
        std::thread::spawn(move || {
            let state = match find() {
                Ok(version) => State::Found(version),
                Err(why) => State::Missing(why),
            };
            inner.lock().unwrap().state = state;
            ctx.request_repaint();
        });
    }

    /// Whether this system can have Git installed from inside GitKrill.
    pub fn can_install(&self) -> bool {
        cfg!(windows)
    }

    /// Windows: downloads Git for Windows, checks it, installs it for the
    /// current user and looks for git again.
    pub fn install(&mut self, ctx: &egui::Context) {
        if !self.can_install() || self.state().is_busy() {
            return;
        }
        // Scripted screenshot runs only ever install from a local test feed.
        if std::env::var_os("GITKRILL_SHOT").is_some() && !self.source.is_local() {
            self.inner.lock().unwrap().state = State::Failed("Not in a scripted run".to_owned());
            return;
        }
        self.inner.lock().unwrap().state = State::Downloading { received: 0, total: None };
        let (inner, ctx, source) = (self.inner.clone(), ctx.clone(), self.source.clone());
        std::thread::spawn(move || {
            let progress_inner = inner.clone();
            let progress_ctx = ctx.clone();
            let mut last_paint = std::time::Instant::now();
            let mut progress = move |received: u64, total: Option<u64>| {
                if let State::Downloading { received: r, total: t } = &mut progress_inner.lock().unwrap().state {
                    (*r, *t) = (received, total);
                }
                if last_paint.elapsed() > Duration::from_millis(100) {
                    last_paint = std::time::Instant::now();
                    progress_ctx.request_repaint();
                }
            };
            let on_installing = || {
                inner.lock().unwrap().state = State::Installing;
                ctx.request_repaint();
            };
            let outcome = fetch_and_install(&mut Wire::new(), &source, &mut progress, on_installing);
            let mut inner = inner.lock().unwrap();
            inner.state = match &outcome {
                Ok(version) => State::Found(version.clone()),
                Err(why) => State::Failed(why.clone()),
            };
            inner.finished = Some(outcome);
            ctx.request_repaint();
        });
    }
}

/// Looks for git and asks it for its version.
fn find() -> Result<String, String> {
    cli::relocate().ok_or_else(|| cli::NOT_FOUND.to_owned())?;
    cli::version()
}

/// The installer for this machine among Git for Windows' downloads: the
/// 64-bit one, or the ARM64 one on ARM64 Windows (where GitKrill itself
/// runs emulated).
pub fn installer_asset(release: &Release, arm64: bool) -> Option<&str> {
    let suffix = if arm64 { "-arm64.exe" } else { "-64-bit.exe" };
    release
        .assets
        .iter()
        .map(|asset| asset.name.as_str())
        // The name becomes part of an address and of a file name.
        .filter(|name| name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')))
        .find(|name| name.starts_with("Git-") && name.ends_with(suffix))
}

/// Whether Windows runs on an ARM processor. `PROCESSOR_IDENTIFIER` comes
/// from the system, so it names the real processor even to an emulated
/// program.
fn arm64_windows() -> bool {
    std::env::var("PROCESSOR_IDENTIFIER").is_ok_and(|id| id.starts_with("ARM"))
}

/// The whole Windows install: the latest release, its installer (checked),
/// the silent per-user install, and git found where it went. Returns git's
/// version. `installing` is called once the download is done.
fn fetch_and_install(
    net: &mut dyn Net,
    source: &Source,
    progress: &mut dyn FnMut(u64, Option<u64>),
    installing: impl FnOnce(),
) -> Result<String, String> {
    let feed = net.get(source, source.feed(), 2 * 1024 * 1024, &mut |_, _| {})?;
    let release = update::parse_git_for_windows(&String::from_utf8_lossy(&feed))?;
    let name = installer_asset(&release, arm64_windows())
        .ok_or_else(|| format!("Git for Windows {} has no installer for this computer", release.tag))?
        .to_owned();
    let file = update::download(net, source, &release, &name, &update::download_dir(), progress)?;
    installing();
    let result = run_installer(&file);
    let _ = std::fs::remove_file(&file);
    result?;
    find().map_err(|_| "Git was installed, but GitKrill cannot find it. Restart GitKrill.".to_owned())
}

/// Where Git for Windows goes: the per-user programs folder.
#[cfg(windows)]
pub fn install_dir() -> PathBuf {
    cli::user_programs().join("Git")
}

/// Runs Git for Windows' installer without questions or windows, for the
/// current user, with Git's own default choices, and waits for it.
#[cfg(windows)]
fn run_installer(file: &Path) -> Result<(), String> {
    let log = update::download_dir().join("git-setup.log");
    let status = Command::new(file)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/NOCANCEL", "/SP-"])
        .arg(format!("/DIR={}", install_dir().display()))
        .arg(format!("/LOG={}", log.display()))
        .status()
        .map_err(|e| format!("Could not start Git's installer ({e})"))?;
    match status.code() {
        Some(0) => Ok(()),
        code => Err(format!(
            "Git's installer stopped (exit code {}). Its log is {}",
            code.map_or("none".to_owned(), |c| c.to_string()),
            log.display()
        )),
    }
}

#[cfg(not(windows))]
fn run_installer(_file: &Path) -> Result<(), String> {
    Err("Git is installed from inside GitKrill on Windows only".to_owned())
}

/// macOS: asks the system to install the Command Line Tools, which include
/// Git. macOS shows its own window for it.
pub fn install_command_line_tools() -> Result<(), String> {
    Command::new("xcode-select")
        .arg("--install")
        .spawn()
        .map(drop)
        .map_err(|e| format!("Could not ask macOS to install the tools ({e})"))
}

/// The command that installs Git on this Linux distribution, from the
/// contents of `/etc/os-release`.
pub fn linux_command(os_release: &str) -> &'static str {
    let field = |key: &str| {
        os_release
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(|value| value.trim_matches('"').to_ascii_lowercase())
            .unwrap_or_default()
    };
    let ids = format!("{} {}", field("ID"), field("ID_LIKE"));
    let is = |name: &str| ids.split_whitespace().any(|id| id == name);
    if is("debian") || is("ubuntu") {
        "sudo apt install git"
    } else if is("fedora") || is("rhel") || is("centos") {
        "sudo dnf install git"
    } else if is("arch") {
        "sudo pacman -S git"
    } else if is("suse") || is("opensuse") || ids.contains("opensuse") {
        "sudo zypper install git"
    } else if is("alpine") {
        "sudo apk add git"
    } else {
        "Install the git package with your package manager"
    }
}

/// [`linux_command`] for the system GitKrill runs on.
pub fn this_linux_command() -> &'static str {
    linux_command(&std::fs::read_to_string("/etc/os-release").unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::Asset;

    fn release(names: &[&str]) -> Release {
        Release {
            version: crate::update::Version(2, 56, 0),
            tag: "v2.56.0.windows.4".into(),
            notes: String::new(),
            assets: names.iter().map(|n| Asset { name: (*n).to_owned(), size: 1, sha256: None }).collect(),
        }
    }

    #[test]
    fn picks_the_installer_for_the_processor() {
        let release = release(&[
            "Git-2.56.0.4-64-bit.tar.bz2",
            "MinGit-2.56.0.4-64-bit.zip",
            "PortableGit-2.56.0.4-64-bit.7z.exe",
            "Git-2.56.0.4-arm64.exe",
            "Git-2.56.0.4-64-bit.exe",
        ]);
        assert_eq!(installer_asset(&release, false), Some("Git-2.56.0.4-64-bit.exe"));
        assert_eq!(installer_asset(&release, true), Some("Git-2.56.0.4-arm64.exe"));
        assert_eq!(installer_asset(&super::tests::release(&["MinGit-2.56.0.4-64-bit.zip"]), false), None);
        assert_eq!(installer_asset(&super::tests::release(&["Git-../y-64-bit.exe"]), false), None);
    }

    #[test]
    fn linux_distributions_get_their_package_manager() {
        let ubuntu = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n";
        assert_eq!(linux_command(ubuntu), "sudo apt install git");
        assert_eq!(linux_command("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"), "sudo apt install git");
        assert_eq!(linux_command("ID=fedora\n"), "sudo dnf install git");
        assert_eq!(linux_command("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"), "sudo dnf install git");
        assert_eq!(linux_command("ID=manjaro\nID_LIKE=arch\n"), "sudo pacman -S git");
        assert_eq!(linux_command("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n"), "sudo zypper install git");
        assert_eq!(linux_command("ID=alpine\n"), "sudo apk add git");
        assert!(linux_command("").starts_with("Install the git package"));
        // A key that merely starts like another is not read as it.
        assert!(linux_command("IDENTITY=debian\n").starts_with("Install"));
    }

    #[test]
    fn git_for_windows_releases_are_read_with_their_checksums() {
        let json = r#"{
            "tag_name": "v2.56.0.windows.4",
            "body": "Filename | SHA-256\r\n-------- | -------\r\nGit-2.56.0.4-arm64.exe | E7DCC4ADBB0EB373064598CD18BC75E60128707D816EAD51CD9BD98E356F0075\r\n",
            "assets": [
                {"name": "Git-2.56.0.4-64-bit.exe", "size": 66850032, "digest": "sha256:4961a4a8cc778d8b6e00a5819e5fe4d078b754555aff7aedc2d77a5d74c2de80"},
                {"name": "Git-2.56.0.4-arm64.exe", "size": 64406136}
            ]
        }"#;
        let release = update::parse_git_for_windows(json).unwrap();
        assert_eq!(release.version, crate::update::Version(2, 56, 0));
        assert_eq!(release.assets[0].sha256.as_deref(), Some("4961a4a8cc778d8b6e00a5819e5fe4d078b754555aff7aedc2d77a5d74c2de80"));
        // No digest from GitHub: the table in the notes has it.
        assert_eq!(release.assets[1].sha256.as_deref(), Some("e7dcc4adbb0eb373064598cd18bc75e60128707d816ead51cd9bd98e356f0075"));

        for tag in ["v2.56.0-rc1.windows.1", "v2.56.0.windows.4/../x", "nightly"] {
            let json = format!(r#"{{"tag_name": "{tag}", "assets": []}}"#);
            assert!(update::parse_git_for_windows(&json).is_err(), "{tag}");
        }
        let draft = r#"{"tag_name": "v2.56.0.windows.4", "draft": true}"#;
        assert!(update::parse_git_for_windows(draft).is_err());
    }
}
