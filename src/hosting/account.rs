//! Accounts on the hosting services, for Preferences > Integrations: which
//! sign-in GitKrill finds for a host, the account it belongs to, and that
//! account's SSH keys (listing them, adding one from a file, generating a
//! new one and adding it).
//!
//! Bitbucket keeps an account's keys under its id (`/users/{uuid}/ssh-keys`),
//! and Azure DevOps has no API for them at all: there only the profile is
//! read, outside any organisation ([`azure::PROFILE_API`]).
//!
//! Everything here that blocks (the network, `gh` / `glab` / `tea` / `az` /
//! `git credential`, `ssh-keygen`, reading `~/.ssh`) runs on an
//! [`AccountSession`]'s own thread. The thread lives while the preferences
//! page holds the session and ends once it is dropped; the UI thread only
//! sends it orders and drains its events.
//!
//! Tokens are found exactly as for the sidebar ([`find_token`], asked about
//! the account's host only) and only ever attached by a [`Wire`] to
//! requests below that host's API. GitKrill does not own them (git pushes
//! with the same credential), so nothing here stores, erases or shows one:
//! "Disconnect" is a setting ([`Disconnected`]) under which GitKrill stops
//! using the sign-in, and the sidebar reads as signed out.
//!
//! The offline fixture (`GITKRILL_HOSTING_FIXTURE=<dir>`) answers
//! `GET /user` from `user.json` and `GET /user/keys` from `keys.json`
//! (`keys.after.json` once a key was added); an upload is answered from
//! `keys.post.json` (201 unless `keys.post.head` says otherwise) and the
//! body sent is left in `keys.sent.json`. Generated keys go to `<dir>/ssh`.
//!
//! [`Disconnected`]: super::Disconnected

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use serde::Deserialize;

use super::{
    Canned, FixtureSignIn, HttpRequest, HttpResponse, NoSecrets, Offer, Rate, RepoId, Secrets, Service, System,
    Token, TokenSource, Transport, Wire, api_base, azure, bitbucket, clock, epoch_now, find_token, gitlab, headers,
    limited_until, offer_for, public, timestamp, tidy, unsupported, valid_host,
};
use crate::git::RemoteInfo;

// ---- which account ----------------------------------------------------------

/// One account GitKrill may sign in to: a service on a host. Tokens are
/// found per host, so the host is what tells accounts apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountId {
    pub service: Service,
    /// `github.com`, `gitlab.com`, or a server of `settings.hosting_servers`,
    /// written as [`RepoId::host`] is.
    pub host: String,
}

impl AccountId {
    pub fn github() -> Self {
        AccountId { service: Service::GitHub, host: "github.com".to_owned() }
    }

    pub fn gitlab() -> Self {
        AccountId { service: Service::GitLab, host: "gitlab.com".to_owned() }
    }

    pub fn bitbucket() -> Self {
        AccountId { service: Service::Bitbucket, host: "bitbucket.org".to_owned() }
    }

    pub fn codeberg() -> Self {
        AccountId { service: Service::Codeberg, host: "codeberg.org".to_owned() }
    }

    pub fn azure() -> Self {
        AccountId { service: Service::AzureDevOps, host: azure::HOST.to_owned() }
    }

    /// On a server the user named, rather than a public service.
    pub fn self_hosted(&self) -> bool {
        public(&self.host).is_none()
    }

    /// "GitHub", "GitLab", "Bitbucket"..., or the server's name.
    pub fn site(&self) -> String {
        if self.self_hosted() { self.host.clone() } else { self.service.name().to_owned() }
    }

    /// Where the REST API lives; a token is only ever sent below it. Azure
    /// DevOps keeps the user's profile outside every organisation.
    pub fn api(&self) -> String {
        match self.service {
            Service::AzureDevOps => azure::PROFILE_API.to_owned(),
            service => api_base(service, &self.host),
        }
    }

    /// Whether the service lets its SSH keys be listed and added through its
    /// API. Azure DevOps does not.
    pub fn keys_supported(&self) -> bool {
        self.service != Service::AzureDevOps
    }

    /// What [`find_token`] is asked about: the service and the host, nothing
    /// else of a repository.
    fn repo(&self) -> RepoId {
        RepoId { service: self.service, host: self.host.clone(), owner: String::new(), name: String::new() }
    }
}

/// The host as `settings.hosting_servers` writes it, from what someone typed
/// to add a server: `git.example.com`, `https://git.example.com/`, or
/// `git.example.com:8443`. github.com, gitlab.com and services GitKrill
/// cannot talk to are turned down with the reason.
pub fn server_host(text: &str) -> Result<String, String> {
    let text = text.trim();
    let (https, rest) = match text.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("https") => (true, rest),
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => (false, rest),
        Some(_) => return Err("Type the server's name, such as git.example.com".to_owned()),
        None => (true, text),
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let (name, port) = match authority.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (authority, None),
    };
    let name = name.to_ascii_lowercase();
    if name.is_empty() {
        return Err("Type the server's name, such as git.example.com".to_owned());
    }
    if !valid_host(&name) || !name.contains('.') && name != "localhost" {
        return Err(format!("\"{}\" is not a server name", tidy(&name)));
    }
    let port = match port {
        None | Some("") => None,
        Some(port) => Some(port.parse::<u16>().map_err(|_| format!("\"{}\" is not a port", tidy(port)))?),
    };
    if let Some((service, _)) = public(&name) {
        return Err(format!("{name} is {} itself: it is listed above", service.name()));
    }
    if unsupported(&name) {
        return Err(format!("GitKrill cannot show accounts on {name}"));
    }
    // As remotes are read: the port only counts for HTTPS, and 443 is the
    // default anyway.
    Ok(match port.filter(|port| https && *port != 443) {
        Some(port) => format!("{name}:{port}"),
        None => name,
    })
}

/// `host` (from [`server_host`]) with the path a server is installed under
/// when it is not at the root, as typed (`gitlab`, `/gitlab/`,
/// `tools/gitlab`): `example.com/gitlab`, as `settings.hosting_servers` and
/// [`RepoId::host`] write it. An empty path is the host alone.
pub fn under_path(host: &str, path: &str) -> Result<String, String> {
    let path = path.trim().trim_matches('/');
    if path.is_empty() {
        return Ok(host.to_owned());
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > 8 || !parts.iter().all(|part| super::path_part(part)) {
        return Err(format!("\"{}\" is not a path GitLab can be installed under", tidy(path)));
    }
    Ok(format!("{host}/{}", parts.join("/")))
}

/// Self-hosted servers the remotes of the repositories at `paths` point at,
/// one per host in the order first seen, whether or not the user has said
/// what they run: what Preferences > Integrations offers to connect. Opens
/// each repository to read its remotes, so it belongs on a background
/// thread.
pub fn found_servers(paths: &[PathBuf]) -> Vec<Offer> {
    let mut found: Vec<Offer> = Vec::new();
    for path in paths {
        let Ok(repo) = git2::Repository::open(path) else { continue };
        let Ok(names) = repo.remotes() else { continue };
        for name in names.iter_bytes() {
            let name = String::from_utf8_lossy(name).into_owned();
            let Ok(remote) = repo.find_remote(&name) else { continue };
            let url = String::from_utf8_lossy(remote.url_bytes()).into_owned();
            if let Some(offer) = offer_for(&RemoteInfo { name, url })
                && !found.iter().any(|seen| seen.host == offer.host)
            {
                found.push(offer);
            }
        }
    }
    found
}

// ---- the account and its keys -----------------------------------------------

/// The account a token belongs to (`GET /user`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub id: u64,
    pub login: String,
    /// The display name; empty when the account has none.
    pub name: String,
    /// An address the account shows (its public e-mail), for its picture.
    pub email: Option<String>,
}

impl Profile {
    /// The name to show: the display name, or the login when there is none.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() { &self.login } else { &self.name }
    }

    /// The address the avatar cache finds this account's picture by:
    /// github.com accounts by their numeric id (the cache turns
    /// `<id>+<login>@users.noreply.github.com` into the GitHub picture),
    /// others by their public address on Gravatar. `None`: initials.
    pub fn avatar_email(&self, account: &AccountId) -> Option<String> {
        if account.service == Service::GitHub && self.id > 0 {
            return Some(format!("{}+{}@users.noreply.github.com", self.id, self.login));
        }
        self.email.clone()
    }
}

/// An answer to `GET /user`: GitHub's (`login`, public `email`), GitLab's
/// (`username`, `public_email`, and the account's own `email`, which only
/// its owner is shown), Gitea's and Forgejo's (`login`, `full_name`,
/// `email`); Bitbucket's and Azure DevOps' profiles have shapes of their own.
pub fn parse_user(service: Service, json: &str) -> Result<Profile, String> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        id: u64,
        #[serde(default)]
        login: String,
        #[serde(default)]
        username: String,
        name: Option<String>,
        full_name: Option<String>,
        email: Option<String>,
        public_email: Option<String>,
    }
    match service {
        Service::Bitbucket => return bitbucket::parse_user(json).map(|(profile, _)| profile),
        Service::AzureDevOps => return azure::parse_profile(json),
        _ => {}
    }
    let raw: Raw = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let present = |text: Option<String>| text.map(|text| tidy(&text)).filter(|text| !text.is_empty());
    let (login, email, name) = match service {
        Service::GitLab => (raw.username, present(raw.public_email).or_else(|| present(raw.email)), raw.name),
        service if service.is_gitea() => (raw.login, present(raw.email), raw.full_name),
        _ => (raw.login, present(raw.email), raw.name),
    };
    let login = tidy(&login);
    if login.is_empty() {
        return Err("the answer names no account".to_owned());
    }
    Ok(Profile { id: raw.id, login, name: present(name).unwrap_or_default(), email })
}

