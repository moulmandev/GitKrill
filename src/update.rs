//! Updates from GitKrill's GitHub releases.
//!
//! How a copy updates depends on how it was installed ([`Kind`]):
//!
//! - **Installed** with the Windows installer, which leaves
//!   [`INSTALLED_MARKER`] next to the executable: a newer release's installer
//!   is downloaded in the background and run silently when GitKrill closes,
//!   or straight away with "Restart to update".
//! - **Portable** (the standalone executable on Windows, the binary on
//!   Linux): the user is asked first; on yes the new executable is
//!   downloaded, checked, put in place of the running one, and the app
//!   restarts.
//! - **Browser** (macOS, anything else): the user is asked, and the release
//!   page opens.
//! - **Off**: builds made from source never update themselves.
//!
//! Every download is checked against the SHA-256 that GitHub reports for
//! the file and the release's `SHA256SUMS.txt` before it is used.
//!
//! Network: one request to api.github.com per check (shortly after start,
//! then every six hours), and downloads from github.com, which redirects to
//! its download host. Nothing is sent but the requests themselves.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Where releases are published.
pub const RELEASES: &str = "https://github.com/moulmandev/GitKrill/releases";
const LATEST: &str = "https://api.github.com/repos/moulmandev/GitKrill/releases/latest";
/// Where Git for Windows is published, for installing Git (`crate::git_setup`).
const GIT_FOR_WINDOWS: &str = "https://github.com/git-for-windows/git/releases";
const GIT_FOR_WINDOWS_LATEST: &str = "https://api.github.com/repos/git-for-windows/git/releases/latest";

/// Left next to the executable by the installer.
pub const INSTALLED_MARKER: &str = "gitkrill-installed";
/// Release files, as CI names them.
pub const INSTALLER: &str = "GitKrill-Setup-x64.exe";
pub const PORTABLE_WINDOWS: &str = "gitkrill-portable-windows-x64.exe";
pub const PORTABLE_LINUX: &str = "gitkrill-portable-linux-x64";
pub const SUMS: &str = "SHA256SUMS.txt";

/// The first check waits until the app has settled.
const FIRST_CHECK: Duration = Duration::from_secs(8);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_FEED: u64 = 2 * 1024 * 1024;
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;

// ---- versions ---------------------------------------------------------------

/// `major.minor.patch`. Anything else (pre-releases, build metadata) is not
/// offered as an update.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let number = |part: Option<&str>| -> Option<u32> {
            let part = part?;
            (!part.is_empty() && part.len() <= 9 && part.bytes().all(|b| b.is_ascii_digit())).then(|| part.parse().ok())?
        };
        let mut parts = text.split('.');
        let version = Version(number(parts.next())?, number(parts.next())?, number(parts.next())?);
        parts.next().is_none().then_some(version)
    }

    /// This build's version.
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("the package version is x.y.z")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

// ---- how this copy updates --------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Installed,
    Portable,
    Browser,
    Off,
}

impl Kind {
    pub fn detect() -> Kind {
        // For trying the flows out on a development build.
        if let Ok(forced) = std::env::var("GITKRILL_UPDATE_KIND") {
            return match forced.as_str() {
                "installed" => Kind::Installed,
                "portable" => Kind::Portable,
                "browser" => Kind::Browser,
                _ => Kind::Off,
            };
        }
        let official = option_env!("GITKRILL_OFFICIAL_BUILD").is_some();
        if !official && std::env::var_os("GITKRILL_UPDATE_FEED").is_none() {
            return Kind::Off;
        }
        if cfg!(all(windows, target_arch = "x86_64")) {
            let dir = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf));
            match dir.is_some_and(|dir| dir.join(INSTALLED_MARKER).exists()) {
                true => Kind::Installed,
                false => Kind::Portable,
            }
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Kind::Portable
        } else {
            Kind::Browser
        }
    }

    /// The release file this copy updates itself from.
    pub fn asset(self) -> Option<&'static str> {
        match self {
            Kind::Installed => Some(INSTALLER),
            Kind::Portable if cfg!(windows) => Some(PORTABLE_WINDOWS),
            Kind::Portable => Some(PORTABLE_LINUX),
            Kind::Browser | Kind::Off => None,
        }
    }
}

