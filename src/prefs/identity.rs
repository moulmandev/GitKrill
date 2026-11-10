//! Profiles, SSH and commit signing (Preferences > Profiles, SSH, GPG): who
//! commits, with which key.
//!
//! Shown by `crate::ui::prefs::profiles`, `ssh` and `gpg`.
//!
//! A [`Profile`] is an author (name and email), an SSH key (or the system's
//! agent) and commit-signing choices; one of them is current. Until the user
//! first opens one of those pages there are no profiles, and nothing here
//! has any effect: git uses the system ssh and the user's own git config, as
//! it always did. The first visit makes "Default Profile" from the global
//! git config, so nothing changes then either.
//!
//! How the current profile reaches git ([`apply`]):
//!
//! - **SSH.** A profile that does not use the agent gives every git command
//!   GitKrill starts `GIT_SSH_COMMAND="ssh -i <key> -o IdentitiesOnly=yes"`
//!   ([`crate::git::cli::set_session_env`]).
//! - **Author and signing**, with "Keep my .gitconfig updated" on (the
//!   default): a change of profile, or of its author or signing settings,
//!   is written to the global git config (`user.name`, `user.email`,
//!   `user.signingkey`, `gpg.format`, `commit.gpgsign`, `tag.gpgSign`), so
//!   every git tool commits as that profile. With it off, the global config is left
//!   alone and only GitKrill's own git commands get the profile, through
//!   `GIT_AUTHOR_*` / `GIT_COMMITTER_*` and `GIT_CONFIG_COUNT` /
//!   `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>` (git 2.31+).
//!
//! Writes to the global config happen on a short-lived background thread,
//! in order, and only when something changed since the last call: starting
//! GitKrill never rewrites a file the user may have edited in the meantime.
//!
//! The helpers at the end (listing secret keys, generating keys, finding
//! `gpg` and `ssh-keygen`) block: the pages call them from background
//! threads.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::git::cli;
use crate::prefs::gitconfig::{self, Scope, normalize, parse_bool};
use crate::state::Settings;

/// Held by `Settings::identity`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Identity {
    /// Empty until Profiles, SSH or GPG is first opened.
    pub profiles: Vec<Profile>,
    /// Index of the profile in use.
    pub current: usize,
    /// "Keep my .gitconfig updated with my profile preferences".
    pub keep_gitconfig: bool,
}

impl Default for Identity {
    fn default() -> Self {
        Identity { profiles: Vec::new(), current: 0, keep_gitconfig: true }
    }
}

impl Identity {
    /// Index of the profile in use (`current`, kept in range).
    pub fn current_index(&self) -> usize {
        self.current.min(self.profiles.len().saturating_sub(1))
    }

    /// The profile in use; `None` before the first one is made.
    pub fn current(&self) -> Option<&Profile> {
        self.profiles.get(self.current_index())
    }

    pub fn current_mut(&mut self) -> Option<&mut Profile> {
        let index = self.current_index();
        self.profiles.get_mut(index)
    }

    /// Removes a profile, never the last one; the current one stays current
    /// (or the first becomes current if it was the one removed).
    pub fn remove(&mut self, index: usize) {
        if self.profiles.len() < 2 || index >= self.profiles.len() {
            return;
        }
        let current = self.current_index();
        self.profiles.remove(index);
        self.current = match current.cmp(&index) {
            std::cmp::Ordering::Greater => current - 1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Less => current,
        };
    }

    /// A profile name not taken yet: `base`, else `base 2`, `base 3`...
    pub fn free_name(&self, base: &str) -> String {
        let taken = |name: &str| self.profiles.iter().any(|p| p.name.eq_ignore_ascii_case(name));
        if !taken(base) {
            return base.to_owned();
        }
        (2..).map(|n| format!("{base} {n}")).find(|name| !taken(name)).unwrap_or_default()
    }
}

/// One identity to commit as.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub author_name: String,
    pub author_email: String,
    pub ssh: Ssh,
    pub signing: Signing,
}

impl Profile {
    /// A profile holding what a git configuration says (`get(key)`).
    pub fn from_config(name: &str, get: impl Fn(&str) -> Option<String>) -> Self {
        let text = |key: &str| get(key).unwrap_or_default().trim().to_owned();
        let flag = |key: &str| get(key).and_then(|v| parse_bool(&v)).unwrap_or(false);
        Profile {
            name: name.to_owned(),
            author_name: text("user.name"),
            author_email: text("user.email"),
            ssh: Ssh::default(),
            signing: Signing {
                format: SignFormat::parse(get("gpg.format").as_deref()),
                key: text("user.signingkey"),
                sign_commits: flag("commit.gpgsign"),
                sign_tags: flag("tag.gpgSign"),
            },
        }
    }

    /// What this profile decides in a git configuration.
    pub fn config(&self) -> ConfigValues {
        ConfigValues {
            name: self.author_name.trim().to_owned(),
            email: self.author_email.trim().to_owned(),
            signing: Signing { key: self.signing.key.trim().to_owned(), ..self.signing.clone() },
        }
    }
}

