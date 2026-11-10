//! Thin wrapper around the `git` executable.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::RwLock;

/// Environment every git command gets on top of GitKrill's own: the SSH key
/// and the author of the profile in use (`crate::prefs::identity::apply`).
static SESSION_ENV: RwLock<Vec<(String, String)>> = RwLock::new(Vec::new());

/// Replaces the environment [`command`] adds to every git command.
pub fn set_session_env(env: Vec<(String, String)>) {
    if let Ok(mut current) = SESSION_ENV.write() {
        *current = env;
    }
}

/// Puts [`set_session_env`]'s variables on `cmd`; for the few places that
/// start git without [`command`].
pub fn apply_session_env(cmd: &mut Command) {
    if let Ok(env) = SESSION_ENV.read() {
        for (key, value) in env.iter() {
            cmd.env(key, value);
        }
    }
}

/// What a failed start of git tells the user.
pub const NOT_FOUND: &str = "Could not find Git. Install it (the warning in the status bar offers to), then try again.";

/// The git executable in use, once looked for.
static PROGRAM: RwLock<Option<PathBuf>> = RwLock::new(None);

/// The git executable commands run: looked for the first time it is needed
/// (see [`locate`]), and again by [`relocate`]. When there is none, what
/// was asked for, so that starting it fails the usual way.
pub fn program() -> PathBuf {
    if let Some(path) = PROGRAM.read().ok().and_then(|p| p.clone()) {
        return path;
    }
    relocate().unwrap_or_else(fallback)
}

/// Looks for git again, after it was installed, and uses what is found.
pub fn relocate() -> Option<PathBuf> {
    let found = locate();
    if let Ok(mut program) = PROGRAM.write() {
        *program = Some(found.clone().unwrap_or_else(fallback));
    }
    found
}

/// `GITKRILL_GIT` when set (even to a file that is not there), else `git`.
fn fallback() -> PathBuf {
    std::env::var_os("GITKRILL_GIT").map_or_else(|| PathBuf::from("git"), PathBuf::from)
}

/// Where git is. `GITKRILL_GIT` names it outright (nothing else is tried
/// then); otherwise the folders on `PATH`, then the places installers put
/// it. The last ones matter when git was installed while GitKrill ran
/// (the new `PATH` only reaches programs started afterwards), or with the
/// "Git Bash only" option, or when a Mac app starts without the shell's
/// `PATH`.
pub fn locate() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("GITKRILL_GIT") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let exe = if cfg!(windows) { "git.exe" } else { "git" };
    let paths = std::env::var_os("PATH").unwrap_or_default();
    if let Some(found) = std::env::split_paths(&paths).map(|dir| dir.join(exe)).find(|path| path.is_file()) {
        return Some(found);
    }
    installed_locations().into_iter().find(|path| path.is_file())
}

/// Where Git for Windows installs itself: for the current user, for
/// everyone, or wherever its registry entry says.
#[cfg(windows)]
fn installed_locations() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    roots.push(user_programs().join("Git"));
    for var in ["ProgramW6432", "ProgramFiles"] {
        if let Some(dir) = std::env::var_os(var) {
            roots.push(PathBuf::from(dir).join("Git"));
        }
    }
    for key in [r"HKCU\Software\GitForWindows", r"HKLM\Software\GitForWindows"] {
        let mut query = Command::new("reg");
        query.args(["query", key, "/v", "InstallPath"]);
        hide_window(&mut query);
        if let Ok(out) = query.stdin(Stdio::null()).stderr(Stdio::null()).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            // "    InstallPath    REG_SZ    C:\Program Files\Git"
            if let Some((_, value)) = text.lines().find_map(|line| line.trim().split_once("REG_SZ")) {
                roots.push(PathBuf::from(value.trim()));
            }
        }
    }
    roots.into_iter().map(|root| root.join("cmd").join("git.exe")).collect()
}

/// `%LOCALAPPDATA%\Programs`, where per-user installs go.
#[cfg(windows)]
pub fn user_programs() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .unwrap_or_default()
        .join("Programs")
}

#[cfg(not(windows))]
fn installed_locations() -> Vec<PathBuf> {
    // Homebrew on Apple silicon and Intel, then the system's.
    ["/opt/homebrew/bin/git", "/usr/local/bin/git", "/usr/bin/git"].into_iter().map(PathBuf::from).collect()
}

/// `git --version`'s answer ("git version 2.51.2.windows.1"), or why there
/// is none.
pub fn version() -> Result<String, String> {
    let mut cmd = Command::new(program());
    cmd.arg("--version").stdin(Stdio::null());
    hide_window(&mut cmd);
    let out = cmd.output().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => NOT_FOUND.to_owned(),
        _ => format!("Failed to start git: {e}"),
    })?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() && text.starts_with("git version") {
        Ok(text)
    } else {
        // A Mac without the developer tools answers with an error (and
        // offers to install them).
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if err.is_empty() { format!("git --version failed ({})", out.status) } else { err })
    }
}

/// A `git` command rooted at `dir`, set up so it can never block on a prompt
/// or flash a console window.
pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new(program());
    cmd.current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .args(["-c", "color.ui=false", "-c", "core.quotepath=false"]);
    apply_session_env(&mut cmd);
    hide_window(&mut cmd);
    cmd
}

#[cfg(windows)]
pub fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn hide_window(_cmd: &mut Command) {}

/// How to run one git command. `Default` is a plain, interactive-safe run.
#[derive(Default)]
pub struct Options<'a> {
    /// Bytes to feed on standard input.
    pub stdin: Option<&'a [u8]>,
    /// Treat every pathspec as an exact path instead of a glob. Opt-in per
    /// command because some porcelain (`stash push --include-untracked`, for
    /// one) silently misbehaves when it is set.
    pub literal_paths: bool,
    /// For commands nobody asked for (auto-fetch): tells the credential
    /// manager not to open a login window if credentials are missing.
    pub background: bool,
    /// Extra environment variables, applied last so they win.
    pub env: &'a [(String, String)],
}

/// Runs `git <args>` in `dir`. On success returns stdout followed by stderr
/// (git reports plenty of useful progress on stderr); on failure returns
/// whatever git printed.
pub fn run(dir: &Path, args: &[String], options: Options<'_>) -> Result<String, String> {
    let Options { stdin, literal_paths, background, env } = options;
    let mut cmd = command(dir);
    if literal_paths {
        cmd.env("GIT_LITERAL_PATHSPECS", "1");
    }
    if background {
        cmd.env("GCM_INTERACTIVE", "Never").env("GIT_ASKPASS", "");
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            NOT_FOUND.to_owned()
        } else {
            format!("Failed to start git: {e}")
        }
    })?;

    if let Some(bytes) = stdin
        && let Some(mut pipe) = child.stdin.take() {
            // Git may exit before reading everything (bad patch); the exit
            // status below is what we report.
            let _ = pipe.write_all(bytes);
        }

    let out = child.wait_with_output().map_err(|e| format!("git failed: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();

    if out.status.success() {
        Ok(match (stdout.is_empty(), stderr.is_empty()) {
            (false, false) => format!("{stdout}\n{stderr}"),
            (false, true) => stdout,
            _ => stderr,
        })
    } else {
        Err(match (stderr.is_empty(), stdout.is_empty()) {
            (false, false) => format!("{stderr}\n{stdout}"),
            (false, true) => stderr,
            (true, false) => stdout,
            (true, true) => format!("git {} failed ({})", args.join(" "), out.status),
        })
    }
}
