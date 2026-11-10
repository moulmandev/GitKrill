//! Git configuration for the preferences pages: the user's global file
//! (`~/.gitconfig`) and a repository's own (`.git/config`).
//!
//! A [`ConfigFile`] reads every value once, in the background, when a page
//! first needs it, and keeps them in memory while preferences are open.
//! Writes go through `git config` on a background thread of their own (in
//! the order they were made), so they land exactly where git would put them
//! and every other git tool sees them; the copy in memory changes at once,
//! so a page never waits for the disk.
//!
//! Scripted screenshot runs and tests never touch the real global file: with
//! `GITKRILL_CONFIG_DIR` set and `GIT_CONFIG_GLOBAL` not, "global" means
//! `<config dir>/gitconfig` (`tools/shot.sh` sets `GIT_CONFIG_GLOBAL` to that
//! same file, so the git commands of the run read what the pages wrote).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use eframe::egui;

use crate::git::cli;

/// Which configuration file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The user's own settings, for every repository (`git config --global`).
    Global,
    /// One repository's `.git/config` (`git config --local`), by working
    /// directory.
    Repo(PathBuf),
    /// A file named outright (`git config --file`): for tests.
    #[cfg_attr(not(test), allow(dead_code))]
    File(PathBuf),
}

impl Scope {
    /// The `git config` arguments that pick this file, and the directory to
    /// run git in.
    fn target(&self) -> (Vec<String>, PathBuf) {
        let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
        match self {
            Scope::Global => match test_global_file() {
                Some(file) => (vec!["--file".to_owned(), file.display().to_string()], home),
                None => (vec!["--global".to_owned()], home),
            },
            Scope::Repo(dir) => (vec!["--local".to_owned()], dir.clone()),
            Scope::File(file) => (vec!["--file".to_owned(), file.display().to_string()], home),
        }
    }

    /// The file itself when it is known without asking git, so a missing one
    /// reads as empty rather than as an error.
    fn known_file(&self) -> Option<PathBuf> {
        match self {
            Scope::Global => test_global_file().or_else(|| std::env::var_os("GIT_CONFIG_GLOBAL").map(PathBuf::from)),
            Scope::Repo(_) => None,
            Scope::File(file) => Some(file.clone()),
        }
    }
}

/// Where "global" points during a scripted run that did not say.
fn test_global_file() -> Option<PathBuf> {
    if std::env::var_os("GIT_CONFIG_GLOBAL").is_some() {
        return None;
    }
    std::env::var_os("GITKRILL_CONFIG_DIR").map(|dir| PathBuf::from(dir).join("gitconfig"))
}

/// Git compares section and variable names without regard to case, and
/// subsection names exactly: `Core.AutoCRLF` is `core.autocrlf`, while
/// `branch.Main.remote` keeps `Main`. `git config --list` prints names this
/// way, so lookups normalise the same way.
pub fn normalize(key: &str) -> String {
    match (key.find('.'), key.rfind('.')) {
        (Some(first), Some(last)) if first < last => format!(
            "{}{}{}",
            key[..first].to_ascii_lowercase(),
            &key[first..last],
            key[last..].to_ascii_lowercase()
        ),
        _ => key.to_ascii_lowercase(),
    }
}

