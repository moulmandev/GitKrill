//! Thin wrapper around the `git` executable.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::RwLock;

/// Environment every git command gets on top of GitKrill's own: the SSH key
/// and the author of the profile in use (`crate::prefs::identity::apply`).
static SESSION_ENV: RwLock<Vec<(String, String)>> = RwLock::new(Vec::new());

/// Replaces the environment [`command`] adds to every git command.
#[allow(dead_code)] // TODO(prefs scaffold): used by Profiles and SSH.
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

/// A `git` command rooted at `dir`, set up so it can never block on a prompt
/// or flash a console window.
pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
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
            "Could not find `git`. Install Git and make sure it is on your PATH.".to_owned()
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