/// How git reaches remotes over SSH.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Ssh {
    /// The system's ssh with its agent and `~/.ssh/config` (git's own
    /// behaviour); otherwise the key below.
    pub use_agent: bool,
    pub private_key: String,
    pub public_key: String,
}

impl Default for Ssh {
    fn default() -> Self {
        Ssh { use_agent: true, private_key: String::new(), public_key: String::new() }
    }
}

/// Commit and tag signing.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Signing {
    /// `gpg.format`.
    pub format: SignFormat,
    /// `user.signingkey`: a key id for OpenPGP and X.509, a public key file
    /// for SSH. Empty: git picks the key matching the committer's email.
    pub key: String,
    /// `commit.gpgsign`.
    pub sign_commits: bool,
    /// `tag.gpgSign`.
    pub sign_tags: bool,
}

/// `gpg.format`.
#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum SignFormat {
    #[default]
    OpenPgp,
    Ssh,
    X509,
}

impl SignFormat {
    pub const ALL: [SignFormat; 3] = [SignFormat::OpenPgp, SignFormat::Ssh, SignFormat::X509];

    /// The value of `gpg.format`.
    pub fn config_value(self) -> &'static str {
        match self {
            SignFormat::OpenPgp => "openpgp",
            SignFormat::Ssh => "ssh",
            SignFormat::X509 => "x509",
        }
    }

    /// Reads `gpg.format`; unset (or unknown) is OpenPGP, as for git.
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("ssh") => SignFormat::Ssh,
            Some("x509") => SignFormat::X509,
            _ => SignFormat::OpenPgp,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SignFormat::OpenPgp => "OpenPGP",
            SignFormat::Ssh => "SSH",
            SignFormat::X509 => "X.509",
        }
    }

    /// The git setting naming the program that signs in this format.
    pub fn program_key(self) -> &'static str {
        match self {
            SignFormat::OpenPgp => "gpg.program",
            SignFormat::Ssh => "gpg.ssh.program",
            SignFormat::X509 => "gpg.x509.program",
        }
    }

    /// The program git runs when [`SignFormat::program_key`] is unset.
    pub fn default_program(self) -> &'static str {
        match self {
            SignFormat::OpenPgp => "gpg",
            SignFormat::Ssh => "ssh-keygen",
            SignFormat::X509 => "gpgsm",
        }
    }
}

/// The part of a profile that lives in a git configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigValues {
    pub name: String,
    pub email: String,
    pub signing: Signing,
}

/// The writes (`None` removes) that make a configuration whose values are
/// `current(key)` hold `wanted`. Values that already mean the same leave the
/// file alone, and so do git's defaults where nothing is set: a file that
/// never had `gpg.format` or `commit.gpgsign` does not get
/// `format = openpgp` or `gpgsign = false` lines.
pub fn config_changes(wanted: &ConfigValues, current: impl Fn(&str) -> Option<String>) -> Vec<(&'static str, Option<String>)> {
    let mut changes = Vec::new();
    for (key, value) in [("user.name", &wanted.name), ("user.email", &wanted.email), ("user.signingkey", &wanted.signing.key)] {
        let want = (!value.is_empty()).then(|| value.clone());
        if current(key).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) != want {
            changes.push((key, want));
        }
    }
    if SignFormat::parse(current("gpg.format").as_deref()) != wanted.signing.format {
        changes.push(("gpg.format", Some(wanted.signing.format.config_value().to_owned())));
    }
    for (key, value) in [("commit.gpgsign", wanted.signing.sign_commits), ("tag.gpgSign", wanted.signing.sign_tags)] {
        if current(key).and_then(|v| parse_bool(&v)).unwrap_or(false) != value {
            changes.push((key, Some(value.to_string())));
        }
    }
    changes
}

/// Makes the configuration file `scope` hold `wanted`. Blocks.
pub fn write_profile(scope: &Scope, wanted: &ConfigValues) -> Result<(), String> {
    let entries = gitconfig::read(scope)?;
    let get = |key: &str| {
        let key = normalize(key);
        entries.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
    };
    for (key, value) in config_changes(wanted, get) {
        write_retrying(scope, key, value.as_deref())?;
    }
    Ok(())
}

/// `git config` refuses to write while another `git config` holds the
/// file's lock (the preferences pages write on threads of their own): try
/// again for a second before giving up.
fn write_retrying(scope: &Scope, key: &str, value: Option<&str>) -> Result<(), String> {
    let mut attempt = 0;
    loop {
        match gitconfig::write(scope, key, value) {
            Err(e) if e.contains("could not lock") && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            result => return result.map_err(|e| format!("Could not change {key}: {}", e.trim())),
        }
    }
}