// ---- where releases come from -----------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    feed: String,
    downloads: String,
    pages: String,
    /// A local test server: plain HTTP on the loopback address only.
    local: bool,
}

impl Source {
    pub fn github() -> Source {
        Source::releases(RELEASES, LATEST)
    }

    /// Git for Windows' releases. `GITKRILL_GIT_FEED` stands in for them as
    /// `GITKRILL_UPDATE_FEED` does for GitKrill's (see [`Source::from_env`]).
    pub fn git_for_windows() -> Source {
        Source::local_or("GITKRILL_GIT_FEED", Source::releases(GIT_FOR_WINDOWS, GIT_FOR_WINDOWS_LATEST))
    }

    fn releases(pages: &str, latest: &str) -> Source {
        Source {
            feed: latest.to_owned(),
            downloads: format!("{pages}/download"),
            pages: format!("{pages}/tag"),
            local: false,
        }
    }

    /// `GITKRILL_UPDATE_FEED=http://127.0.0.1:8000` tries updates against a
    /// folder served locally: `<feed>/latest.json` in the API's format and
    /// the files under `<feed>/download/<tag>/`. Only loopback addresses are
    /// accepted; anything else means GitHub.
    pub fn from_env() -> Source {
        Source::local_or("GITKRILL_UPDATE_FEED", Source::github())
    }

    /// The local test server named by the variable `var`, or `fallback`.
    fn local_or(var: &str, fallback: Source) -> Source {
        match std::env::var(var) {
            Ok(base) if loopback(&base) => {
                let base = base.trim_end_matches('/');
                Source {
                    feed: format!("{base}/latest.json"),
                    downloads: format!("{base}/download"),
                    pages: format!("{base}/tag"),
                    local: true,
                }
            }
            _ => fallback,
        }
    }

    /// Where the latest release is described (the API's `releases/latest`).
    pub fn feed(&self) -> &str {
        &self.feed
    }

    /// A local test server rather than GitHub.
    pub fn is_local(&self) -> bool {
        self.local
    }

    /// The release's page, for "What's new" and for downloading by hand.
    /// Built from the tag, which [`parse_release`] only lets through when it
    /// is plain `v1.2.3`-like text.
    pub fn page(&self, release: &Release) -> String {
        format!("{}/{}", self.pages, release.tag)
    }

    fn download_url(&self, release: &Release, name: &str) -> String {
        format!("{}/{}/{name}", self.downloads, release.tag)
    }

    /// Whether a request may go to `url`, first or after a redirect.
    fn allows(&self, url: &str) -> bool {
        if self.local {
            return loopback(url);
        }
        let Some(host) = url.strip_prefix("https://").and_then(authority) else { return false };
        host == "github.com" || host == "api.github.com" || host.ends_with(".githubusercontent.com")
    }
}

/// The host of `rest` (what follows `scheme://`), refusing anything that a
/// URL parser could read another way: user names, backslashes, odd bytes.
fn authority(rest: &str) -> Option<&str> {
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let valid = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':');
    if authority.is_empty() || !authority.bytes().all(valid) || !rest.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    Some(authority.split(':').next().unwrap_or(authority))
}

fn loopback(url: &str) -> bool {
    url.strip_prefix("http://")
        .and_then(authority)
        .is_some_and(|host| host == "127.0.0.1" || host == "localhost")
}

// ---- the release on offer -----------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    /// The release notes, as written on GitHub (Markdown).
    pub notes: String,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    /// The file's SHA-256 in hex, when GitHub reports it.
    pub sha256: Option<String>,
}

/// The `releases/latest` answer.
pub fn parse_release(json: &str) -> Result<Release, String> {
    let raw = parse_raw(json)?;
    let version = Version::parse(&raw.tag_name).filter(|_| !raw.draft && !raw.prerelease);
    let Some(version) = version else { return Err("No release to update to".to_owned()) };
    Ok(raw.into_release(version))
}