/// A public SSH key on the account (`GET /user/keys`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshKey {
    pub id: u64,
    pub title: String,
    /// The key's type and body (`ssh-ed25519 AAAA…`), without a comment.
    pub key: String,
    /// `SHA256:…`, as `ssh-keygen -l` prints it; empty when the key cannot
    /// be read.
    pub fingerprint: String,
    /// Seconds since the epoch; 0 when unknown.
    pub created: i64,
    /// When a GitLab key stops working; 0 when it does not expire.
    pub expires: i64,
}

/// One key as both services write it: GitHub sends `id`, `title`, `key`
/// and `created_at`; GitLab the same plus `expires_at`.
#[derive(Deserialize)]
struct RawKey {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    key: String,
    created_at: Option<String>,
    expires_at: Option<String>,
}

fn key_model(raw: RawKey) -> SshKey {
    let parts: Vec<&str> = raw.key.split_whitespace().take(2).collect();
    SshKey {
        id: raw.id,
        title: tidy(&raw.title),
        key: parts.join(" "),
        fingerprint: parts.get(1).and_then(|blob| fingerprint(blob)).unwrap_or_default(),
        created: timestamp(raw.created_at.as_deref()),
        expires: timestamp(raw.expires_at.as_deref()),
    }
}

/// An answer to `GET /user/keys`, from either service.
pub fn parse_keys(json: &str) -> Result<Vec<SshKey>, String> {
    let raw: Vec<RawKey> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(raw.into_iter().map(key_model).collect())
}

/// The key a `POST /user/keys` answers with, from either service.
pub fn parse_key(json: &str) -> Result<SshKey, String> {
    serde_json::from_str::<RawKey>(json).map(key_model).map_err(|e| e.to_string())
}

/// The body of `POST /user/keys`; both services take the same two fields.
pub fn new_key_json(title: &str, key: &PublicKey) -> String {
    serde_json::json!({ "title": title, "key": key.line() }).to_string()
}

// ---- public key files -------------------------------------------------------

/// Key types GitHub and GitLab accept.
const KINDS: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
    "ssh-dss",
];

/// A public key as an OpenSSH `.pub` file holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    pub kind: String,
    /// The base64 body.
    pub blob: String,
    /// Whatever follows the body (often `user@computer`); may be empty.
    pub comment: String,
}

impl PublicKey {
    /// The type and the body: what is uploaded. The comment is left out;
    /// the title says what the key is.
    pub fn line(&self) -> String {
        format!("{} {}", self.kind, self.blob)
    }
}

/// Reads an OpenSSH public key. A private key is turned down with a word on
/// where its public half is, so it can never be sent by mistake.
pub fn parse_public_key(text: &str) -> Result<PublicKey, String> {
    if text.contains("PRIVATE KEY") {
        return Err("That is a private key. Choose the .pub file beside it: only the public half is ever sent".to_owned());
    }
    let not_a_key = || "Not an SSH public key: the file should start with ssh-ed25519, ssh-rsa or ecdsa-sha2".to_owned();
    let line = text.lines().map(str::trim).find(|line| !line.is_empty() && !line.starts_with('#')).ok_or_else(not_a_key)?;
    let mut parts = line.split_whitespace();
    let (Some(kind), Some(blob)) = (parts.next(), parts.next()) else { return Err(not_a_key()) };
    if !KINDS.contains(&kind) {
        return Err(not_a_key());
    }
    // The body starts with its own type, as a length-prefixed string.
    let bytes = base64_decode(blob).ok_or_else(not_a_key)?;
    let named = bytes.get(..4).map(|len| u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize);
    if named != Some(kind.len()) || bytes.get(4..4 + kind.len()) != Some(kind.as_bytes()) {
        return Err("The key in this file is damaged".to_owned());
    }
    Ok(PublicKey { kind: kind.to_owned(), blob: blob.to_owned(), comment: tidy(&parts.collect::<Vec<_>>().join(" ")) })
}

/// A public key file in the SSH folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalKey {
    pub path: PathBuf,
    /// [`PublicKey::line`]: compared with [`SshKey::key`] to tell which of
    /// the account's keys are on this computer.
    pub key: String,
}

/// The public keys in `dir`, by file name. Files too big to be a key, and
/// anything that does not read as one, are passed over.
pub fn local_keys(dir: &Path) -> Vec<LocalKey> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut keys: Vec<LocalKey> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pub"))
        .take(64)
        .filter_map(|path| Some(LocalKey { key: read_public_key(&path).ok()?.line(), path }))
        .collect();
    keys.sort_by(|a, b| a.path.cmp(&b.path));
    keys
}

/// The public key in the file at `path`, or why there is none.
fn read_public_key(path: &Path) -> Result<PublicKey, String> {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let size = std::fs::metadata(path).map_err(|_| format!("{name} was not found"))?.len();
    if size > 64 * 1024 {
        return Err(format!("{name} is too big to be an SSH public key"));
    }
    let text = std::fs::read_to_string(path).map_err(|_| format!("{name} could not be read"))?;
    parse_public_key(&text)
}

/// `SHA256:…` of a key's base64 body, as `ssh-keygen -l` writes it.
pub fn fingerprint(blob: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = base64_decode(blob)?;
    Some(format!("SHA256:{}", base64_encode(&Sha256::digest(&bytes))))
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64, padding optional. `None` for anything else.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = BASE64.iter().position(|&c| c == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Standard base64 without padding, as fingerprints are written.
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, &byte| (n << 8) | byte as u32) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(BASE64[(n >> (18 - 6 * i)) as usize & 63] as char);
        }
    }
    out
}

// ---- generating a key -------------------------------------------------------

/// Where a new key for `account` goes in `dir`: `id_ed25519` when there is
/// none yet (ssh uses that one without being told), else
/// `id_ed25519_github` (`_gitlab`, or after the server's name), numbered
/// when that is taken too. Never a name in use, private or public half.
pub fn new_key_path(dir: &Path, account: &AccountId) -> PathBuf {
    let free = |name: &str| !dir.join(name).exists() && !dir.join(format!("{name}.pub")).exists();
    if free("id_ed25519") {
        return dir.join("id_ed25519");
    }
    let site = if account.self_hosted() { account.host.clone() } else { account.service.name().to_ascii_lowercase() };
    let site: String = site.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let base = format!("id_ed25519_{site}");
    let name = std::iter::once(base.clone())
        .chain((2..).map(|n| format!("{base}_{n}")))
        .find(|name| free(name))
        .unwrap_or(base);
    dir.join(name)
}

/// The public half beside the private key at `path`.
fn public_half(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|name| name.to_os_string()).unwrap_or_default();
    name.push(".pub");
    path.with_file_name(name)
}

/// `ssh-keygen` making an Ed25519 key without a passphrase at `path` (and
/// `path.pub`), quietly: no window, no prompt (stdin is closed, so an
/// unexpected question fails instead of waiting).
fn keygen_command(program: &OsString, path: &Path, comment: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(["-q", "-t", "ed25519", "-N", "", "-C"]).arg(comment).arg("-f").arg(path);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    crate::git::cli::hide_window(&mut cmd);
    cmd
}

/// Where `ssh-keygen` may be: on the `PATH`, else (on Windows) the copy that
/// comes with Git for Windows.
fn keygen_programs() -> Vec<OsString> {
    #[allow(unused_mut)]
    let mut programs = vec![OsString::from("ssh-keygen")];
    #[cfg(windows)]
    for (variable, below) in [
        ("ProgramFiles", r"Git\usr\bin\ssh-keygen.exe"),
        ("ProgramW6432", r"Git\usr\bin\ssh-keygen.exe"),
        ("LOCALAPPDATA", r"Programs\Git\usr\bin\ssh-keygen.exe"),
    ] {
        if let Some(base) = std::env::var_os(variable) {
            programs.push(PathBuf::from(base).join(below).into_os_string());
        }
    }
    programs
}

/// Runs `ssh-keygen` (see [`keygen_command`]), at most `timeout` long.
fn keygen(path: &Path, comment: &str, timeout: Duration) -> Result<(), String> {
    for program in keygen_programs() {
        let mut child = match keygen_command(&program, path, comment).spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("ssh-keygen could not be started ({})", error.kind())),
        };
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("ssh-keygen did not finish in time".to_owned());
                }
            }
        };
        if status.success() {
            return Ok(());
        }
        let mut said = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut said);
        }
        let said = tidy(&said);
        return Err(if said.is_empty() { "ssh-keygen failed".to_owned() } else { format!("ssh-keygen failed: {said}") });
    }
    Err("ssh-keygen was not found. It comes with OpenSSH and with Git for Windows; install either, then try again".to_owned())
}