/// Every `key = value` in the file, in file order (a key can repeat). Blocks:
/// call it from a background thread.
pub fn read(scope: &Scope) -> Result<Vec<(String, String)>, String> {
    if let Some(file) = scope.known_file()
        && !file.exists()
    {
        return Ok(Vec::new());
    }
    let (mut args, dir) = scope.target();
    args.insert(0, "config".to_owned());
    args.extend(["--list".to_owned(), "-z".to_owned()]);
    match cli::run(&dir, &args, cli::Options::default()) {
        Ok(out) => Ok(parse_list(&out)),
        // `--global` without a ~/.gitconfig yet, or a repository without
        // a config file: nothing is set.
        Err(e) if e.contains("unable to read config file") || e.trim().is_empty() => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// `git config --list -z` output: `key\nvalue` records separated by NUL; a
/// key alone (no newline) is a boolean set to true.
fn parse_list(out: &str) -> Vec<(String, String)> {
    out.split('\0')
        .filter(|record| !record.trim().is_empty())
        .map(|record| match record.split_once('\n') {
            Some((key, value)) => (key.trim_start().to_owned(), value.to_owned()),
            None => (record.trim().to_owned(), "true".to_owned()),
        })
        .collect()
}

/// Sets `key` to `value`, or removes every value of it with `None`. Blocks:
/// call it from a background thread.
pub fn write(scope: &Scope, key: &str, value: Option<&str>) -> Result<(), String> {
    let (target, dir) = scope.target();
    let mut args = vec!["config".to_owned()];
    args.extend(target);
    match value {
        Some(value) => args.extend([key.to_owned(), value.to_owned()]),
        None => args.extend(["--unset-all".to_owned(), key.to_owned()]),
    }
    match cli::run(&dir, &args, cli::Options::default()) {
        Ok(_) => Ok(()),
        // Unsetting what is not set (exit status 5) is no failure.
        // Windows prints the status as "exit code: 5", Unix as "exit status: 5".
        Err(e) if value.is_none() && (e.contains("exit code: 5") || e.contains("exit status: 5") || e.contains("no such section")) => Ok(()),
        Err(e) => Err(e),
    }
}

type Entries = Vec<(String, String)>;

struct Write {
    key: String,
    value: Option<String>,
}

/// One configuration file, as the preferences pages see it; see the module
/// documentation.
pub struct ConfigFile {
    pub scope: Scope,
    /// `None` until the first read has come back.
    entries: Option<Entries>,
    loading: Option<Receiver<Result<Entries, String>>>,
    writer: Option<Sender<Write>>,
    failures: Option<Receiver<String>>,
    /// The last read or write that failed, until [`ConfigFile::take_error`].
    error: Option<String>,
}

impl ConfigFile {
    /// Nothing is read until [`ConfigFile::load`].
    pub fn new(scope: Scope) -> Self {
        ConfigFile { scope, entries: None, loading: None, writer: None, failures: None, error: None }
    }

    /// Starts reading the file in the background, unless it was read already
    /// or is being read. Cheap: call it every frame from a page that needs
    /// values.
    pub fn load(&mut self, ctx: &egui::Context) {
        if self.entries.is_none() && self.loading.is_none() {
            self.reload(ctx);
        }
    }

    /// Reads the file again, for when something else may have changed it
    /// (a command the page ran, another program).
    pub fn reload(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let scope = self.scope.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read(&scope));
            ctx.request_repaint();
        });
        self.loading = Some(rx);
    }

    /// Collects finished reads and failed writes. Called once a frame by the
    /// preferences page.
    pub fn poll(&mut self) {
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(Ok(entries)) => {
                    self.entries = Some(entries);
                    self.loading = None;
                }
                Ok(Err(e)) => {
                    self.entries.get_or_insert_with(Vec::new);
                    self.error = Some(e);
                    self.loading = None;
                }
                Err(TryRecvError::Disconnected) => self.loading = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.failures {
            while let Ok(e) = rx.try_recv() {
                self.error = Some(e);
            }
        }
    }

    /// True once the first read is in, so the page can show values instead
    /// of a spinner.
    pub fn ready(&self) -> bool {
        self.entries.is_some()
    }

    /// The value git would use for `key` from this file (its last one).
    pub fn get(&self, key: &str) -> Option<&str> {
        let key = normalize(key);
        self.entries.as_ref()?.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| v.as_str())
    }

    /// A boolean the way git reads one; `None` when unset or not a boolean.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        parse_bool(self.get(key)?)
    }

    /// Sets `key` (`None` removes it). The value in memory changes now; the
    /// file is written in the background, and a failure comes back through
    /// [`ConfigFile::take_error`].
    pub fn set(&mut self, ctx: &egui::Context, key: &str, value: Option<&str>) {
        let normalized = normalize(key);
        let entries = self.entries.get_or_insert_with(Vec::new);
        entries.retain(|(k, _)| *k != normalized);
        if let Some(value) = value {
            entries.push((normalized, value.to_owned()));
        }
        let write = Write { key: key.to_owned(), value: value.map(str::to_owned) };
        let write = match &self.writer {
            Some(writer) => match writer.send(write) {
                Ok(()) => return,
                Err(failed) => failed.0,
            },
            None => write,
        };
        // The first write starts the writer thread; it ends when this
        // `ConfigFile` is dropped (preferences closed).
        let (tx, rx) = channel::<Write>();
        let (fail_tx, fail_rx) = channel();
        let scope = self.scope.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for Write { key, value } in rx {
                if let Err(e) = write_value(&scope, &key, value.as_deref()) {
                    let _ = fail_tx.send(e);
                    ctx.request_repaint();
                }
            }
        });
        let _ = tx.send(write);
        self.writer = Some(tx);
        self.failures = Some(fail_rx);
    }

    /// Sets a boolean the way git writes one.
    pub fn set_bool(&mut self, ctx: &egui::Context, key: &str, value: bool) {
        self.set(ctx, key, Some(if value { "true" } else { "false" }));
    }

    /// The last failure, once.
    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }
}