/// Git for Windows' `releases/latest` answer. Its tags read
/// `v2.56.0.windows.4`; the version is the part before `.windows`. Files
/// without a digest from GitHub take the SHA-256 from the table in the
/// release notes ("Git-2.56.0.4-64-bit.exe | 4961a4...").
pub fn parse_git_for_windows(json: &str) -> Result<Release, String> {
    let raw = parse_raw(json)?;
    let number = raw.tag_name.split(".windows").next().unwrap_or_default();
    let plain = raw.tag_name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.');
    let version = Version::parse(number).filter(|_| plain && !raw.draft && !raw.prerelease);
    let Some(version) = version else { return Err("Git for Windows has no release to install".to_owned()) };
    let mut release = raw.into_release(version);
    for asset in &mut release.assets {
        if asset.sha256.is_none() {
            asset.sha256 = release.notes.lines().find_map(|line| {
                let (name, hex) = line.split_once('|')?;
                (name.trim() == asset.name && is_sha256(hex.trim())).then(|| hex.trim().to_ascii_lowercase())
            });
        }
    }
    Ok(release)
}

#[derive(Deserialize)]
struct Raw {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

fn parse_raw(json: &str) -> Result<Raw, String> {
    serde_json::from_str(json).map_err(|_| "The release information could not be read".to_owned())
}

impl Raw {
    fn into_release(self, version: Version) -> Release {
        let assets = self
            .assets
            .into_iter()
            .map(|asset| Asset {
                sha256: asset
                    .digest
                    .and_then(|digest| digest.strip_prefix("sha256:").map(str::to_ascii_lowercase))
                    .filter(|hex| is_sha256(hex)),
                name: asset.name,
                size: asset.size,
            })
            .collect();
        let mut notes = self.body.unwrap_or_default().replace("\r\n", "\n");
        if let Some((cut, _)) = notes.char_indices().nth(6000) {
            notes.truncate(cut);
        }
        Release { version, tag: self.tag_name, notes, assets }
    }
}

fn is_sha256(hex: &str) -> bool {
    hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The hash `SHA256SUMS.txt` gives for `name` (`<hex>  <name>` lines, as
/// `sha256sum` writes them).
pub fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name && is_sha256(hex)).then(|| hex.to_ascii_lowercase())
    })
}

/// One line of release notes, ready to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    Heading(String),
    Bullet(String),
    Text(String),
}

/// The release notes (Markdown) as headings, bullets and paragraphs, with
/// wrapped lines joined, emphasis marks and GitHub's "by @someone in <link>"
/// tails dropped, and the "Full Changelog" link left out.
pub fn plain_notes(notes: &str) -> Vec<Note> {
    let clean = |text: &str| {
        let text = match text.find(" by @") {
            Some(at) if text[at..].contains(" in http") => &text[..at],
            _ => text,
        };
        text.replace("**", "").replace('`', "")
    };
    let mut out: Vec<Note> = Vec::new();
    let mut joinable = false;
    for line in notes.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("**Full Changelog**") || line.starts_with("<!--") {
            joinable = false;
            continue;
        }
        if line.starts_with('#') {
            out.push(Note::Heading(clean(line.trim_start_matches('#').trim())));
            joinable = false;
        } else if let Some(item) = line.strip_prefix("* ").or_else(|| line.strip_prefix("- ")) {
            out.push(Note::Bullet(clean(item)));
            joinable = true;
        } else if joinable && let Some(Note::Bullet(text) | Note::Text(text)) = out.last_mut() {
            // A wrapped line belongs to the bullet or paragraph above.
            text.push(' ');
            text.push_str(&clean(line));
        } else {
            out.push(Note::Text(clean(line)));
            joinable = true;
        }
    }
    out
}

// ---- the network ---------------------------------------------------------------

/// GET requests, for checking and downloading.
pub trait Net: Send {
    /// The body of `url`, at most `limit` bytes. `progress(received, total)`
    /// is called as it arrives. Redirects are followed only where `source`
    /// allows.
    fn get(&mut self, source: &Source, url: &str, limit: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Vec<u8>, String>;
}

/// The real network, through `ureq`.
pub struct Wire {
    agent: ureq::Agent,
}

impl Wire {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // Redirects are followed here, checking each address first.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_body(Some(Duration::from_secs(120)))
            .user_agent(concat!("GitKrill/", env!("CARGO_PKG_VERSION")))
            .build();
        Wire { agent: config.into() }
    }
}