// ---- what can go wrong ------------------------------------------------------

/// Why something about the account could not be done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trouble {
    /// No sign-in for this host anywhere GitKrill looks.
    NoToken,
    /// Every sign-in found was refused (expired or revoked).
    Rejected,
    /// The sign-in works, but its token may not do this: it lacks `scope`.
    /// `fine_grained` when the token is a fine-grained one (or an app's),
    /// which has permissions instead of scopes.
    Scope { scope: &'static str, fine_grained: bool },
    /// No request before this time (seconds since the epoch).
    RateLimited { until: u64 },
    /// The service said no, in its own words.
    Refused(String),
    /// The request never got an answer.
    Unreachable(String),
    /// Anything else.
    Failed(String),
}

impl Trouble {
    /// What went wrong trying `to` ("read your SSH keys") on `account`, with
    /// the way out when there is one. `source` is where the sign-in came
    /// from, which decides how to give it more scopes.
    pub fn explain(&self, account: &AccountId, source: Option<TokenSource>, to: &str) -> String {
        let site = account.site();
        match self {
            Trouble::NoToken => format!("Not signed in to {site}."),
            Trouble::Rejected => {
                format!("{site} refused the saved sign-in (expired or revoked?). Sign in again, then click Check again.")
            }
            Trouble::Scope { scope, fine_grained: true } => format!(
                "{site} does not let this sign-in {to}: it is a fine-grained token without the \"SSH keys\" \
                 account permission (a classic token needs the {scope} scope)."
            ),
            Trouble::Scope { scope, fine_grained: false } => {
                let fix = match (account.service, source) {
                    (Service::GitHub | Service::GitHubEnterprise, Some(TokenSource::Cli)) => format!(
                        "Run `gh auth refresh -h {} -s admin:public_key` to add it, then try again.",
                        account.host
                    ),
                    (Service::GitHub | Service::GitHubEnterprise, _) => format!(
                        "Sign in with a token that has it, for example `gh auth login -h {} -s admin:public_key`, \
                         then try again.",
                        account.host
                    ),
                    (Service::GitLab, _) => {
                        "Sign in with a personal access token that has it (`glab auth login`), then try again."
                            .to_owned()
                    }
                    (Service::Bitbucket, _) => "Sign in with an API token that has it (Atlassian account settings > \
                         Security > API tokens), then try again."
                        .to_owned(),
                    (service, _) if service.is_gitea() => format!(
                        "Sign in with an access token that has it (Settings > Applications on {site}), then try again."
                    ),
                    _ => "Sign in with a token that has it, then try again.".to_owned(),
                };
                format!("{site} does not let this sign-in {to}: its token needs the {scope} scope. {fix}")
            }
            Trouble::RateLimited { until } => format!("{site} asked us to slow down. Try again after {}.", clock(*until)),
            Trouble::Refused(message) => format!("{site} refused: {message}"),
            Trouble::Unreachable(message) => format!("{site} unreachable: {message}"),
            Trouble::Failed(message) => message.clone(),
        }
    }

    /// The same trouble with every occurrence of `tokens` blanked out: every
    /// one passes through here before it leaves the worker.
    fn scrubbed(self, tokens: &[&Token]) -> Trouble {
        let clean = |text: String| tidy(&tokens.iter().fold(text, |text, token| token.scrub(&text)));
        match self {
            Trouble::Refused(text) => Trouble::Refused(clean(text)),
            Trouble::Unreachable(text) => Trouble::Unreachable(clean(text)),
            Trouble::Failed(text) => Trouble::Failed(clean(text)),
            other => other,
        }
    }
}

