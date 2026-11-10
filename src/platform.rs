//! The few things that need to ask the operating system for help.

use std::path::Path;
use std::process::Command;

use crate::git::cli::hide_window;

/// Opens a file with its default application.
pub fn open(path: &Path) {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", ""]).arg(path);
        hide_window(&mut cmd);
        let _ = cmd.spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = Command::new("xdg-open").arg(path).spawn();
    }
}

/// Shows a file or folder in the system file manager.
pub fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        // Explorer wants backslashes and its own /select, syntax.
        let native = path.to_string_lossy().replace('/', "\\");
        let mut cmd = Command::new("explorer");
        if path.is_dir() {
            cmd.arg(native);
        } else {
            cmd.arg(format!("/select,{native}"));
        }
        let _ = cmd.spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg("-R").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        let _ = Command::new("xdg-open").arg(dir).spawn();
    }
}

/// Opens a terminal in `dir`.
pub fn terminal(dir: &Path) {
    #[cfg(windows)]
    {
        // Windows Terminal when installed, the classic console otherwise.
        let modern = Command::new("wt.exe").arg("-d").arg(dir).spawn();
        if modern.is_err() {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "start", "cmd.exe"]).current_dir(dir);
            hide_window(&mut cmd);
            let _ = cmd.spawn();
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").args(["-a", "Terminal"]).arg(dir).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = hide_window;
        for term in ["x-terminal-emulator", "gnome-terminal", "konsole", "xterm"] {
            if Command::new(term).current_dir(dir).spawn().is_ok() {
                break;
            }
        }
    }
}