/// The environment every git command GitKrill starts gets for `identity`.
pub fn session_env(identity: &Identity) -> Vec<(String, String)> {
    let Some(profile) = identity.current() else { return Vec::new() };
    let mut env = Vec::new();
    let key = profile.ssh.private_key.trim();
    if !profile.ssh.use_agent && !key.is_empty() {
        env.push(("GIT_SSH_COMMAND".to_owned(), ssh_command(key)));
    }
    if !identity.keep_gitconfig {
        let values = profile.config();
        let mut config: Vec<(&str, String)> = Vec::new();
        for (who, value) in [("NAME", &values.name), ("EMAIL", &values.email)] {
            if !value.is_empty() {
                env.push((format!("GIT_AUTHOR_{who}"), value.clone()));
                env.push((format!("GIT_COMMITTER_{who}"), value.clone()));
                config.push((if who == "NAME" { "user.name" } else { "user.email" }, value.clone()));
            }
        }
        if !values.signing.key.is_empty() {
            config.push(("user.signingkey", values.signing.key.clone()));
        }
        config.push(("gpg.format", values.signing.format.config_value().to_owned()));
        config.push(("commit.gpgsign", values.signing.sign_commits.to_string()));
        config.push(("tag.gpgSign", values.signing.sign_tags.to_string()));
        env.push(("GIT_CONFIG_COUNT".to_owned(), config.len().to_string()));
        for (i, (key, value)) in config.into_iter().enumerate() {
            env.push((format!("GIT_CONFIG_KEY_{i}"), key.to_owned()));
            env.push((format!("GIT_CONFIG_VALUE_{i}"), value));
        }
    }
    env
}

/// `GIT_SSH_COMMAND` for a private key file. Git runs it through `sh`, so
/// the path is single-quoted for the shell; on Windows its backslashes
/// become forward slashes, which both Git for Windows' ssh and Windows'
/// own OpenSSH read as the same path.
pub fn ssh_command(key: &str) -> String {
    ssh_command_for(key, cfg!(windows))
}

fn ssh_command_for(key: &str, windows: bool) -> String {
    let key = if windows { key.replace('\\', "/") } else { key.to_owned() };
    format!("ssh -i {} -o IdentitiesOnly=yes", sh_quote(&key))
}

/// `value` as one word for a POSIX shell.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// What [`apply`] put into effect last.
#[derive(Clone, PartialEq)]
struct Applied {
    env: Vec<(String, String)>,
    /// What the global config should hold; `None` while it is left alone.
    global: Option<ConfigValues>,
}

static APPLIED: Mutex<Option<Applied>> = Mutex::new(None);

/// Counts changes of the identity git commands get, so views showing it
/// know when to ask again ([`generation`]).
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Changes whenever the identity GitKrill's git commands use may have
/// changed (another profile, a finished write to the global config).
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Puts these preferences into effect; see [`crate::prefs::apply`] and the
/// module documentation.
pub fn apply(ctx: &egui::Context, settings: &Settings) {
    let identity = &settings.identity;
    let next = Applied {
        env: session_env(identity),
        global: identity.keep_gitconfig.then(|| identity.current().map(Profile::config)).flatten(),
    };
    let Ok(mut applied) = APPLIED.lock() else { return };
    if applied.as_ref() == Some(&next) {
        return;
    }
    let previous = applied.replace(next.clone());
    drop(applied);
    if previous.as_ref().map(|p| &p.env) != Some(&next.env) {
        cli::set_session_env(next.env);
    }
    // The first call is GitKrill starting: the global config may have been
    // edited since, and is left as it is.
    if let Some(previous) = previous
        && previous.global != next.global
        && let Some(values) = next.global
    {
        queue_write(ctx, |pending| pending.profile = Some(values));
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Global config writes not started yet. Each write thread takes everything
/// pending, so a burst of changes ends with the newest values written last.
struct Pending {
    profile: Option<ConfigValues>,
    keys: Vec<(String, Option<String>)>,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending { profile: None, keys: Vec::new() });
/// Held while a thread writes, so writes land in the order they were asked.
static WRITING: Mutex<()> = Mutex::new(());
static FAILURE: Mutex<Option<String>> = Mutex::new(None);

fn queue_write(ctx: &egui::Context, add: impl FnOnce(&mut Pending)) {
    if let Ok(mut pending) = PENDING.lock() {
        add(&mut pending);
    }
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let Ok(_writing) = WRITING.lock() else { return };
        let Some(Pending { profile, keys }) =
            PENDING.lock().ok().map(|mut p| std::mem::replace(&mut *p, Pending { profile: None, keys: Vec::new() }))
        else {
            return;
        };
        if profile.is_none() && keys.is_empty() {
            return;
        }
        let scope = Scope::Global;
        let mut failure = None;
        for (key, value) in keys {
            if let Err(e) = write_retrying(&scope, &key, value.as_deref()) {
                failure = Some(e);
            }
        }
        if let Some(values) = profile
            && let Err(e) = write_profile(&scope, &values)
        {
            failure = Some(e);
        }
        if let Some(e) = failure
            && let Ok(mut slot) = FAILURE.lock()
        {
            *slot = Some(e);
        }
        GENERATION.fetch_add(1, Ordering::Relaxed);
        ctx.request_repaint();
    });
}