fn write_value(scope: &Scope, key: &str, value: Option<&str>) -> Result<(), String> {
    write(scope, key, value).map_err(|e| format!("Could not change {key}: {}", e.trim()))
}

/// Git's booleans: true/yes/on/1 and false/no/off/0/empty, any case.
pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// Reads `key` from the effective configuration of the repository at `dir`
/// (system, global and local, as git itself would), or from the user's when
/// `dir` is `None`. Blocks; for background code that needs one value.
pub fn effective(dir: Option<&Path>, key: &str) -> Option<String> {
    let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    let args = ["config".to_owned(), "--get".to_owned(), key.to_owned()];
    cli::run(dir.unwrap_or(&home), &args, cli::Options::default())
        .ok()
        .map(|out| out.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_normalise_like_git() {
        assert_eq!(normalize("Core.AutoCRLF"), "core.autocrlf");
        assert_eq!(normalize("branch.Main.Remote"), "branch.Main.remote");
        assert_eq!(normalize("url.HTTPS://x/.insteadOf"), "url.HTTPS://x/.insteadof");
        assert_eq!(normalize("Init"), "init");
    }

    #[test]
    fn list_output_parses_values_booleans_and_newlines() {
        let out = "user.name\nAda Lovelace\0core.bare\0alias.lg\nlog\n--graph\0";
        assert_eq!(
            parse_list(out),
            vec![
                ("user.name".to_owned(), "Ada Lovelace".to_owned()),
                ("core.bare".to_owned(), "true".to_owned()),
                ("alias.lg".to_owned(), "log\n--graph".to_owned()),
            ]
        );
    }

    #[test]
    fn values_round_trip_through_a_file() {
        let dir = std::env::temp_dir().join(format!("gitkrill-gitconfig-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let scope = Scope::File(dir.join("gitconfig"));
        let _ = std::fs::remove_file(dir.join("gitconfig"));

        assert_eq!(read(&scope).unwrap(), Vec::new());
        write(&scope, "Core.LongPaths", Some("true")).unwrap();
        write(&scope, "user.name", Some("Grace Hopper")).unwrap();
        write(&scope, "user.name", None).unwrap();
        write(&scope, "never.set", None).unwrap();

        let mut file = ConfigFile::new(scope);
        file.entries = Some(read(&file.scope).unwrap());
        assert_eq!(file.get_bool("core.longpaths"), Some(true));
        assert_eq!(file.get("user.name"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