impl Net for Wire {
    fn get(&mut self, source: &Source, url: &str, limit: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Vec<u8>, String> {
        let mut url = url.to_owned();
        for _ in 0..6 {
            if !source.allows(&url) {
                return Err("The download was sent somewhere unexpected".to_owned());
            }
            let mut response = self
                .agent
                .get(&url)
                .header("Accept", "application/vnd.github+json, application/octet-stream")
                .call()
                .map_err(|error| match error {
                    ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => "No connection".to_owned(),
                    ureq::Error::Timeout(_) => "The connection timed out".to_owned(),
                    _ => "The connection failed".to_owned(),
                })?;
            let status = response.status().as_u16();
            let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
            match status {
                200 => {
                    let total = header("content-length").and_then(|v| v.parse().ok());
                    if total.is_some_and(|total| total > limit) {
                        return Err("The download is larger than expected".to_owned());
                    }
                    let mut reader = response.body_mut().with_config().limit(limit).reader();
                    let mut body = Vec::with_capacity(total.unwrap_or(0).min(limit) as usize);
                    let mut chunk = vec![0u8; 64 * 1024];
                    loop {
                        let read = std::io::Read::read(&mut reader, &mut chunk).map_err(|_| "The download was interrupted".to_owned())?;
                        if read == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..read]);
                        progress(body.len() as u64, total);
                    }
                    return Ok(body);
                }
                301 | 302 | 303 | 307 | 308 => {
                    let Some(location) = header("location") else { return Err("GitHub sent a broken redirect".to_owned()) };
                    url = location;
                }
                403 | 429 => return Err("GitHub asked to wait a while (rate limited)".to_owned()),
                404 => return Err("Not found on GitHub".to_owned()),
                status => return Err(format!("GitHub answered {status}")),
            }
        }
        Err("Too many redirects".to_owned())
    }
}

/// The latest release.
pub fn check(net: &mut dyn Net, source: &Source) -> Result<Release, String> {
    let body = net.get(source, &source.feed, MAX_FEED, &mut |_, _| {})?;
    parse_release(&String::from_utf8_lossy(&body))
}

/// Downloads `name` from `release` into `dir` and checks it. Returns where
/// it was saved.
pub fn download(
    net: &mut dyn Net,
    source: &Source,
    release: &Release,
    name: &str,
    dir: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| format!("Release {} has no download for this system", release.tag))?;

    // What the file must hash to: GitHub's own record and the release's
    // checksum list. At least one is needed, and they must agree.
    let mut expected = asset.sha256.clone();
    if release.assets.iter().any(|asset| asset.name == SUMS) {
        let sums = net.get(source, &source.download_url(release, SUMS), 1024 * 1024, &mut |_, _| {})?;
        if let Some(listed) = sum_for(&String::from_utf8_lossy(&sums), name) {
            if expected.as_ref().is_some_and(|known| *known != listed) {
                return Err("The release's checksums disagree; not updating".to_owned());
            }
            expected = Some(listed);
        }
    }
    let Some(expected) = expected else { return Err("The release has no checksum for this download".to_owned()) };

    let bytes = net.get(source, &source.download_url(release, name), MAX_DOWNLOAD, progress)?;
    let actual: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        return Err("The download is damaged (its checksum does not match)".to_owned());
    }

    std::fs::create_dir_all(dir).map_err(|e| format!("Could not save the download ({e})"))?;
    let path = dir.join(format!("{}-{name}", release.tag));
    std::fs::write(&path, &bytes).map_err(|e| format!("Could not save the download ({e})"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(path)
}

/// Where downloads wait until they are used.
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("GitKrill-update")
}

// ---- putting an update in place ---------------------------------------------------

/// Starts the installer without any window. With `relaunch`, it starts
/// GitKrill again when it is done (see `installer/gitkrill.iss`). The
/// installer closes this copy itself if it is still running.
pub fn run_installer(file: &Path, relaunch: bool) -> Result<(), String> {
    let mut cmd = Command::new(file);
    cmd.args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"]);
    if relaunch {
        cmd.arg("/RELAUNCH=1");
    }
    cmd.spawn().map(drop).map_err(|e| format!("Could not start the installer ({e})"))
}