/// The service's own words in an error answer: GitHub's `message` and the
/// reasons under `errors`, or whatever GitLab put in its several shapes.
fn api_message(service: Service, body: &str) -> String {
    if service.is_gitlab() {
        return gitlab::api_message(body);
    }
    if service == Service::Bitbucket {
        return bitbucket::api_message(body);
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct RawReason {
        message: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct RawError {
        message: String,
        errors: Vec<RawReason>,
    }
    let error: RawError = serde_json::from_str(body).unwrap_or_default();
    let reasons: Vec<&str> =
        error.errors.iter().map(|reason| reason.message.as_str()).filter(|text| !text.is_empty()).collect();
    match (error.message.is_empty(), reasons.is_empty()) {
        (_, true) => error.message,
        (true, false) => reasons.join("; "),
        (false, false) => format!("{} ({})", error.message, reasons.join("; ")),
    }
}

// ---- the worker -------------------------------------------------------------

/// One account's requests, on the session's thread.
struct Worker {
    account: AccountId,
    /// [`AccountId::api`]: every request goes below it.
    api: String,
    transport: Box<dyn Transport>,
    secrets: Box<dyn Secrets>,
    token: Option<(Token, TokenSource)>,
    /// Tokens the API answered 401 to.
    rejected: Vec<Token>,
    /// Whether `secrets` has been consulted since the last reason to.
    looked: bool,
    /// Where public keys are looked for and new ones made: `~/.ssh`.
    ssh_dir: Option<PathBuf>,
    /// The account's login once known: a generated key's comment.
    login: Option<String>,
    /// Bitbucket lists an account's keys under its id, which the profile
    /// gives.
    key_owner: Option<String>,
}

impl Worker {
    fn new(account: AccountId, transport: Box<dyn Transport>, secrets: Box<dyn Secrets>, ssh_dir: Option<PathBuf>) -> Self {
        Worker {
            api: account.api(),
            account,
            transport,
            secrets,
            token: None,
            rejected: Vec::new(),
            looked: false,
            ssh_dir,
            login: None,
            key_owner: None,
        }
    }

    /// The real network and sign-ins, or the offline fixture.
    fn connect(account: AccountId) -> Self {
        match std::env::var_os("GITKRILL_HOSTING_FIXTURE") {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                let secrets: Box<dyn Secrets> =
                    if dir.join("signed-in").exists() { Box::new(FixtureSignIn) } else { Box::new(NoSecrets) };
                let ssh = dir.join("ssh");
                let transport = CannedAccount { canned: Canned { dir, created: false }, added: false };
                Worker::new(account, Box::new(transport), secrets, Some(ssh))
            }
            None => {
                let headers = headers(account.service);
                let home = dirs::home_dir();
                // `git credential fill` reads the global configuration from
                // anywhere; the home folder is a place sure to exist.
                let secrets = System::new(home.as_deref().unwrap_or(Path::new(".")));
                // Tests and scripted runs keep their keys beside their
                // settings, never in the real ~/.ssh.
                let ssh = match std::env::var_os("GITKRILL_CONFIG_DIR") {
                    Some(dir) => Some(PathBuf::from(dir).join("ssh")),
                    None => home.map(|home| home.join(".ssh")),
                };
                Worker::new(account.clone(), Box::new(Wire::new(account.api(), headers)), Box::new(secrets), ssh)
            }
        }
    }

    fn source(&self) -> Option<TokenSource> {
        self.token.as_ref().map(|(_, source)| *source)
    }

    /// Looks for a sign-in now if there is reason to.
    fn look(&mut self) {
        if !self.looked {
            self.token = find_token(self.secrets.as_ref(), &self.account.repo(), &self.rejected);
            self.looked = true;
        }
    }

    /// The trouble with every token this worker has held blanked out.
    fn scrub(&self, trouble: Trouble) -> Trouble {
        let mut tokens: Vec<&Token> = self.rejected.iter().collect();
        tokens.extend(self.token.as_ref().map(|(token, _)| token));
        trouble.scrubbed(&tokens)
    }

    /// One request below the API, with the sign-in: a GET, or a POST of
    /// `json`. A refused token (401, which means nothing was done) gives way
    /// to the next one found; with none left, nothing is sent at all.
    fn send(&mut self, url: &str, json: Option<&str>, now: u64) -> Result<HttpResponse, Trouble> {
        // Each round drops one token, and there are only so many sources.
        for _ in 0..8 {
            self.look();
            let Some((token, _)) = &self.token else { break };
            let request = HttpRequest { url, etag: None, token: Some(token) };
            let response = match json {
                None => self.transport.get(&request),
                Some(json) => self.transport.post(&request, json),
            }
            .map_err(Trouble::Unreachable)?;
            if response.status == 401 {
                if let Some((token, _)) = self.token.take() {
                    self.rejected.push(token);
                }
                self.looked = false;
                continue;
            }
            let message = api_message(self.account.service, &response.body);
            if let Some(until) = limited_until(response.status, &Rate::of(&response), &message, 0, now) {
                return Err(Trouble::RateLimited { until });
            }
            return Ok(response);
        }
        Err(if self.rejected.is_empty() { Trouble::NoToken } else { Trouble::Rejected })
    }

    fn unreadable(&self, error: String) -> Trouble {
        Trouble::Failed(format!("Unreadable answer from {} ({error})", self.account.site()))
    }

    fn unexpected(&self, status: u16) -> Trouble {
        Trouble::Failed(format!("{} answered {status}", self.account.site()))
    }

    /// What a 403 or 404 to a key request means. Both services answer that
    /// way when the token lacks the scope (GitHub hides the endpoint behind
    /// a 404); only a 403 that says something else is a plain refusal.
    fn refusal(&self, response: &HttpResponse, write: bool) -> Trouble {
        let message = api_message(self.account.service, &response.body);
        let lower = message.to_ascii_lowercase();
        let fine_grained = response.header("x-accepted-github-permissions").is_some()
            || lower.contains("personal access token")
            || lower.contains("by integration");
        let about_scope = response.status == 404
            || fine_grained
            || message.is_empty()
            || lower.contains("not accessible")
            || lower.contains("scope")
            || lower.contains("privileges")
            || message == "403 Forbidden"
            || response.body.contains("insufficient_scope")
            || response.header("x-accepted-oauth-scopes").is_some();
        if !about_scope {
            return Trouble::Refused(message);
        }
        let scope = match (self.account.service, write) {
            (Service::GitLab, false) => "read_user",
            (Service::GitLab, true) => "api",
            (Service::Bitbucket, false) => "read:ssh-key:bitbucket",
            (Service::Bitbucket, true) => "write:ssh-key:bitbucket",
            (service, false) if service.is_gitea() => "read:user",
            (service, true) if service.is_gitea() => "write:user",
            (_, false) => "read:public_key",
            (_, true) => "write:public_key",
        };
        Trouble::Scope { scope, fine_grained }
    }

    /// Where the account's keys are listed and added. Bitbucket's are under
    /// the account's id, known once the profile was read.
    fn keys_url(&self) -> Result<String, Trouble> {
        match self.account.service {
            Service::Bitbucket => match &self.key_owner {
                Some(owner) => Ok(bitbucket::keys_url(owner)),
                None => Err(Trouble::Failed("The account has not been read yet".to_owned())),
            },
            Service::AzureDevOps => Err(Trouble::Failed(
                "Azure DevOps has no API for SSH keys: add them in User settings > SSH public keys".to_owned(),
            )),
            _ => Ok(format!("{}/user/keys", self.api)),
        }
    }

    /// `GET /user` (Azure DevOps: the profile): whose sign-in this is.
    fn profile(&mut self, now: u64) -> Result<Profile, Trouble> {
        let url = match self.account.service {
            Service::AzureDevOps => azure::profile_url(),
            _ => format!("{}/user", self.api),
        };
        let response = self.send(&url, None, now)?;
        match response.status {
            200 if self.account.service == Service::Bitbucket => {
                let (profile, owner) = bitbucket::parse_user(&response.body).map_err(|e| self.unreadable(e))?;
                self.login = Some(profile.login.clone());
                self.key_owner = Some(owner);
                Ok(profile)
            }
            200 => {
                let profile = parse_user(self.account.service, &response.body).map_err(|e| self.unreadable(e))?;
                self.login = Some(profile.login.clone());
                Ok(profile)
            }
            // GitLab tokens need read_user (or api) even for this.
            403 | 404 if self.account.service.is_gitlab() => Err(Trouble::Scope { scope: "read_user", fine_grained: false }),
            403 => Err(Trouble::Refused(api_message(self.account.service, &response.body))),
            status => Err(self.unexpected(status)),
        }
    }

    /// `GET /user/keys` (Bitbucket: `/users/{id}/ssh-keys`).
    fn keys(&mut self, now: u64) -> Result<Vec<SshKey>, Trouble> {
        let page = match self.account.service {
            Service::Bitbucket => "pagelen=100",
            // Gitea and Forgejo cap their pages at 50.
            service if service.is_gitea() => "limit=50",
            _ => "per_page=100",
        };
        let url = format!("{}?{page}", self.keys_url()?);
        let response = self.send(&url, None, now)?;
        match response.status {
            200 if self.account.service == Service::Bitbucket => {
                bitbucket::parse_keys(&response.body).map_err(|e| self.unreadable(e))
            }
            200 => parse_keys(&response.body).map_err(|e| self.unreadable(e)),
            403 | 404 => Err(self.refusal(&response, false)),
            status => Err(self.unexpected(status)),
        }
    }

    /// `POST /user/keys` (Bitbucket: `/users/{id}/ssh-keys`). Never repeated
    /// after a request that may have reached the service, except after a
    /// 401, which means nothing was done.
    fn upload(&mut self, title: &str, key: &PublicKey, now: u64) -> Result<SshKey, Trouble> {
        let bitbucket = self.account.service == Service::Bitbucket;
        let body = if bitbucket { bitbucket::new_key_json(title, key) } else { new_key_json(title, key) };
        let url = self.keys_url()?;
        let response = self.send(&url, Some(&body), now)?;
        let site = self.account.site();
        match response.status {
            200 | 201 if bitbucket => bitbucket::parse_key(&response.body).map_err(|e| self.unreadable(e)),
            200 | 201 => parse_key(&response.body).map_err(|e| self.unreadable(e)),
            403 | 404 => Err(self.refusal(&response, true)),
            400 | 409 | 422 => {
                let message = api_message(self.account.service, &response.body);
                let lower = message.to_ascii_lowercase();
                Err(Trouble::Refused(if lower.contains("already") || lower.contains("been used") {
                    format!("this key is already on a {site} account, this one or another")
                } else if message.is_empty() {
                    "the key was not accepted".to_owned()
                } else {
                    message
                }))
            }
            status => Err(self.unexpected(status)),
        }
    }

    /// The title a key is uploaded with: the one typed, else the key's
    /// comment, else its file name.
    fn title(typed: &str, key: &PublicKey, path: &Path) -> String {
        let typed = tidy(typed);
        if !typed.is_empty() {
            return typed;
        }
        if !key.comment.is_empty() {
            return key.comment.clone();
        }
        path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "GitKrill".to_owned())
    }

    /// Adds the public key in the file at `path`. A private key's path is
    /// taken to mean the `.pub` beside it.
    fn add(&mut self, title: &str, path: &Path, now: u64) -> Result<SshKey, Trouble> {
        let path = if path.extension().is_none_or(|ext| ext != "pub") && public_half(path).is_file() {
            public_half(path)
        } else {
            path.to_path_buf()
        };
        let key = read_public_key(&path).map_err(Trouble::Failed)?;
        self.upload(&Self::title(title, &key, &path), &key, now)
    }

    /// Makes a new key in the SSH folder and adds it. Returns where the key
    /// was made, if it was, with how the upload went.
    fn generate(&mut self, title: &str, now: u64) -> (Option<PathBuf>, Result<SshKey, Trouble>) {
        // A key that cannot be added is not worth making: sign-in first.
        self.look();
        if self.token.is_none() {
            return (None, Err(if self.rejected.is_empty() { Trouble::NoToken } else { Trouble::Rejected }));
        }
        let Some(dir) = self.ssh_dir.clone() else {
            return (None, Err(Trouble::Failed("There is no home folder to keep the key in".to_owned())));
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return (None, Err(Trouble::Failed(format!("{} could not be created", dir.display()))));
        }
        let path = new_key_path(&dir, &self.account);
        let typed = tidy(title);
        let comment = if typed.is_empty() { self.login.clone().unwrap_or_else(|| "gitkrill".to_owned()) } else { typed };
        if let Err(error) = keygen(&path, &comment, Duration::from_secs(30)) {
            return (None, Err(Trouble::Failed(error)));
        }
        let key = match read_public_key(&public_half(&path)) {
            Ok(key) => key,
            Err(error) => return (Some(path), Err(Trouble::Failed(error))),
        };
        let result = self.upload(&comment, &key, now);
        (Some(path), result)
    }

    fn local(&self) -> Vec<LocalKey> {
        self.ssh_dir.as_deref().map(local_keys).unwrap_or_default()
    }
}

/// The offline fixture's answers for the account endpoints; see the module
/// documentation.
struct CannedAccount {
    canned: Canned,
    added: bool,
}

impl Transport for CannedAccount {
    fn get(&mut self, request: &HttpRequest<'_>) -> Result<HttpResponse, String> {
        let path = request.url.split('?').next().unwrap_or_default();
        // Bitbucket's keys are `.../ssh-keys`, Azure DevOps' profile `.../me`.
        match path.rsplit('/').next() {
            Some("user" | "me") => Ok(self.canned.answer("user", 200)),
            Some("keys" | "ssh-keys") if self.added && self.canned.dir.join("keys.after.json").exists() => {
                Ok(self.canned.answer("keys.after", 200))
            }
            Some("keys" | "ssh-keys") => Ok(self.canned.answer("keys", 200)),
            _ => Err("no fixture for this address".to_owned()),
        }
    }

    fn post(&mut self, request: &HttpRequest<'_>, body: &str) -> Result<HttpResponse, String> {
        if !request.url.ends_with("/user/keys") && !request.url.ends_with("/ssh-keys") {
            return Err("no fixture for this address".to_owned());
        }
        let _ = std::fs::write(self.canned.dir.join("keys.sent.json"), body);
        let response = self.canned.answer("keys.post", 201);
        self.added |= response.status == 201;
        Ok(response)
    }
}