/// Writes one global git setting (`None` removes it) on the same ordered
/// background writer as profile changes, for settings shown on these pages
/// that are not part of a profile (`gpg.program`).
pub fn set_global(ctx: &egui::Context, key: &str, value: Option<&str>) {
    let write = (key.to_owned(), value.map(str::to_owned));
    queue_write(ctx, |pending| pending.keys.push(write));
}

/// The last failed write to the global config, once.
pub fn take_failure() -> Option<String> {
    FAILURE.lock().ok()?.take()
}

/// The profile in use, for the preferences sidebar ("Current profile");
/// `None` hides that part of the sidebar.
pub fn current_profile_name(identity: &Identity) -> Option<&str> {
    identity.current().map(|p| p.name.as_str())
}

// ---- programs ---------------------------------------------------------------

/// The program to run for `configured` (a `gpg.program` value, or the
/// default name when unset). A bare name is looked for the way git finds
/// it: on Windows in Git for Windows' own `usr/bin` first (git puts it
/// first on the PATH of what it runs), then on PATH. Blocks.
pub fn find_program(configured: &str) -> Option<PathBuf> {
    let configured = configured.trim();
    if configured.is_empty() {
        return None;
    }
    let path = Path::new(configured);
    if path.components().count() > 1 || path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    let names: Vec<String> = if cfg!(windows) && path.extension().is_none() {
        vec![format!("{configured}.exe"), configured.to_owned()]
    } else {
        vec![configured.to_owned()]
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if cfg!(windows)
        && let Some(root) = git_root()
    {
        dirs.push(root.join("usr").join("bin"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.iter().flat_map(|dir| names.iter().map(move |name| dir.join(name))).find(|candidate| candidate.is_file())
}

/// Where Git for Windows is installed (`C:\Program Files\Git`), from `git
/// --exec-path` (`<root>/mingw64/libexec/git-core`). Asked once.
fn git_root() -> Option<PathBuf> {
    static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
        let out = cli::run(&home, &["--exec-path".to_owned()], cli::Options::default()).ok()?;
        let exec = PathBuf::from(out.lines().next()?.trim());
        let root = exec.parent()?.parent()?.parent()?.to_path_buf();
        root.join("usr").join("bin").is_dir().then_some(root)
    })
    .clone()
}

/// Runs a signing tool without a console window or a prompt. Blocks.
fn run_tool(program: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<String, String> {
    use std::io::Write as _;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cli::hide_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("Could not run {}: {e}", program.display()))?;
    if let Some(bytes) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let _ = pipe.write_all(bytes);
    }
    let out = child.wait_with_output().map_err(|e| format!("{} failed: {e}", program.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if stderr.is_empty() { format!("{} failed ({})", program.display(), out.status) } else { stderr })
    }
}

// ---- GPG keys ---------------------------------------------------------------

/// A secret key that can sign, from `gpg --list-secret-keys`.
#[derive(Clone, Debug, PartialEq)]
pub struct SecretKey {
    /// Long key id (16 hex digits): what `user.signingkey` gets.
    pub id: String,
    pub fingerprint: String,
    /// The first user id ("Name <email>").
    pub user_id: String,
    /// Seconds since 1970, when the key expires.
    pub expires: Option<i64>,
    pub expired: bool,
    pub revoked: bool,
}

impl SecretKey {
    /// Whether `value` (a `user.signingkey`) names this key: a key id, short
    /// or long, or the fingerprint, any case, with an optional `0x` and `!`.
    pub fn matches(&self, value: &str) -> bool {
        let value = value.trim().trim_end_matches('!');
        let value = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(value);
        value.len() >= 8
            && value.chars().all(|c| c.is_ascii_hexdigit())
            && (self.fingerprint.to_ascii_uppercase().ends_with(&value.to_ascii_uppercase())
                || self.id.eq_ignore_ascii_case(value))
    }
}

/// The signing keys in `gpg --list-secret-keys --with-colons` (or `gpgsm`'s,
/// whose secret certificates are `crs` records) output. `now` decides which
/// have expired.
pub fn parse_secret_keys(out: &str, now: i64) -> Vec<SecretKey> {
    let mut keys: Vec<SecretKey> = Vec::new();
    // Whether the record before was the key itself (its fingerprint comes
    // next), and whether the key being read can sign.
    let mut primary = false;
    let mut usable = false;
    for line in out.lines() {
        let fields: Vec<&str> = line.trim_end_matches('\r').split(':').collect();
        let field = |i: usize| fields.get(i).copied().unwrap_or("");
        match field(0) {
            "sec" | "crs" => {
                let validity = field(1);
                let expires = field(6).parse::<i64>().ok().filter(|t| *t > 0);
                // Capabilities of the whole key: an upper-case S means it
                // (or a subkey) can sign. Disabled keys carry a D.
                let caps = field(11);
                usable = (caps.is_empty() || caps.contains(['S', 's'])) && !caps.contains('D');
                primary = true;
                if usable {
                    keys.push(SecretKey {
                        id: field(4).to_ascii_uppercase(),
                        fingerprint: String::new(),
                        user_id: String::new(),
                        expires,
                        expired: validity == "e" || expires.is_some_and(|t| t <= now),
                        revoked: validity == "r",
                    });
                }
            }
            "fpr" if primary => {
                primary = false;
                if usable && let Some(key) = keys.last_mut() {
                    key.fingerprint = field(9).to_ascii_uppercase();
                }
            }
            "uid" if usable => {
                if let Some(key) = keys.last_mut()
                    && key.user_id.is_empty()
                {
                    key.user_id = unescape_colons(field(9));
                }
            }
            "ssb" | "ssr" => primary = false,
            _ => {}
        }
    }
    keys
}

/// Colon listings escape special characters as `\xHH`.
fn unescape_colons(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && bytes.get(i + 1) == Some(&b'x')
            && let Some(hex) = value.get(i + 2..i + 4)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The program for `format` given its configured value (empty: git's
/// default), or a message saying it is missing. Blocks.
pub fn signing_program(format: SignFormat, configured: &str) -> Result<PathBuf, String> {
    let name = if configured.trim().is_empty() { format.default_program() } else { configured.trim() };
    find_program(name).ok_or_else(|| match format {
        SignFormat::OpenPgp => format!("{name} was not found. Install GnuPG, or set the GPG program."),
        SignFormat::X509 => format!("{name} was not found. Install GnuPG (it comes with gpgsm), or set the program."),
        SignFormat::Ssh => format!("{name} was not found. Install OpenSSH, or set the program."),
    })
}

/// The secret keys that can sign, for OpenPGP or X.509. Blocks.
pub fn list_secret_keys(format: SignFormat, configured_program: &str) -> Result<Vec<SecretKey>, String> {
    let program = signing_program(format, configured_program)?;
    let out = run_tool(&program, &["--batch", "--list-secret-keys", "--with-colons"], None)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    Ok(parse_secret_keys(&out, now))
}

/// The parameters of `gpg --batch --gen-key` for a 4096-bit RSA key that
/// expires in two years.
fn gen_key_params(name: &str, email: &str, passphrase: &str) -> String {
    // A line break would start another parameter.
    let clean = |value: &str| value.replace(['\r', '\n'], " ").trim().to_owned();
    let protection = if passphrase.is_empty() {
        "%no-protection".to_owned()
    } else {
        format!("Passphrase: {}", clean(passphrase))
    };
    format!(
        "Key-Type: RSA\nKey-Length: 4096\nKey-Usage: sign\nSubkey-Type: RSA\nSubkey-Length: 4096\nSubkey-Usage: encrypt\n\
         Name-Real: {}\nName-Email: {}\nExpire-Date: 2y\n{protection}\n%commit\n",
        clean(name),
        clean(email)
    )
}

/// Makes a new OpenPGP key for `name <email>` and returns its long key id.
/// Blocks (several seconds).
pub fn generate_gpg_key(configured_program: &str, name: &str, email: &str, passphrase: &str) -> Result<String, String> {
    if name.trim().is_empty() || email.trim().is_empty() {
        return Err("The current profile needs an author name and email first.".to_owned());
    }
    let program = signing_program(SignFormat::OpenPgp, configured_program)?;
    let params = gen_key_params(name, email, passphrase);
    let out = run_tool(
        &program,
        &["--batch", "--status-fd", "1", "--pinentry-mode", "loopback", "--gen-key"],
        Some(params.as_bytes()),
    )?;
    // [GNUPG:] KEY_CREATED B <fingerprint>
    out.lines()
        .find_map(|line| line.strip_prefix("[GNUPG:] KEY_CREATED "))
        .and_then(|rest| rest.split_whitespace().nth(1))
        .map(|fpr| fpr[fpr.len().saturating_sub(16)..].to_ascii_uppercase())
        .ok_or_else(|| "gpg did not say which key it made.".to_owned())
}

// ---- SSH keys ---------------------------------------------------------------

/// Where new SSH keys go: `~/.ssh`, or `<GITKRILL_CONFIG_DIR>/ssh` in tests
/// and scripted runs, so they never add keys to the user's own folder.
pub fn ssh_dir() -> PathBuf {
    match std::env::var_os("GITKRILL_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir).join("ssh"),
        None => dirs::home_dir().unwrap_or_else(std::env::temp_dir).join(".ssh"),
    }
}

/// A key file name in `dir` that is free along with its `.pub`:
/// `id_ed25519_gitkrill`, else `id_ed25519_gitkrill_2`...
fn free_key_path(dir: &Path) -> PathBuf {
    let free = |path: &PathBuf| !path.exists() && !path.with_extension("pub").exists();
    std::iter::once(dir.join("id_ed25519_gitkrill"))
        .chain((2..).map(|n| dir.join(format!("id_ed25519_gitkrill_{n}"))))
        .find(free)
        .unwrap_or_else(|| dir.join("id_ed25519_gitkrill"))
}

/// Makes a new ed25519 key pair without a passphrase in `dir`, never over
/// an existing file. Returns the private and public key paths. Blocks.
pub fn generate_ssh_key(dir: &Path, email: &str) -> Result<(PathBuf, PathBuf), String> {
    let program = find_program("ssh-keygen").ok_or("ssh-keygen was not found. Install OpenSSH (it comes with Git).")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    let private = free_key_path(dir);
    let comment = email.replace(['\r', '\n'], " ");
    let path = private.display().to_string();
    run_tool(&program, &["-q", "-t", "ed25519", "-N", "", "-C", comment.trim(), "-f", &path], None)?;
    let public = private.with_extension("pub");
    if !private.is_file() || !public.is_file() {
        return Err(format!("ssh-keygen did not create {path}"));
    }
    Ok((private, public))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("identity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn profile(name: &str, author: &str, email: &str) -> Profile {
        Profile { name: name.to_owned(), author_name: author.to_owned(), author_email: email.to_owned(), ..Default::default() }
    }

    fn read_file(scope: &Scope) -> Vec<(String, String)> {
        gitconfig::read(scope).unwrap()
    }

    #[test]
    fn switching_profiles_rewrites_only_what_differs() {
        let dir = temp("switch");
        let file = dir.join("gitconfig");
        std::fs::write(&file, "[user]\n\tname = Ada\n\temail = ada@example.com\n[core]\n\tautocrlf = false\n").unwrap();
        let scope = Scope::File(file.clone());

        // The default profile is made from the file, so writing it back
        // changes nothing.
        let entries = read_file(&scope);
        let get = |key: &str| entries.iter().rev().find(|(k, _)| *k == normalize(key)).map(|(_, v)| v.clone());
        let default = Profile::from_config("Default Profile", get);
        assert_eq!(default.author_name, "Ada");
        assert_eq!(config_changes(&default.config(), get), Vec::new());
        let before = std::fs::read_to_string(&file).unwrap();
        write_profile(&scope, &default.config()).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);

        let mut work = profile("Work", "Ada Lovelace", "ada@work.example");
        work.signing = Signing { format: SignFormat::Ssh, key: "C:/keys/id.pub".to_owned(), sign_commits: true, sign_tags: true };
        write_profile(&scope, &work.config()).unwrap();
        let file_now = ConfigFileView(read_file(&scope));
        assert_eq!(file_now.get("user.name"), Some("Ada Lovelace"));
        assert_eq!(file_now.get("user.email"), Some("ada@work.example"));
        assert_eq!(file_now.get("user.signingkey"), Some("C:/keys/id.pub"));
        assert_eq!(file_now.get("gpg.format"), Some("ssh"));
        assert_eq!(file_now.get("commit.gpgsign"), Some("true"));
        assert_eq!(file_now.get("tag.gpgsign"), Some("true"));
        assert_eq!(file_now.get("core.autocrlf"), Some("false"), "other settings stay");

        // Back to the default: the key goes, format and signing say so.
        write_profile(&scope, &default.config()).unwrap();
        let file_now = ConfigFileView(read_file(&scope));
        assert_eq!(file_now.get("user.name"), Some("Ada"));
        assert_eq!(file_now.get("user.email"), Some("ada@example.com"));
        assert_eq!(file_now.get("user.signingkey"), None);
        assert_eq!(file_now.get("gpg.format"), Some("openpgp"));
        assert_eq!(file_now.get("commit.gpgsign"), Some("false"));
        assert_eq!(file_now.get("tag.gpgsign"), Some("false"));
        assert_eq!(file_now.get("core.autocrlf"), Some("false"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct ConfigFileView(Vec<(String, String)>);
    impl ConfigFileView {
        fn get(&self, key: &str) -> Option<&str> {
            let key = normalize(key);
            self.0.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| v.as_str())
        }
    }

    #[test]
    fn defaults_that_are_not_set_stay_unset() {
        let wanted = profile("P", "Ada", "ada@example.com").config();
        let changes = config_changes(&wanted, |_| None);
        assert_eq!(
            changes,
            vec![("user.name", Some("Ada".to_owned())), ("user.email", Some("ada@example.com".to_owned()))]
        );
        // "yes" already means true; "OpenPGP" already means openpgp.
        let mut signing = wanted.clone();
        signing.signing.sign_commits = true;
        let set = |key: &str| match key {
            "user.name" => Some("Ada".to_owned()),
            "user.email" => Some("ada@example.com".to_owned()),
            "commit.gpgsign" => Some("yes".to_owned()),
            "gpg.format" => Some("OpenPGP".to_owned()),
            _ => None,
        };
        assert_eq!(config_changes(&signing, set), Vec::new());
    }

    #[test]
    fn session_env_with_and_without_the_gitconfig() {
        let mut identity = Identity::default();
        assert_eq!(session_env(&identity), Vec::new(), "no profile: nothing changes");

        let mut work = profile("Work", "Ada Lovelace", "ada@work.example");
        work.signing = Signing { format: SignFormat::OpenPgp, key: "ABCDEF0123456789".to_owned(), sign_commits: true, sign_tags: false };
        identity.profiles = vec![profile("Default Profile", "Ada", "ada@example.com"), work];
        identity.current = 1;
        assert_eq!(session_env(&identity), Vec::new(), "the gitconfig holds the profile, the agent does SSH");

        identity.profiles[1].ssh = Ssh { use_agent: false, private_key: "/home/ada/.ssh/work".to_owned(), public_key: String::new() };
        assert_eq!(
            session_env(&identity),
            vec![("GIT_SSH_COMMAND".to_owned(), ssh_command("/home/ada/.ssh/work"))]
        );

        identity.keep_gitconfig = false;
        let env = session_env(&identity);
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("GIT_AUTHOR_NAME"), Some("Ada Lovelace"));
        assert_eq!(get("GIT_AUTHOR_EMAIL"), Some("ada@work.example"));
        assert_eq!(get("GIT_COMMITTER_NAME"), Some("Ada Lovelace"));
        assert_eq!(get("GIT_COMMITTER_EMAIL"), Some("ada@work.example"));
        assert_eq!(get("GIT_CONFIG_COUNT"), Some("6"));
        let config: Vec<(&str, &str)> = (0..6)
            .map(|i| (get(&format!("GIT_CONFIG_KEY_{i}")).unwrap(), get(&format!("GIT_CONFIG_VALUE_{i}")).unwrap()))
            .collect();
        assert_eq!(
            config,
            vec![
                ("user.name", "Ada Lovelace"),
                ("user.email", "ada@work.example"),
                ("user.signingkey", "ABCDEF0123456789"),
                ("gpg.format", "openpgp"),
                ("commit.gpgsign", "true"),
                ("tag.gpgSign", "false"),
            ]
        );
        assert!(get("GIT_SSH_COMMAND").is_some());
    }

    #[test]
    fn ssh_command_quotes_paths_for_the_shell() {
        assert_eq!(
            ssh_command_for(r"C:\Users\Ada Lovelace\.ssh\id_ed25519", true),
            "ssh -i 'C:/Users/Ada Lovelace/.ssh/id_ed25519' -o IdentitiesOnly=yes"
        );
        assert_eq!(ssh_command_for("/home/o'brien/.ssh/k", false), r"ssh -i '/home/o'\''brien/.ssh/k' -o IdentitiesOnly=yes");
        // Elsewhere a backslash is part of a file name.
        assert_eq!(ssh_command_for(r"/tmp/a\b", false), r"ssh -i '/tmp/a\b' -o IdentitiesOnly=yes");
    }

    /// Git runs `GIT_SSH_COMMAND` through its shell exactly as it runs a `!`
    /// alias: ask ssh, run that way, which key file it would use.
    #[test]
    fn ssh_reads_the_quoted_key_path() {
        let dir = temp("ssh path");
        let key = dir.join("my key's file");
        // ssh ignores an -i file that does not exist.
        std::fs::write(&key, "").unwrap();
        let probe = format!("alias.sshprobe=!{} -G example.invalid", ssh_command(&key.display().to_string()));
        let out = match cli::run(&dir, &["-c".to_owned(), probe, "sshprobe".to_owned()], cli::Options::default()) {
            Ok(out) => out,
            // No ssh on this machine.
            Err(e) if e.contains("not found") || e.contains("not recognized") => return,
            Err(e) => panic!("ssh -G failed: {e}"),
        };
        let expected = key.display().to_string().replace('\\', "/");
        let files: Vec<&str> = out.lines().filter_map(|line| line.strip_prefix("identityfile ")).collect();
        assert!(
            files.iter().any(|file| file.replace('\\', "/").eq_ignore_ascii_case(&expected)),
            "ssh would use {files:?}, not {expected:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_keys_parse_from_colon_listings() {
        let out = "\
sec:u:4096:1:3AA5C34371567BD2:1700000000:1900000000::u:::scESC:::+:::23::0:
fpr:::::::::4AEE18F83AFDEB23C8D8E0E43AA5C34371567BD2:
grp:::::::::0123456789ABCDEF0123456789ABCDEF01234567:
uid:u::::1700000000::HASH::Ada Lovelace (work\\x3a laptop) <ada@example.com>::::::::::0:
uid:u::::1700000000::HASH::Second <second@example.com>::::::::::0:
ssb:u:4096:1:42B317FD4BA89E7A:1700000000:1900000000:::::e:::+:::23:
fpr:::::::::0000000000000000000000000000000042B317FD4BA89E7A:
sec:e:2048:1:1111222233334444:1500000000:1600000000::u:::scSC:::+:::23::0:
fpr:::::::::AAAABBBBCCCCDDDDEEEEFFFF1111222233334444:
uid:e::::1500000000::HASH::Old <old@example.com>::::::::::0:
sec:u:2048:1:5555666677778888:1500000000:::u:::eE:::+:::23::0:
fpr:::::::::AAAABBBBCCCCDDDDEEEEFFFF5555666677778888:
uid:u::::1500000000::HASH::Encrypt only <enc@example.com>::::::::::0:
";
        let keys = parse_secret_keys(out, 1_800_000_000);
        assert_eq!(keys.len(), 2, "the encrypt-only key cannot sign");
        assert_eq!(keys[0].id, "3AA5C34371567BD2");
        assert_eq!(keys[0].fingerprint, "4AEE18F83AFDEB23C8D8E0E43AA5C34371567BD2");
        assert_eq!(keys[0].user_id, "Ada Lovelace (work: laptop) <ada@example.com>");
        assert_eq!(keys[0].expires, Some(1_900_000_000));
        assert!(!keys[0].expired && !keys[0].revoked);
        assert!(keys[1].expired);
        assert_eq!(keys[1].user_id, "Old <old@example.com>");

        assert!(keys[0].matches("3aa5c34371567bd2"));
        assert!(keys[0].matches("0x71567BD2"));
        assert!(keys[0].matches("4AEE18F83AFDEB23C8D8E0E43AA5C34371567BD2!"));
        assert!(!keys[0].matches("ada@example.com"));
        assert!(!keys[0].matches("42B317FD4BA89E7A"), "a subkey's id is not the key's");

        // A key that has passed its expiry date but whose validity was
        // computed before then.
        assert!(parse_secret_keys(out, 1_950_000_000)[0].expired);
    }

    #[test]
    fn generated_key_parameters_cannot_be_smuggled() {
        let params = gen_key_params("Ada\nName-Email: evil@example.com", "ada@example.com", "");
        assert!(params.contains("Name-Real: Ada Name-Email: evil@example.com\n"));
        assert!(params.contains("%no-protection\n"));
        assert!(gen_key_params("Ada", "a@b", "secret").contains("Passphrase: secret\n"));
    }

    #[test]
    fn profiles_switch_and_remove_keep_current_sensible() {
        let mut identity = Identity {
            profiles: vec![profile("A", "a", "a@x"), profile("B", "b", "b@x"), profile("C", "c", "c@x")],
            current: 2,
            ..Default::default()
        };
        identity.remove(0);
        assert_eq!(current_profile_name(&identity), Some("C"));
        identity.remove(1);
        assert_eq!(current_profile_name(&identity), Some("B"));
        identity.remove(0);
        assert_eq!(identity.profiles.len(), 1, "the last profile stays");
        assert_eq!(identity.free_name("B"), "B 2");
        assert_eq!(identity.free_name("Work"), "Work");
    }

    /// Commits signed through the session environment alone (the profile
    /// with "Keep my .gitconfig updated" off), with an SSH key, carry a
    /// signature; picking commits keeps their author.
    #[test]
    fn commits_made_with_the_session_env_are_signed_and_authored() {
        let Some(_) = find_program("ssh-keygen") else { return };
        let dir = temp("sign");
        let (_, public) = generate_ssh_key(&dir.join("keys"), "work@example.com").unwrap();
        assert!(public.ends_with("id_ed25519_gitkrill.pub"));
        let (second, _) = generate_ssh_key(&dir.join("keys"), "work@example.com").unwrap();
        assert!(second.ends_with("id_ed25519_gitkrill_2"), "never over an existing key");

        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        crate::tests::git(&repo, &["init", "-q", "-b", "main"]);
        crate::tests::git(&repo, &["config", "user.name", "Local User"]);
        crate::tests::git(&repo, &["config", "user.email", "local@example.com"]);
        crate::tests::git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        crate::tests::git(&repo, &["add", "a.txt"]);
        crate::tests::git(&repo, &["commit", "-q", "-m", "by local"]);
        crate::tests::git(&repo, &["checkout", "-q", "-b", "side"]);
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        crate::tests::git(&repo, &["add", "b.txt"]);
        crate::tests::git(&repo, &["commit", "-q", "-m", "picked"]);
        crate::tests::git(&repo, &["checkout", "-q", "main"]);

        let mut work = profile("Work", "Work Person", "work@example.com");
        work.signing = Signing { format: SignFormat::Ssh, key: public.display().to_string(), sign_commits: true, sign_tags: false };
        let identity = Identity { profiles: vec![work], current: 0, keep_gitconfig: false };
        let env = session_env(&identity);
        let run = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            cli::run(&repo, &args, cli::Options { env: &env, ..Default::default() }).unwrap()
        };

        std::fs::write(repo.join("c.txt"), "c\n").unwrap();
        run(&["add", "c.txt"]);
        run(&["commit", "-q", "-m", "by work"]);
        let head = run(&["cat-file", "-p", "HEAD"]);
        assert!(head.contains("\ngpgsig -----BEGIN SSH SIGNATURE-----"), "{head}");
        assert!(head.contains("\nauthor Work Person <work@example.com>"), "{head}");
        assert!(head.contains("\ncommitter Work Person <work@example.com>"), "{head}");

        run(&["cherry-pick", "side"]);
        let picked = run(&["cat-file", "-p", "HEAD"]);
        assert!(picked.contains("\nauthor Local User <local@example.com>"), "{picked}");
        assert!(picked.contains("\ncommitter Work Person <work@example.com>"), "{picked}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