/// Puts `file` in place of the running executable.
pub fn replace_self(file: &Path) -> Result<(), String> {
    self_replace::self_replace(file).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            "GitKrill's folder is read-only for you; download the new version by hand".to_owned()
        }
        _ => format!("Could not replace GitKrill ({e})"),
    })?;
    let _ = std::fs::remove_file(file);
    Ok(())
}

/// Starts the (new) executable again with the same arguments. Scripted
/// screenshot runs only say so, so they never leave a window behind.
pub fn restart() -> Result<(), String> {
    if std::env::var_os("GITKRILL_SHOT").is_some() {
        eprintln!("would restart GitKrill");
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| format!("Could not restart ({e})"))?;
    Command::new(exe)
        .args(std::env::args_os().skip(1))
        .spawn()
        .map(drop)
        .map_err(|e| format!("Could not restart ({e})"))
}

// ---- the state the UI sees -----------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Checking,
    UpToDate,
    Available(Arc<Release>),
    Downloading { release: Arc<Release>, received: u64, total: Option<u64> },
    Ready { release: Arc<Release>, file: PathBuf },
    Failed(String),
}

/// What the UI should do about a change: shown once each.
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    /// A newer release, to offer (portable, browser) or to fetch
    /// (installed); `asked` when the user ran the check.
    Found { release: Arc<Release>, asked: bool },
    /// The check the user asked for found nothing newer.
    UpToDate,
    /// The download finished and checked out.
    Downloaded(Arc<Release>, PathBuf),
    /// Checking or downloading failed; `asked` when the user started it.
    Failed { message: String, asked: bool },
}

struct Inner {
    status: Status,
    notices: Vec<Notice>,
}

/// Checks for and fetches updates on background threads.
pub struct Updater {
    pub kind: Kind,
    source: Source,
    inner: Arc<Mutex<Inner>>,
    started: Instant,
    last_check: Option<Instant>,
    /// "Restart to update" or quitting has started the installer.
    pub applied: bool,
    /// A release to offer once no other dialog is open.
    pub offer: Option<Arc<Release>>,
}

impl Updater {
    pub fn new() -> Self {
        Updater::with(Kind::detect(), Source::from_env())
    }

    pub fn with(kind: Kind, source: Source) -> Self {
        Updater {
            kind,
            source,
            inner: Arc::new(Mutex::new(Inner { status: Status::Idle, notices: Vec::new() })),
            started: Instant::now(),
            last_check: None,
            applied: false,
            offer: None,
        }
    }