// ---- the session ------------------------------------------------------------

enum Order {
    /// Look for a sign-in again, then read the account and its keys.
    Check,
    Add { title: String, path: PathBuf },
    Generate { title: String },
}

enum Event {
    Checked { source: Option<TokenSource>, profile: Result<Profile, Trouble> },
    Keys(Result<Vec<SshKey>, Trouble>),
    Local(Vec<LocalKey>),
    Added { generated: Option<PathBuf>, result: Result<SshKey, Trouble> },
}

/// Serves orders until the session goes away (`orders` closes). `wake`
/// asks the UI for a frame.
fn serve(mut worker: Worker, orders: Receiver<Order>, events: Sender<Event>, wake: impl Fn()) {
    let send = |event: Event| {
        let sent = events.send(event).is_ok();
        wake();
        sent
    };
    while let Ok(order) = orders.recv() {
        let now = epoch_now();
        // Every order looks for the sign-in afresh: the user may have signed
        // in or out since, or given the token another scope.
        worker.token = None;
        worker.looked = false;
        let refresh = match order {
            Order::Check => {
                let profile = worker.profile(now).map_err(|trouble| worker.scrub(trouble));
                let known = profile.is_ok();
                if !send(Event::Checked { source: worker.source(), profile }) || !send(Event::Local(worker.local())) {
                    return;
                }
                known
            }
            Order::Add { title, path } => {
                let result = worker.add(&title, &path, now).map_err(|trouble| worker.scrub(trouble));
                let added = result.is_ok();
                if !send(Event::Added { generated: None, result }) {
                    return;
                }
                added
            }
            Order::Generate { title } => {
                let (generated, result) = worker.generate(&title, now);
                let result = result.map_err(|trouble| worker.scrub(trouble));
                let made = generated.is_some();
                let added = result.is_ok();
                if !send(Event::Added { generated, result }) || (made && !send(Event::Local(worker.local()))) {
                    return;
                }
                added
            }
        };
        if refresh {
            let keys = worker.keys(epoch_now()).map_err(|trouble| worker.scrub(trouble));
            if !send(Event::Keys(keys)) {
                return;
            }
        }
    }
}

/// What a key upload was started from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    /// "Add existing SSH key".
    Adding,
    /// "Generate SSH key and add".
    Generating,
}

/// How the last upload went.
#[derive(Clone, Debug)]
pub struct Done {
    pub work: Work,
    /// Where a generated key was made, even when adding it then failed.
    pub generated: Option<PathBuf>,
    pub result: Result<SshKey, Trouble>,
}

/// What the page shows about one account.
#[derive(Clone, Debug, Default)]
pub struct AccountView {
    /// A check is under way.
    pub checking: bool,
    /// Where the token in use came from (never the token).
    pub source: Option<TokenSource>,
    /// The account, or why it is not known; `None` before the first answer.
    pub profile: Option<Result<Profile, Trouble>>,
    /// The account's keys, or why they could not be read; `None` before
    /// the first answer and while there is no account.
    pub keys: Option<Result<Vec<SshKey>, Trouble>>,
    /// The keys are being read (again).
    pub keys_loading: bool,
    /// Public keys in the SSH folder.
    pub local: Vec<LocalKey>,
    /// An upload under way.
    pub working: Option<Work>,
    pub done: Option<Done>,
}

impl AccountView {
    /// The local file holding `key`, if this computer has it.
    pub fn local_file(&self, key: &SshKey) -> Option<&Path> {
        self.local.iter().find(|local| local.key == key.key).map(|local| local.path.as_path())
    }

    /// `id_ed25519.pub` in the SSH folder, if there is one: what "Add
    /// existing SSH key" offers first.
    pub fn default_key(&self) -> Option<&Path> {
        let named = |local: &&LocalKey| local.path.file_name().is_some_and(|name| name == "id_ed25519.pub");
        self.local.iter().find(named).map(|local| local.path.as_path())
    }
}

/// One account's background work while the preferences page shows it. The
/// thread ends once this is dropped (after the request in flight, if any).
pub struct AccountSession {
    pub account: AccountId,
    orders: Sender<Order>,
    events: Receiver<Event>,
    pub view: AccountView,
}

impl AccountSession {
    /// Starts the thread and the first check.
    pub fn start(account: AccountId, ctx: &egui::Context) -> Self {
        let worker = Worker::connect(account.clone());
        let ctx = ctx.clone();
        Self::with_worker(account, worker, move || ctx.request_repaint())
    }

    fn with_worker(account: AccountId, worker: Worker, wake: impl Fn() + Send + 'static) -> Self {
        let (orders, orders_rx) = channel();
        let (events_tx, events) = channel();
        // If the thread cannot start, its channel ends close with the
        // closure and `poll` says the worker stopped.
        let _ = thread::Builder::new().name("account".into()).spawn(move || serve(worker, orders_rx, events_tx, wake));
        let mut session = AccountSession { account, orders, events, view: AccountView::default() };
        session.check();
        session
    }

    /// Looks for a sign-in again and reads the account and its keys.
    pub fn check(&mut self) {
        if self.view.checking || self.view.working.is_some() {
            return;
        }
        self.view.checking = true;
        self.view.done = None;
        self.order(Order::Check);
    }

    /// Adds the public key in the file at `path` to the account.
    pub fn add(&mut self, title: &str, path: PathBuf) {
        self.start_work(Work::Adding, Order::Add { title: title.to_owned(), path });
    }

    /// Makes a new key in the SSH folder and adds it to the account.
    pub fn generate(&mut self, title: &str) {
        self.start_work(Work::Generating, Order::Generate { title: title.to_owned() });
    }

    pub fn busy(&self) -> bool {
        self.view.checking || self.view.working.is_some()
    }

    fn start_work(&mut self, work: Work, order: Order) {
        if self.busy() {
            return;
        }
        self.view.working = Some(work);
        self.view.done = None;
        self.order(order);
    }

    fn order(&mut self, order: Order) {
        if self.orders.send(order).is_err() {
            self.stopped();
        }
    }

    /// The thread is gone, which only a panic or a failed start can cause:
    /// say so instead of spinning forever.
    fn stopped(&mut self) {
        let trouble = Trouble::Failed("The background worker stopped".to_owned());
        let view = &mut self.view;
        if view.checking {
            view.profile = Some(Err(trouble.clone()));
        }
        if let Some(work) = view.working.take() {
            view.done = Some(Done { work, generated: None, result: Err(trouble) });
        }
        view.checking = false;
        view.keys_loading = false;
    }

    /// Takes in what the thread has produced. Cheap: call it every frame
    /// the account is shown.
    pub fn poll(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(Event::Checked { source, profile }) => {
                    let view = &mut self.view;
                    view.checking = false;
                    view.source = source;
                    view.keys_loading = profile.is_ok();
                    if profile.is_err() {
                        view.keys = None;
                    }
                    view.profile = Some(profile);
                }
                Ok(Event::Keys(keys)) => {
                    self.view.keys_loading = false;
                    self.view.keys = Some(keys);
                }
                Ok(Event::Local(local)) => self.view.local = local,
                Ok(Event::Added { generated, result }) => {
                    let view = &mut self.view;
                    view.keys_loading |= result.is_ok();
                    if let Some(work) = view.working.take() {
                        view.done = Some(Done { work, generated, result });
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.busy() || self.view.keys_loading {
                        self.stopped();
                    }
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fake, Log, SECRET, Script, reply};
    use super::*;

    const NOW: u64 = 1_800_000_000;

    /// A real Ed25519 public key (made for this test, never used) and what
    /// `ssh-keygen -l` prints for it.
    const PUBLIC: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ6/rvs4h30pDKGBvPxCCedyMBv2RjD2wg++vAYe/VwB test@vector";
    const FINGERPRINT: &str = "SHA256:rLmucD0ZT+3NWpg/ewTzpahexLO2rz/okUf2br8ey0Y";
    const BLOB: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJ6/rvs4h30pDKGBvPxCCedyMBv2RjD2wg++vAYe/VwB";

    fn scripted(account: AccountId, replies: Vec<Result<HttpResponse, String>>, secrets: Fake) -> (Worker, Log) {
        let sent = Log::default();
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        (Worker::new(account, Box::new(transport), Box::new(secrets), None), sent)
    }

    fn signed_in() -> Fake {
        Fake { env: vec![("GITHUB_TOKEN", SECRET), ("GH_ENTERPRISE_TOKEN", SECRET), ("GITLAB_TOKEN", SECRET)], ..Default::default() }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("account-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn enterprise() -> AccountId {
        AccountId { service: Service::GitHubEnterprise, host: "git.example.com".to_owned() }
    }

    // ---- answers ----

    #[test]
    fn github_users_map_to_the_profile() {
        let json = r#"{"login": "octocat", "id": 583231, "name": "The Octocat", "email": null,
                       "avatar_url": "https://avatars.githubusercontent.com/u/583231?v=4"}"#;
        let profile = parse_user(Service::GitHub, json).unwrap();
        assert_eq!(profile, Profile { id: 583231, login: "octocat".into(), name: "The Octocat".into(), email: None });
        assert_eq!(profile.display_name(), "The Octocat");
        // github.com pictures come by id; an Enterprise server's do not.
        assert_eq!(
            profile.avatar_email(&AccountId::github()).as_deref(),
            Some("583231+octocat@users.noreply.github.com")
        );
        assert_eq!(profile.avatar_email(&enterprise()), None);

        let bare = parse_user(Service::GitHubEnterprise, r#"{"login": "ada", "id": 7, "name": "", "email": "ada@example.com"}"#).unwrap();
        assert_eq!(bare.display_name(), "ada");
        assert_eq!(bare.avatar_email(&enterprise()).as_deref(), Some("ada@example.com"));
        assert!(parse_user(Service::GitHub, r#"{"id": 1}"#).is_err());
        assert!(parse_user(Service::GitHub, "<html>").is_err());
    }

    #[test]
    fn gitlab_users_map_to_the_profile() {
        let json = r#"{"id": 1, "username": "john_smith", "name": "John Smith", "state": "active",
                       "public_email": "", "email": "john@example.com", "web_url": "https://gitlab.com/john_smith"}"#;
        let profile = parse_user(Service::GitLab, json).unwrap();
        assert_eq!(profile.login, "john_smith");
        assert_eq!(profile.name, "John Smith");
        // The public address first, else the account's own.
        assert_eq!(profile.email.as_deref(), Some("john@example.com"));
        let public = parse_user(Service::GitLab, r#"{"id": 2, "username": "x", "public_email": "x@pub.example", "email": "x@own.example"}"#);
        assert_eq!(public.unwrap().avatar_email(&AccountId::gitlab()).as_deref(), Some("x@pub.example"));
        // GitHub's field is not GitLab's.
        assert!(parse_user(Service::GitLab, r#"{"id": 3, "login": "octocat"}"#).is_err());
    }

    #[test]
    fn keys_of_both_services_map_to_the_model() {
        let github = format!(
            r#"[{{"key": "{PUBLIC}", "id": 2, "url": "https://api.github.com/user/keys/2", "title": "Laptop",
                 "created_at": "2025-02-23T10:00:00Z", "verified": true, "read_only": false}}]"#
        );
        let keys = parse_keys(&github).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].title, "Laptop");
        assert_eq!(keys[0].key, format!("ssh-ed25519 {BLOB}"));
        assert_eq!(keys[0].fingerprint, FINGERPRINT);
        assert_eq!(keys[0].created, 1_740_304_800);
        assert_eq!(keys[0].expires, 0);

        let gitlab = format!(
            r#"[{{"id": 1, "title": "Work", "key": "ssh-ed25519 {BLOB}", "created_at": "2025-02-23T10:00:00.080Z",
                 "expires_at": "2027-01-01T00:00:00.000Z", "usage_type": "auth_and_signing"}},
                {{"id": 5, "title": "Odd", "key": "ssh-rsa not-base64!"}}]"#
        );
        let keys = parse_keys(&gitlab).unwrap();
        assert_eq!(keys[0].created, 1_740_304_800);
        assert_eq!(keys[0].expires, 1_798_761_600);
        assert_eq!(keys[0].fingerprint, FINGERPRINT);
        // A key that cannot be read is still listed, without a fingerprint.
        assert_eq!((keys[1].title.as_str(), keys[1].fingerprint.as_str()), ("Odd", ""));