    pub fn status(&self) -> Status {
        self.inner.lock().unwrap().status.clone()
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    fn busy(&self) -> bool {
        matches!(self.status(), Status::Checking | Status::Downloading { .. })
    }

    /// Changes since the last call.
    pub fn notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.inner.lock().unwrap().notices)
    }

    /// Checks when one is due: shortly after start, then every few hours.
    /// Scripted screenshot runs only check against a local test feed.
    pub fn tick(&mut self, ctx: &egui::Context, enabled: bool) {
        let scripted = std::env::var_os("GITKRILL_SHOT").is_some() && !self.source.local;
        if !enabled || scripted || self.kind == Kind::Off || self.busy() {
            return;
        }
        // Nothing more to look for once an update waits to be installed.
        if matches!(self.status(), Status::Ready { .. }) {
            return;
        }
        let due = match self.last_check {
            None => self.started + FIRST_CHECK,
            Some(at) => at + CHECK_EVERY,
        };
        let now = Instant::now();
        if now >= due {
            self.check(ctx, false);
        } else {
            ctx.request_repaint_after(due - now);
        }
    }

    /// Asks GitHub for the latest release.
    pub fn check(&mut self, ctx: &egui::Context, asked: bool) {
        if self.kind == Kind::Off || self.busy() {
            return;
        }
        self.last_check = Some(Instant::now());
        self.inner.lock().unwrap().status = Status::Checking;
        let (inner, source, ctx) = (self.inner.clone(), self.source.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = check(&mut Wire::new(), &source);
            let mut inner = inner.lock().unwrap();
            match result {
                Ok(release) if release.version > Version::current() => {
                    let release = Arc::new(release);
                    inner.status = Status::Available(release.clone());
                    inner.notices.push(Notice::Found { release, asked });
                }
                Ok(_) => {
                    inner.status = Status::UpToDate;
                    if asked {
                        inner.notices.push(Notice::UpToDate);
                    }
                }
                Err(message) => {
                    inner.status = Status::Failed(message.clone());
                    inner.notices.push(Notice::Failed { message, asked });
                }
            }
            ctx.request_repaint();
        });
    }

    /// Downloads this copy's file from `release`.
    pub fn download(&mut self, ctx: &egui::Context, release: Arc<Release>, asked: bool) {
        let Some(name) = self.kind.asset() else { return };
        if self.busy() {
            return;
        }
        self.inner.lock().unwrap().status = Status::Downloading { release: release.clone(), received: 0, total: None };
        let (inner, source, ctx) = (self.inner.clone(), self.source.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut last_paint = Instant::now();
            let mut progress = |received: u64, total: Option<u64>| {
                if let Status::Downloading { received: r, total: t, .. } = &mut inner.lock().unwrap().status {
                    (*r, *t) = (received, total);
                }
                if last_paint.elapsed() > Duration::from_millis(100) {
                    last_paint = Instant::now();
                    ctx.request_repaint();
                }
            };
            let result = download(&mut Wire::new(), &source, &release, name, &download_dir(), &mut progress);
            let mut inner = inner.lock().unwrap();
            match result {
                Ok(file) => {
                    inner.status = Status::Ready { release: release.clone(), file: file.clone() };
                    inner.notices.push(Notice::Downloaded(release, file));
                }
                Err(message) => {
                    inner.status = Status::Failed(message.clone());
                    inner.notices.push(Notice::Failed { message, asked });
                }
            }
            ctx.request_repaint();
        });
    }

    /// Installed copies: starts the downloaded installer, once.
    pub fn install(&mut self, relaunch: bool) -> Result<bool, String> {
        let Status::Ready { file, .. } = self.status() else { return Ok(false) };
        if self.applied || self.kind != Kind::Installed {
            return Ok(false);
        }
        run_installer(&file, relaunch)?;
        self.applied = true;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn versions_compare_by_number_and_only_plain_ones_count() {
        assert_eq!(Version::parse("v0.2.0"), Some(Version(0, 2, 0)));
        assert_eq!(Version::parse("1.10.3"), Some(Version(1, 10, 3)));
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
        for bad in ["v1.2", "1.2.3.4", "v1.2.3-beta", "1.2.x", "", "v", "1..2", "+1.2.3", "1.2.3+build"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn requests_only_go_to_github_or_a_local_test_feed() {
        let github = Source::github();
        for good in [
            "https://api.github.com/repos/moulmandev/GitKrill/releases/latest",
            "https://github.com/moulmandev/GitKrill/releases/download/v0.2.0/x.exe",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1?sp=r",
            "https://objects.githubusercontent.com/x",
        ] {
            assert!(github.allows(good), "{good}");
        }
        for bad in [
            "http://github.com/x",
            "https://github.com.evil.example/x",
            "https://evil.example/github.com",
            "https://user@github.com/x",
            "https://githubusercontent.com.evil.example/x",
            "https://github.com\\@evil.example/x",
            "https://github.com/x y",
            "file:///C:/x",
            "http://127.0.0.1:8000/latest.json",
        ] {
            assert!(!github.allows(bad), "{bad}");
        }

        let local = Source { feed: String::new(), downloads: String::new(), pages: String::new(), local: true };
        assert!(local.allows("http://127.0.0.1:8000/download/v1.0.0/a"));
        assert!(local.allows("http://localhost:9/x"));
        assert!(!local.allows("https://github.com/x"));
        assert!(!local.allows("http://127.0.0.1.evil.example/x"));
        assert!(loopback("http://127.0.0.1:8765") && !loopback("http://10.0.0.1:8765"));
    }

    const RELEASE: &str = r###"{
        "tag_name": "v9.1.0", "draft": false, "prerelease": false,
        "body": "## What's Changed\r\n* Preview images in diffs by @moulmandev in https://github.com/moulmandev/GitKrill/pull/3\r\n* Faster graph\r\n\r\n**Full Changelog**: https://github.com/moulmandev/GitKrill/compare/v0.2.0...v9.1.0",
        "assets": [
            {"name": "GitKrill-Setup-x64.exe", "size": 9000000, "digest": "sha256:AB12000000000000000000000000000000000000000000000000000000000000"},
            {"name": "gitkrill-portable-windows-x64.exe", "size": 18000000, "digest": null},
            {"name": "SHA256SUMS.txt", "size": 400}
        ]
    }"###;

    #[test]
    fn the_latest_release_is_read_from_the_api_answer() {
        let release = parse_release(RELEASE).unwrap();
        assert_eq!((release.version, release.tag.as_str()), (Version(9, 1, 0), "v9.1.0"));
        assert_eq!(release.assets.len(), 3);
        assert_eq!(release.assets[0].sha256.as_deref(), Some("ab12000000000000000000000000000000000000000000000000000000000000"));
        assert_eq!(release.assets[1].sha256, None);
        assert_eq!(
            plain_notes(&release.notes),
            [
                Note::Heading("What's Changed".into()),
                Note::Bullet("Preview images in diffs".into()),
                Note::Bullet("Faster graph".into()),
            ]
        );
        assert_eq!(Source::github().page(&release), "https://github.com/moulmandev/GitKrill/releases/tag/v9.1.0");

        // Drafts, pre-releases and odd tags are not updates.
        for json in [
            r#"{"tag_name": "v9.1.0", "draft": true}"#,
            r#"{"tag_name": "v9.1.0", "prerelease": true}"#,
            r#"{"tag_name": "v9.1.0/../../x"}"#,
            r#"{"tag_name": "nightly"}"#,
            "not json",
        ] {
            assert!(parse_release(json).is_err(), "{json}");
        }
    }

    #[test]
    fn wrapped_changelog_lines_are_joined() {
        let notes = "- Windows installer that keeps GitKrill up to date, and portable
  builds that ask
- Light theme

Thanks for
trying it.";
        assert_eq!(
            plain_notes(notes),
            [
                Note::Bullet("Windows installer that keeps GitKrill up to date, and portable builds that ask".into()),
                Note::Bullet("Light theme".into()),
                Note::Text("Thanks for trying it.".into()),
            ]
        );
        // The section CI publishes for this version reads cleanly too.
        let changelog = include_str!("../CHANGELOG.md");
        let section = changelog.split("\n## ").find(|s| s.starts_with(env!("CARGO_PKG_VERSION"))).expect("a changelog section for this version");
        let body = section.split_once("\n").map_or("", |(_, body)| body);
        let notes = plain_notes(body);
        assert!(!notes.is_empty() && notes.iter().all(|note| matches!(note, Note::Bullet(_))), "{notes:?}");
    }

    #[test]
    fn checksum_lists_are_read_as_sha256sum_writes_them() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!("{a}  GitKrill-Setup-x64.exe\n{b} *gitkrill-portable-linux-x64\nbad  x\n");
        assert_eq!(sum_for(&sums, INSTALLER), Some(a));
        assert_eq!(sum_for(&sums, PORTABLE_LINUX), Some("b".repeat(64)));
        assert_eq!(sum_for(&sums, "x"), None);
        assert_eq!(sum_for(&sums, "missing"), None);
    }

    /// Answers from a table of URLs; records what was asked.
    struct Table(HashMap<String, Vec<u8>>, Vec<String>);

    impl Net for Table {
        fn get(&mut self, source: &Source, url: &str, limit: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Vec<u8>, String> {
            assert!(source.allows(url), "{url}");
            self.1.push(url.to_owned());
            let body = self.0.get(url).cloned().ok_or("Not found on GitHub")?;
            if body.len() as u64 > limit {
                return Err("too large".to_owned());
            }
            progress(body.len() as u64, Some(body.len() as u64));
            Ok(body)
        }
    }

    fn hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    fn release_with(name: &str, digest: Option<String>, sums: bool) -> Release {
        let mut assets = vec![Asset { name: name.to_owned(), size: 3, sha256: digest }];
        if sums {
            assets.push(Asset { name: SUMS.to_owned(), size: 100, sha256: None });
        }
        Release { version: Version(9, 0, 0), tag: "v9.0.0".to_owned(), notes: String::new(), assets }
    }

    #[test]
    fn downloads_are_used_only_when_their_checksum_matches() {
        let dir = std::env::temp_dir().join(format!("gitkrill-update-test-{}", std::process::id()));
        let source = Source::github();
        let base = "https://github.com/moulmandev/GitKrill/releases/download/v9.0.0";
        let file = b"new".to_vec();
        let url = format!("{base}/{INSTALLER}");
        let sums_url = format!("{base}/{SUMS}");
        let table = |sums: Option<String>| {
            let mut map = HashMap::from([(url.clone(), file.clone())]);
            if let Some(sums) = sums {
                map.insert(sums_url.clone(), sums.into_bytes());
            }
            Table(map, Vec::new())
        };
        let mut none = |_: u64, _: Option<u64>| {};

        // GitHub's digest alone is enough.
        let release = release_with(INSTALLER, Some(hex(&file)), false);
        let saved = download(&mut table(None), &source, &release, INSTALLER, &dir, &mut none).unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), file);
        assert_eq!(saved.file_name().unwrap().to_str().unwrap(), "v9.0.0-GitKrill-Setup-x64.exe");

        // So is the checksum list.
        let release = release_with(INSTALLER, None, true);
        let sums = format!("{}  {INSTALLER}\n", hex(&file));
        assert!(download(&mut table(Some(sums)), &source, &release, INSTALLER, &dir, &mut none).is_ok());

        // A damaged file, disagreeing checksums, or none at all: refused.
        let release = release_with(INSTALLER, Some(hex(b"other")), false);
        let error = download(&mut table(None), &source, &release, INSTALLER, &dir, &mut none).unwrap_err();
        assert!(error.contains("damaged"), "{error}");
        let release = release_with(INSTALLER, Some(hex(&file)), true);
        let sums = format!("{}  {INSTALLER}\n", hex(b"other"));
        let error = download(&mut table(Some(sums)), &source, &release, INSTALLER, &dir, &mut none).unwrap_err();
        assert!(error.contains("disagree"), "{error}");
        let release = release_with(INSTALLER, None, false);
        let error = download(&mut table(None), &source, &release, INSTALLER, &dir, &mut none).unwrap_err();
        assert!(error.contains("no checksum"), "{error}");

        // No file for this system.
        let release = release_with(PORTABLE_LINUX, Some(hex(&file)), false);
        let error = download(&mut table(None), &source, &release, INSTALLER, &dir, &mut none).unwrap_err();
        assert!(error.contains("no download"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Against the real GitHub: the latest release reads, and a download
    /// through GitHub's redirect checks out. `cargo test real_github -- --ignored`
    #[test]
    #[ignore]
    fn real_github_release_reads_and_downloads() {
        let source = Source::github();
        let release = check(&mut Wire::new(), &source).unwrap();
        assert!(release.version >= Version(0, 2, 0), "{release:?}");
        for name in [INSTALLER, PORTABLE_WINDOWS, PORTABLE_LINUX, SUMS] {
            assert!(release.assets.iter().any(|asset| asset.name == name), "{name} missing");
        }
        let dir = std::env::temp_dir().join(format!("gitkrill-real-update-{}", std::process::id()));
        let saved = download(&mut Wire::new(), &source, &release, SUMS, &dir, &mut |_, _| {}).unwrap();
        let sums = std::fs::read_to_string(&saved).unwrap();
        assert!(sum_for(&sums, INSTALLER).is_some(), "{sums}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_kind_of_copy_updates_from_its_own_file() {
        assert_eq!(Kind::Installed.asset(), Some(INSTALLER));
        let portable = if cfg!(windows) { PORTABLE_WINDOWS } else { PORTABLE_LINUX };
        assert_eq!(Kind::Portable.asset(), Some(portable));
        assert_eq!(Kind::Browser.asset(), None);
        assert_eq!(Kind::Off.asset(), None);
    }
}