        let one = parse_key(&format!(r#"{{"id": 9, "title": "New", "key": "{PUBLIC}"}}"#)).unwrap();
        assert_eq!((one.id, one.title.as_str()), (9, "New"));
        assert!(parse_keys(r#"{"message": "Not Found"}"#).is_err());
    }

    #[test]
    fn public_keys_are_read_and_private_ones_refused() {
        let key = parse_public_key(&format!("# made by hand\n\n{PUBLIC}\n")).unwrap();
        assert_eq!(key.kind, "ssh-ed25519");
        assert_eq!(key.comment, "test@vector");
        assert_eq!(key.line(), format!("ssh-ed25519 {BLOB}"));
        assert_eq!(fingerprint(&key.blob).as_deref(), Some(FINGERPRINT));

        let private = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n";
        assert!(parse_public_key(private).unwrap_err().contains("private key"));
        assert!(parse_public_key("").is_err());
        assert!(parse_public_key("hello world").is_err());
        // The body must name the type the line claims.
        assert!(parse_public_key(&format!("ssh-rsa {BLOB}")).unwrap_err().contains("damaged"));
        assert!(parse_public_key("ssh-ed25519 AAAA!!!!").is_err());
    }

    #[test]
    fn base64_round_trips() {
        for text in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            let encoded = base64_encode(text.as_bytes());
            assert_eq!(base64_decode(&encoded).unwrap(), text.as_bytes());
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fo"), "Zm8");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert!(base64_decode("Zm8-").is_none());
    }

    #[test]
    fn the_upload_body_is_the_title_and_the_key_without_its_comment() {
        let key = parse_public_key(PUBLIC).unwrap();
        let body: serde_json::Value = serde_json::from_str(&new_key_json("Laptop \"work\"", &key)).unwrap();
        assert_eq!(body, serde_json::json!({ "title": "Laptop \"work\"", "key": format!("ssh-ed25519 {BLOB}") }));
    }

    #[test]
    fn typed_server_names_are_cleaned_up_or_turned_down() {
        assert_eq!(server_host("git.example.com").unwrap(), "git.example.com");
        assert_eq!(server_host("  https://Git.Example.com/group/app ").unwrap(), "git.example.com");
        assert_eq!(server_host("git.example.com:8443").unwrap(), "git.example.com:8443");
        assert_eq!(server_host("https://git.example.com:443/").unwrap(), "git.example.com");
        assert_eq!(server_host("http://git.example.com:8080").unwrap(), "git.example.com");
        assert!(server_host("").is_err());
        assert!(server_host("github.com").unwrap_err().contains("GitHub"));
        assert!(server_host("https://www.gitlab.com").is_err());
        assert!(server_host("bitbucket.org").is_err());

        // A GitLab installed under a path, however the path is written.
        for path in ["gitlab", "/gitlab", "gitlab/", " /gitlab/ "] {
            assert_eq!(under_path("example.com", path).unwrap(), "example.com/gitlab", "{path:?}");
        }
        assert_eq!(under_path("example.com:8443", "tools/gitlab").unwrap(), "example.com:8443/tools/gitlab");
        assert_eq!(under_path("example.com", "").unwrap(), "example.com");
        assert_eq!(under_path("example.com", " / ").unwrap(), "example.com");
        for path in ["git lab", "../gitlab", "gitlab//x", "a?b", r"gitlab\x", "a/b/c/d/e/f/g/h/i"] {
            assert!(under_path("example.com", path).is_err(), "{path:?}");
        }
        assert!(server_host("ssh://git.example.com").is_err());
        assert!(server_host("not a host").is_err());
        assert!(server_host("intranet").is_err());
        assert!(server_host("git.example.com:http").is_err());
    }

    // ---- requests ----

    #[test]
    fn the_account_and_its_keys_are_asked_of_the_hosts_api_with_its_token() {
        let keys = format!(r#"[{{"id": 2, "title": "Laptop", "key": "{PUBLIC}"}}]"#);
        let (mut worker, sent) = scripted(
            AccountId::github(),
            vec![reply(200, &[], r#"{"login": "octocat", "id": 1}"#), reply(200, &[], &keys)],
            signed_in(),
        );
        assert_eq!(worker.profile(NOW).unwrap().login, "octocat");
        assert_eq!(worker.source(), Some(TokenSource::Env("GITHUB_TOKEN")));
        assert_eq!(worker.keys(NOW).unwrap()[0].fingerprint, FINGERPRINT);
        let sent = sent.lock().unwrap();
        assert_eq!(sent[0].url, "https://api.github.com/user");
        assert_eq!(sent[1].url, "https://api.github.com/user/keys?per_page=100");
        assert!(sent.iter().all(|request| request.token.as_deref() == Some(SECRET) && request.body.is_none()));

        let (mut worker, sent) = scripted(
            AccountId { service: Service::GitLab, host: "git.example.com".into() },
            vec![reply(200, &[], r#"{"id": 1, "username": "john"}"#)],
            Fake { env: vec![("GITLAB_HOST", "git.example.com"), ("GITLAB_TOKEN", SECRET)], ..Default::default() },
        );
        assert_eq!(worker.profile(NOW).unwrap().login, "john");
        assert_eq!(sent.lock().unwrap()[0].url, "https://git.example.com/api/v4/user");
    }

    #[test]
    fn bitbucket_gitea_and_azure_accounts_are_asked_where_each_keeps_them() {
        // Bitbucket: the profile names the account's id, its keys are below it.
        let user = r#"{"username": "ada", "display_name": "Ada", "uuid": "{0f1e2d3c-aaaa-bbbb-cccc-111122223333}"}"#;
        let keys = format!(r#"{{"values": [{{"label": "Laptop", "key": "{PUBLIC}"}}]}}"#);
        let added = format!(r#"{{"uuid": "{{k2}}", "label": "New", "key": "{PUBLIC}"}}"#);
        let bitbucket = Fake { env: vec![("BITBUCKET_EMAIL", "ada@example.com"), ("BITBUCKET_API_TOKEN", SECRET)], ..Default::default() };
        let (mut worker, sent) = scripted(
            AccountId::bitbucket(),
            vec![reply(200, &[], user), reply(200, &[], &keys), reply(201, &[], &added)],
            bitbucket.clone(),
        );
        // Before the profile, the keys' address is not known: nothing is sent.
        assert!(matches!(worker.keys(NOW), Err(Trouble::Failed(_))));
        assert_eq!(worker.profile(NOW).unwrap().display_name(), "Ada");
        assert_eq!(worker.keys(NOW).unwrap()[0].title, "Laptop");
        let key = parse_public_key(PUBLIC).unwrap();
        assert_eq!(worker.upload("New", &key, NOW).unwrap().title, "New");
        {
            let sent = sent.lock().unwrap();
            let owner = "%7B0f1e2d3c-aaaa-bbbb-cccc-111122223333%7D";
            assert_eq!(sent[0].url, "https://api.bitbucket.org/2.0/user");
            assert_eq!(sent[1].url, format!("https://api.bitbucket.org/2.0/users/{owner}/ssh-keys?pagelen=100"));
            assert_eq!(sent[2].url, format!("https://api.bitbucket.org/2.0/users/{owner}/ssh-keys"));
            let body: serde_json::Value = serde_json::from_str(sent[2].body.as_deref().unwrap()).unwrap();
            assert_eq!(body, serde_json::json!({"key": key.line(), "label": "New"}));
        }
        // A missing scope is named in Bitbucket's terms.
        let scope = r#"{"type": "error", "error": {"message": "Your credentials lack one or more required privilege scopes."}}"#;
        let (mut worker, _) = scripted(AccountId::bitbucket(), vec![reply(200, &[], user), reply(403, &[], scope)], bitbucket);
        worker.profile(NOW).unwrap();
        let trouble = worker.keys(NOW).unwrap_err();
        assert_eq!(trouble, Trouble::Scope { scope: "read:ssh-key:bitbucket", fine_grained: false });
        assert!(trouble.explain(&AccountId::bitbucket(), None, "read your SSH keys").contains("API token"));

        // Codeberg: Gitea's `/user`, with `full_name`; pages of at most 50.
        let (mut worker, sent) = scripted(
            AccountId::codeberg(),
            vec![reply(200, &[], r#"{"id": 7, "login": "ada", "full_name": "Ada Lovelace", "email": "ada@noreply.codeberg.org"}"#), reply(200, &[], "[]")],
            Fake { env: vec![("CODEBERG_TOKEN", SECRET)], ..Default::default() },
        );
        let profile = worker.profile(NOW).unwrap();
        assert_eq!((profile.login.as_str(), profile.display_name()), ("ada", "Ada Lovelace"));
        assert_eq!(worker.keys(NOW).unwrap(), Vec::new());
        let urls: Vec<String> = sent.lock().unwrap().iter().map(|request| request.url.clone()).collect();
        assert_eq!(urls, ["https://codeberg.org/api/v1/user", "https://codeberg.org/api/v1/user/keys?limit=50"]);

        // Azure DevOps: the profile service, and no keys API to ask.
        let (mut worker, sent) = scripted(
            AccountId::azure(),
            vec![reply(200, &[], r#"{"displayName": "Ada Lovelace", "emailAddress": "ada@acme.example"}"#)],
            Fake { env: vec![("AZURE_DEVOPS_EXT_PAT", SECRET)], ..Default::default() },
        );
        assert_eq!(worker.profile(NOW).unwrap().login, "ada@acme.example");
        assert!(matches!(worker.keys(NOW), Err(Trouble::Failed(text)) if text.contains("no API for SSH keys")));
        assert!(!AccountId::azure().keys_supported() && AccountId::bitbucket().keys_supported());
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].url.starts_with("https://app.vssps.visualstudio.com/_apis/profile/profiles/me?"));
        assert_eq!(AccountId::azure().api(), "https://app.vssps.visualstudio.com");
        assert!(!AccountId::azure().self_hosted() && AccountId::azure().site() == "Azure DevOps");
    }

    #[test]
    fn without_a_sign_in_nothing_is_sent() {
        let (mut worker, sent) = scripted(AccountId::github(), vec![], Fake::default());
        assert_eq!(worker.profile(NOW), Err(Trouble::NoToken));
        assert_eq!(worker.keys(NOW), Err(Trouble::NoToken));
        let key = parse_public_key(PUBLIC).unwrap();
        assert_eq!(worker.upload("x", &key, NOW), Err(Trouble::NoToken));
        assert_eq!(worker.generate("x", NOW), (None, Err(Trouble::NoToken)));
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn a_refused_token_gives_way_to_the_next_then_to_none() {
        let fake = Fake { env: vec![("GITHUB_TOKEN", SECRET)], cli: Some("gho_SECONDfakeTOKEN000000000000000000"), ..Default::default() };
        let (mut worker, sent) = scripted(
            AccountId::github(),
            vec![reply(401, &[], "{}"), reply(200, &[], r#"{"login": "octocat"}"#)],
            fake.clone(),
        );
        assert!(worker.profile(NOW).is_ok());
        assert_eq!(worker.source(), Some(TokenSource::Cli));
        assert_eq!(sent.lock().unwrap().len(), 2);

        let (mut worker, _) = scripted(AccountId::github(), vec![reply(401, &[], "{}"), reply(401, &[], "{}")], fake);
        assert_eq!(worker.profile(NOW), Err(Trouble::Rejected));
        assert_eq!(worker.source(), None);
    }

    #[test]
    fn a_missing_scope_is_named_for_each_service_and_action() {
        let scopes = [("x-accepted-oauth-scopes", "admin:public_key, read:public_key"), ("x-oauth-scopes", "repo, gist")];
        let (mut worker, _) = scripted(AccountId::github(), vec![reply(404, &scopes, r#"{"message": "Not Found"}"#)], signed_in());
        assert_eq!(worker.keys(NOW), Err(Trouble::Scope { scope: "read:public_key", fine_grained: false }));

        let key = parse_public_key(PUBLIC).unwrap();
        let fine = r#"{"message": "Resource not accessible by personal access token"}"#;
        let (mut worker, _) = scripted(AccountId::github(), vec![reply(403, &[], fine)], signed_in());
        assert_eq!(worker.upload("x", &key, NOW), Err(Trouble::Scope { scope: "write:public_key", fine_grained: true }));

        let insufficient = r#"{"error": "insufficient_scope", "error_description": "The request requires higher privileges than provided by the access token.", "scope": "api"}"#;
        let (mut worker, _) = scripted(AccountId::gitlab(), vec![reply(403, &[], insufficient), reply(403, &[], insufficient)], signed_in());
        assert_eq!(worker.upload("x", &key, NOW), Err(Trouble::Scope { scope: "api", fine_grained: false }));
        assert_eq!(worker.profile(NOW), Err(Trouble::Scope { scope: "read_user", fine_grained: false }));

        // Anything else a 403 says is passed on as it was said.
        let (mut worker, _) = scripted(enterprise(), vec![reply(403, &[], r#"{"message": "SSH keys are managed by your company"}"#)], signed_in());
        assert_eq!(worker.keys(NOW), Err(Trouble::Refused("SSH keys are managed by your company".into())));

        // The way out depends on where the sign-in came from.
        let scope = Trouble::Scope { scope: "write:public_key", fine_grained: false };
        let text = scope.explain(&AccountId::github(), Some(TokenSource::Cli), "add SSH keys");
        assert!(text.contains("write:public_key") && text.contains("gh auth refresh -h github.com -s admin:public_key"), "{text}");
        let text = scope.explain(&enterprise(), Some(TokenSource::CredentialHelper), "add SSH keys");
        assert!(text.contains("gh auth login -h git.example.com"), "{text}");
        let text = Trouble::Scope { scope: "api", fine_grained: false }.explain(&AccountId::gitlab(), None, "add SSH keys");
        assert!(text.contains("api scope") && text.contains("glab auth login"), "{text}");
    }

    #[test]
    fn an_upload_posts_the_key_once_and_explains_a_refusal() {
        let dir = temp("upload");
        std::fs::write(dir.join("id_ed25519.pub"), format!("{PUBLIC}\n")).unwrap();
        std::fs::write(dir.join("id_ed25519"), "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        let created = format!(r#"{{"id": 9, "title": "test@vector", "key": "{PUBLIC}"}}"#);
        let (mut worker, sent) = scripted(AccountId::github(), vec![reply(201, &[], &created)], signed_in());
        // Pointed at the private half, the public one beside it is sent.
        assert_eq!(worker.add("", &dir.join("id_ed25519"), NOW).unwrap().id, 9);
        {
            let sent = sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].url, "https://api.github.com/user/keys");
            let body: serde_json::Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
            // Untitled: the key's comment names it.
            assert_eq!(body["title"], "test@vector");
            assert_eq!(body["key"], format!("ssh-ed25519 {BLOB}"));
        }

        let taken = r#"{"message": "Validation Failed", "errors": [{"resource": "PublicKey", "code": "custom", "field": "key", "message": "key is already in use"}]}"#;
        let (mut worker, sent) = scripted(AccountId::github(), vec![reply(422, &[], taken)], signed_in());
        let refused = worker.add("Laptop", &dir.join("id_ed25519.pub"), NOW).unwrap_err();
        assert!(matches!(&refused, Trouble::Refused(text) if text.contains("already on a GitHub account")), "{refused:?}");
        assert_eq!(sent.lock().unwrap().len(), 1);

        let gitlab = r#"{"message": {"key": ["has already been taken"], "fingerprint_sha256": ["has already been taken"]}}"#;
        let (mut worker, _) = scripted(AccountId::gitlab(), vec![reply(400, &[], gitlab)], signed_in());
        assert!(matches!(worker.add("x", &dir.join("id_ed25519.pub"), NOW), Err(Trouble::Refused(_))));

        // Files that are not public keys are never sent.
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();
        let (mut worker, sent) = scripted(AccountId::github(), vec![], signed_in());
        assert!(matches!(worker.add("x", &dir.join("notes.txt"), NOW), Err(Trouble::Failed(_))));
        assert!(matches!(worker.add("x", &dir.join("missing.pub"), NOW), Err(Trouble::Failed(_))));
        assert!(sent.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_token_never_reaches_a_displayed_error() {
        let (mut worker, _) = scripted(
            AccountId::github(),
            vec![Err(format!("tls: failed sending Authorization: Bearer {SECRET}")), reply(500, &[], "")],
            signed_in(),
        );
        let trouble = worker.profile(NOW).map_err(|trouble| worker.scrub(trouble)).unwrap_err();
        let text = trouble.explain(&AccountId::github(), worker.source(), "read your account");
        assert!(!text.contains(SECRET) && text.contains("***"), "{text}");
        assert!(!format!("{:?}", worker.token).contains(SECRET));
    }

    #[test]
    fn new_keys_never_take_a_name_in_use() {
        let dir = temp("names");
        assert_eq!(new_key_path(&dir, &AccountId::github()), dir.join("id_ed25519"));
        std::fs::write(dir.join("id_ed25519.pub"), "").unwrap();
        assert_eq!(new_key_path(&dir, &AccountId::github()), dir.join("id_ed25519_github"));
        assert_eq!(new_key_path(&dir, &AccountId::gitlab()), dir.join("id_ed25519_gitlab"));
        let own = AccountId { service: Service::GitLab, host: "git.example.com:8443".into() };
        assert_eq!(new_key_path(&dir, &own), dir.join("id_ed25519_git_example_com_8443"));
        std::fs::write(dir.join("id_ed25519_github"), "").unwrap();
        std::fs::write(dir.join("id_ed25519_github_2.pub"), "").unwrap();
        assert_eq!(new_key_path(&dir, &AccountId::github()), dir.join("id_ed25519_github_3"));
        assert_eq!(public_half(&dir.join("id_ed25519_github_3")), dir.join("id_ed25519_github_3.pub"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_keygen_is_asked_for_an_ed25519_key_without_a_passphrase_or_a_prompt() {
        let path = Path::new("keys").join("id_ed25519");
        let cmd = keygen_command(&OsString::from("ssh-keygen"), &path, "octocat laptop");
        let args: Vec<String> = cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["-q", "-t", "ed25519", "-N", "", "-C", "octocat laptop", "-f", &path.to_string_lossy()]);
    }

    #[test]
    fn a_generated_key_is_made_in_the_ssh_folder_and_its_public_half_added() {
        let dir = temp("generate");
        // Without ssh-keygen on this machine there is nothing to try.
        if keygen(&dir.join("probe"), "probe", Duration::from_secs(30)).is_err() {
            return;
        }
        let ssh = dir.join("ssh");
        let created = format!(r#"{{"id": 3, "title": "octocat", "key": "{PUBLIC}"}}"#);
        let sent = Log::default();
        let transport = Script { replies: vec![reply(201, &[], &created)].into(), sent: sent.clone() };
        let mut worker = Worker::new(AccountId::github(), Box::new(transport), Box::new(signed_in()), Some(ssh.clone()));
        worker.login = Some("octocat".into());
        let (made, result) = worker.generate("", NOW);
        assert_eq!(made.as_deref(), Some(ssh.join("id_ed25519").as_path()));
        assert!(result.is_ok());
        let local = local_keys(&ssh);
        assert_eq!(local.len(), 1);
        let body: serde_json::Value = serde_json::from_str(sent.lock().unwrap()[0].body.as_deref().unwrap()).unwrap();
        // Untitled, the key is named after the account, and so is its comment.
        assert_eq!(body["title"], "octocat");
        assert_eq!(body["key"], local[0].key);
        assert!(std::fs::read_to_string(ssh.join("id_ed25519.pub")).unwrap().trim_end().ends_with(" octocat"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_keys_are_matched_to_the_accounts() {
        let dir = temp("local");
        std::fs::write(dir.join("id_ed25519.pub"), PUBLIC).unwrap();
        std::fs::write(dir.join("broken.pub"), "nonsense").unwrap();
        std::fs::write(dir.join("config"), "Host *").unwrap();
        let local = local_keys(&dir);
        assert_eq!(local, vec![LocalKey { path: dir.join("id_ed25519.pub"), key: format!("ssh-ed25519 {BLOB}") }]);
        let view = AccountView { local, ..Default::default() };
        let key = parse_key(&format!(r#"{{"id": 1, "title": "x", "key": "{PUBLIC}"}}"#)).unwrap();
        assert_eq!(view.local_file(&key), Some(dir.join("id_ed25519.pub").as_path()));
        assert_eq!(view.default_key(), Some(dir.join("id_ed25519.pub").as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the offline fixture and the session ----

    #[test]
    fn the_offline_fixture_answers_the_account_endpoints() {
        let dir = temp("fixture");
        std::fs::write(dir.join("user.json"), r#"{"login": "octocat", "id": 1}"#).unwrap();
        std::fs::write(dir.join("keys.json"), "[]").unwrap();
        std::fs::write(dir.join("keys.after.json"), format!(r#"[{{"id": 4, "title": "New", "key": "{PUBLIC}"}}]"#)).unwrap();
        std::fs::write(dir.join("keys.post.json"), format!(r#"{{"id": 4, "title": "New", "key": "{PUBLIC}"}}"#)).unwrap();
        std::fs::write(dir.join("new.pub"), PUBLIC).unwrap();
        let transport = CannedAccount { canned: Canned { dir: dir.clone(), created: false }, added: false };
        let mut worker = Worker::new(AccountId::github(), Box::new(transport), Box::new(FixtureSignIn), None);
        assert_eq!(worker.profile(NOW).unwrap().login, "octocat");
        assert!(worker.keys(NOW).unwrap().is_empty());
        assert_eq!(worker.add("New", &dir.join("new.pub"), NOW).unwrap().id, 4);
        let sent: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("keys.sent.json")).unwrap()).unwrap();
        assert_eq!(sent["title"], "New");
        assert_eq!(worker.keys(NOW).unwrap().len(), 1);

        // A staged refusal.
        std::fs::write(dir.join("keys.post.head"), "404\nx-accepted-oauth-scopes: admin:public_key, write:public_key\n").unwrap();
        assert_eq!(
            worker.add("New", &dir.join("new.pub"), NOW),
            Err(Trouble::Scope { scope: "write:public_key", fine_grained: false })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_checks_at_once_then_reads_the_keys() {
        let keys = format!(r#"[{{"id": 2, "title": "Laptop", "key": "{PUBLIC}"}}]"#);
        let (worker, sent) = scripted(
            AccountId::github(),
            vec![reply(200, &[], r#"{"login": "octocat", "id": 1}"#), reply(200, &[], &keys)],
            signed_in(),
        );
        let (woke, woken) = channel();
        let mut session = AccountSession::with_worker(AccountId::github(), worker, move || {
            let _ = woke.send(());
        });
        assert!(session.view.checking && session.busy());
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.view.keys.is_none() && Instant::now() < deadline {
            let _ = woken.recv_timeout(Duration::from_millis(100));
            session.poll();
        }
        assert!(!session.view.checking && !session.view.keys_loading);
        assert_eq!(session.view.source, Some(TokenSource::Env("GITHUB_TOKEN")));
        assert_eq!(session.view.profile.as_ref().unwrap().as_ref().unwrap().login, "octocat");
        assert_eq!(session.view.keys.as_ref().unwrap().as_ref().unwrap().len(), 1);
        assert_eq!(sent.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_session_without_a_sign_in_reads_no_keys() {
        let (worker, sent) = scripted(AccountId::gitlab(), vec![], Fake::default());
        let mut session = AccountSession::with_worker(AccountId::gitlab(), worker, || {});
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.view.checking && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
            session.poll();
        }
        assert_eq!(session.view.profile, Some(Err(Trouble::NoToken)));
        assert!(session.view.keys.is_none() && !session.view.keys_loading);
        assert!(sent.lock().unwrap().is_empty());
    }
}
