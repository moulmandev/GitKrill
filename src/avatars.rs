//! Author avatars: real pictures from Gravatar and GitHub.
//!
//! The UI asks for a picture by e-mail address with [`Avatars::get`] while it
//! paints, and gets a texture back once one is loaded; until then (and when
//! there is none) it draws initials. Everything slow happens on a few
//! background threads: reading the disk cache, asking the network, decoding
//! and scaling. A frame only pays for one hash-map lookup per avatar drawn.
//!
//! * **Sources.** `<id>+<login>@users.noreply.github.com` and the older
//!   `<login>@users.noreply.github.com` are GitHub accounts; every other
//!   address is looked up on Gravatar by the SHA-256 of the trimmed,
//!   lower-cased address, with `d=404` so "no picture" is an answer.
//! * **Disk cache** (`<cache dir>/GitKrill/avatars`). Pictures are stored as
//!   64 px PNGs and "this address has no picture" as an empty marker file;
//!   both expire by age. A restart therefore shows pictures without the
//!   network and does not ask again for addresses known to have none.
//! * **Network manners.** An address the server has answered for is never
//!   requested again in the same session. Requests that never got an answer
//!   (offline, timeout) are retried later, sparingly: after a few failures in
//!   a row nothing is requested for a while.
//! * **Memory.** At most [`MAX_TEXTURES`] pictures are kept as textures; the
//!   least recently drawn ones are dropped and come back from the disk cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, ColorImage, TextureFilter, TextureHandle, TextureId, TextureOptions};
use sha2::{Digest, Sha256};

/// Side of the stored and displayed pictures, in pixels.
pub const SIZE: u32 = 64;
/// Pictures kept in memory at once (about 22 KB each with mipmaps).
pub const MAX_TEXTURES: usize = 256;
/// How long "this address has no picture" is believed.
pub const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// How old a cached picture may get before it is fetched again.
pub const HIT_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

const WORKERS: usize = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest response body accepted.
const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// How soon the UI asks again for an address the network could not be reached for.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// Unanswered requests in a row before the network is left alone for a while.
const BREAKER_FAILURES: u32 = 3;
const BREAKER_FIRST_PAUSE: Duration = Duration::from_secs(60);
const BREAKER_MAX_PAUSE: Duration = Duration::from_secs(30 * 60);
/// Unanswered requests for one address before it is given up on for the session.
const MAX_TRIES: u8 = 2;

const GITHUB_NOREPLY: &str = "@users.noreply.github.com";

// ---- where a picture comes from ---------------------------------------------

/// Where the picture for an address is looked up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Numeric GitHub user id, from `<id>+<login>@users.noreply.github.com`.
    GithubId(u64),
    /// GitHub login, from the older `<login>@users.noreply.github.com`.
    GithubLogin(String),
    /// Hex SHA-256 of the normalised address.
    Gravatar(String),
}

/// The form of an address that identifies its owner: surrounding whitespace
/// removed, lower case. Gravatar hashes exactly this.
pub fn normalize(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Hex SHA-256 of the normalised address.
pub fn gravatar_hash(email: &str) -> String {
    let digest = Sha256::digest(normalize(email).as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest.iter() {
        hex.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        hex.push(char::from_digit((byte & 15) as u32, 16).unwrap_or('0'));
    }
    hex
}

/// GitHub logins are 1 to 39 letters, digits or hyphens.
fn is_github_login(login: &str) -> bool {
    (1..=39).contains(&login.len()) && login.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Source {
    /// `None` when the text cannot be an e-mail address at all.
    pub fn of(email: &str) -> Option<Source> {
        let email = normalize(email);
        let (local, domain) = email.split_once('@')?;
        if local.is_empty() || domain.is_empty() {
            return None;
        }
        if let Some(local) = email.strip_suffix(GITHUB_NOREPLY) {
            match local.split_once('+') {
                Some((id, login)) => {
                    if let Ok(id) = id.parse::<u64>() {
                        return Some(Source::GithubId(id));
                    }
                    if is_github_login(login) {
                        return Some(Source::GithubLogin(login.to_owned()));
                    }
                }
                None if is_github_login(local) => return Some(Source::GithubLogin(local.to_owned())),
                None => {}
            }
        }
        Some(Source::Gravatar(gravatar_hash(&email)))
    }

    pub fn url(&self) -> String {
        match self {
            Source::GithubId(id) => format!("https://avatars.githubusercontent.com/u/{id}?s={SIZE}"),
            Source::GithubLogin(login) => format!("https://avatars.githubusercontent.com/{login}?s={SIZE}"),
            Source::Gravatar(hash) => format!("https://gravatar.com/avatar/{hash}?s={SIZE}&d=404"),
        }
    }

    /// File name stem in the disk cache; also the identity of the picture, so
    /// two spellings of one address share a download.
    pub fn key(&self) -> String {
        match self {
            Source::GithubId(id) => format!("github-id-{id}"),
            Source::GithubLogin(login) => format!("github-login-{login}"),
            Source::Gravatar(hash) => format!("gravatar-{hash}"),
        }
    }
}

// ---- disk cache -------------------------------------------------------------

/// What the disk cache knows about one picture.
#[derive(Debug, PartialEq, Eq)]
pub enum Cached {
    /// The stored PNG. `stale` once it is older than [`HIT_TTL`]: still good
    /// to show, but worth fetching again.
    Hit { png: Vec<u8>, stale: bool },
    /// Known to have no picture, and that knowledge has not expired.
    Miss,
    /// Never asked, or the answer has expired.
    Unknown,
}

pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        Cache { dir }
    }

    /// `GITKRILL_CACHE_DIR` if set; next to the settings when those were moved
    /// with `GITKRILL_CONFIG_DIR` (tests, scripted runs); else the user's
    /// cache directory.
    pub fn default_dir() -> Option<PathBuf> {
        let base = if let Some(dir) = std::env::var_os("GITKRILL_CACHE_DIR") {
            PathBuf::from(dir)
        } else if let Some(dir) = std::env::var_os("GITKRILL_CONFIG_DIR") {
            PathBuf::from(dir).join("cache")
        } else {
            dirs::cache_dir()?.join("GitKrill")
        };
        Some(base.join("avatars"))
    }

    fn picture(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.png"))
    }

    fn marker(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.none"))
    }

    /// Age of a file at `now`; `None` if it does not exist. A file from the
    /// future (clock change) counts as brand new.
    fn age(path: &Path, now: SystemTime) -> Option<Duration> {
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        Some(now.duration_since(modified).unwrap_or_default())
    }

    pub fn lookup(&self, key: &str, now: SystemTime) -> Cached {
        let picture = self.picture(key);
        if let Some(age) = Self::age(&picture, now)
            && let Ok(png) = std::fs::read(&picture)
        {
            return Cached::Hit { png, stale: age > HIT_TTL };
        }
        match Self::age(&self.marker(key), now) {
            Some(age) if age <= MISS_TTL => Cached::Miss,
            _ => Cached::Unknown,
        }
    }

    /// Remembers the picture. Failures are ignored: the cache is a bonus.
    pub fn store_hit(&self, key: &str, png: &[u8]) {
        let _ = std::fs::create_dir_all(&self.dir);
        // Written aside and renamed so a reader never sees half a file.
        let partial = self.dir.join(format!("{key}.png.{}.part", std::process::id()));
        if std::fs::write(&partial, png).is_ok() && std::fs::rename(&partial, self.picture(key)).is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        let _ = std::fs::remove_file(self.marker(key));
    }

    /// Remembers that there is no picture.
    pub fn store_miss(&self, key: &str) {
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::write(self.marker(key), b"");
        let _ = std::fs::remove_file(self.picture(key));
    }
}

// ---- pictures ---------------------------------------------------------------

/// Decodes a downloaded picture (PNG or JPEG) and scales it to a
/// [`SIZE`]-pixel square, cropping the longer side. `None` if it is not an
/// image we can read.
pub fn thumbnail(bytes: &[u8]) -> Option<image::RgbaImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    // Nothing a well-behaved avatar server sends comes close to these.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(96 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    if decoded.width() == 0 || decoded.height() == 0 {
        return None;
    }
    if decoded.width() == SIZE && decoded.height() == SIZE {
        return Some(decoded.into_rgba8());
    }
    Some(decoded.resize_to_fill(SIZE, SIZE, image::imageops::FilterType::CatmullRom).into_rgba8())
}

fn encode_png(picture: &image::RgbaImage) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    picture.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

fn to_color_image(picture: &image::RgbaImage) -> ColorImage {
    ColorImage::from_rgba_unmultiplied(
        [picture.width() as usize, picture.height() as usize],
        picture.as_raw(),
    )
}

// ---- background loading -----------------------------------------------------

struct Job {
    slot: u32,
    source: Source,
}

enum Outcome {
    Picture(ColorImage),
    /// No picture, and no point asking again this session.
    None,
    /// The network could not be reached; worth another try later.
    Later,
}

enum Fetched {
    Body(Vec<u8>),
    /// The server says this address has no picture.
    NotFound,
    /// The server answered with something else (rate limit, server error).
    Refused,
    /// No answer at all.
    Unreachable,
}

/// Network attempts for one picture this session.
#[derive(Clone, Copy)]
enum Tried {
    /// The server answered; never ask again.
    Answered,
    /// This many requests went unanswered.
    Unanswered(u8),
}

/// Stops requests for a while after several in a row went unanswered, so an
/// offline machine is not kept busy timing out.
struct Breaker {
    failures: u32,
    pause: Duration,
    closed_until: Option<Instant>,
}

impl Breaker {
    fn new() -> Self {
        Breaker { failures: 0, pause: BREAKER_FIRST_PAUSE, closed_until: None }
    }

    fn allows(&mut self, now: Instant) -> bool {
        match self.closed_until {
            Some(until) if now < until => false,
            _ => {
                self.closed_until = None;
                true
            }
        }
    }

    fn answered(&mut self) {
        self.failures = 0;
        self.pause = BREAKER_FIRST_PAUSE;
    }

    fn unanswered(&mut self, now: Instant) {
        self.failures += 1;
        if self.failures >= BREAKER_FAILURES {
            self.failures = 0;
            self.closed_until = Some(now + self.pause);
            self.pause = (self.pause * 2).min(BREAKER_MAX_PAUSE);
        }
    }
}

/// State shared between the UI side and the worker threads.
struct Pool {
    /// Pending jobs; the newest is served first, because it is the one most
    /// likely to still be on screen.
    jobs: Mutex<Vec<Job>>,
    wake: Condvar,
    quit: AtomicBool,
    /// Cleared when the user turns network avatars off.
    network: AtomicBool,
    cache: Option<Cache>,
    tried: Mutex<HashMap<String, Tried>>,
    breaker: Mutex<Breaker>,
    agent: ureq::Agent,
}

/// A poisoned lock only means another worker panicked; the data is still
/// usable for our purposes.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fetch(agent: &ureq::Agent, url: &str) -> Fetched {
    match agent.get(url).call() {
        Ok(mut response) => match response.body_mut().with_config().limit(MAX_BYTES).read_to_vec() {
            Ok(bytes) => Fetched::Body(bytes),
            Err(_) => Fetched::Unreachable,
        },
        Err(ureq::Error::StatusCode(404)) => Fetched::NotFound,
        Err(ureq::Error::StatusCode(_)) => Fetched::Refused,
        Err(_) => Fetched::Unreachable,
    }
}

fn worker(pool: Arc<Pool>, results: Sender<(u32, Outcome)>, ctx: egui::Context) {
    loop {
        let job = {
            let mut jobs = lock(&pool.jobs);
            loop {
                if pool.quit.load(Ordering::Relaxed) {
                    return;
                }
                match jobs.pop() {
                    Some(job) => break job,
                    None => jobs = pool.wake.wait(jobs).unwrap_or_else(|poisoned| poisoned.into_inner()),
                }
            }
        };
        let slot = job.slot;
        let mut answered = false;
        load(&pool, &job.source, &mut |outcome| {
            answered = true;
            if results.send((slot, outcome)).is_ok() {
                ctx.request_repaint();
            }
        });
        if !answered {
            // Every job must be answered or its slot would wait forever.
            let _ = results.send((slot, Outcome::None));
        }
    }
}

/// Produces the picture for `source`: from the disk cache if it is there,
/// from the network otherwise. `report` may be called twice, first with an
/// old cached picture and then with its replacement.
fn load(pool: &Pool, source: &Source, report: &mut dyn FnMut(Outcome)) {
    let key = source.key();
    let mut showing_old = false;
    if let Some(cache) = &pool.cache {
        match cache.lookup(&key, SystemTime::now()) {
            Cached::Hit { png, stale } => {
                if let Some(picture) = thumbnail(&png) {
                    report(Outcome::Picture(to_color_image(&picture)));
                    if !stale {
                        return;
                    }
                    showing_old = true;
                }
            }
            Cached::Miss => return report(Outcome::None),
            Cached::Unknown => {}
        }
    }
    // From here on a failure is only worth reporting if nothing is shown yet.
    let fail = |report: &mut dyn FnMut(Outcome), outcome: Outcome| {
        if !showing_old {
            report(outcome);
        }
    };

    if !pool.network.load(Ordering::Relaxed) {
        return fail(report, Outcome::Later);
    }
    let tries = match lock(&pool.tried).get(&key) {
        Some(Tried::Answered) => return fail(report, Outcome::None),
        Some(Tried::Unanswered(n)) if *n >= MAX_TRIES => return fail(report, Outcome::None),
        Some(Tried::Unanswered(n)) => *n,
        None => 0,
    };
    if !lock(&pool.breaker).allows(Instant::now()) {
        return fail(report, Outcome::Later);
    }

    let fetched = fetch(&pool.agent, &source.url());
    if matches!(fetched, Fetched::Unreachable) {
        lock(&pool.breaker).unanswered(Instant::now());
        lock(&pool.tried).insert(key, Tried::Unanswered(tries + 1));
        return fail(report, if tries + 1 >= MAX_TRIES { Outcome::None } else { Outcome::Later });
    }
    lock(&pool.breaker).answered();
    lock(&pool.tried).insert(key.clone(), Tried::Answered);

    match fetched {
        Fetched::Body(bytes) => match thumbnail(&bytes) {
            Some(picture) => {
                if let (Some(cache), Some(png)) = (&pool.cache, encode_png(&picture)) {
                    cache.store_hit(&key, &png);
                }
                report(Outcome::Picture(to_color_image(&picture)));
            }
            // Not an image we can show; do not download it again and again.
            None => {
                if let Some(cache) = &pool.cache
                    && !showing_old
                {
                    cache.store_miss(&key);
                }
                fail(report, Outcome::None);
            }
        },
        Fetched::NotFound => {
            if let Some(cache) = &pool.cache {
                cache.store_miss(&key);
            }
            // Also when an old picture is showing: its owner removed it.
            report(Outcome::None);
        }
        Fetched::Refused | Fetched::Unreachable => fail(report, Outcome::None),
    }
}

// ---- the store the UI talks to ------------------------------------------------

enum State {
    /// Not asked for yet, or dropped to save memory.
    Idle,
    Loading,
    Ready { texture: TextureHandle, last_used: u64 },
    /// No picture; initials it is.
    Missing,
    /// The network was unreachable; ask again once this time has come.
    Retry(Instant),
}

struct Slot {
    /// `None` for text that is not an address.
    source: Option<Source>,
    state: State,
}

pub struct Avatars {
    ctx: egui::Context,
    /// Addresses exactly as they appear in commits, so a lookup needs no
    /// normalising or hashing.
    by_email: HashMap<String, u32>,
    /// [`Source::key`] to slot: spellings of one address share a slot.
    by_key: HashMap<String, u32>,
    slots: Vec<Slot>,
    textures: usize,
    /// Frame on which results were last collected.
    frame: u64,
    pool: Arc<Pool>,
    results: Receiver<(u32, Outcome)>,
    /// Handed to the workers when they are started, on first use.
    sender: Option<Sender<(u32, Outcome)>>,
}

impl Avatars {
    pub fn new(ctx: &egui::Context) -> Self {
        Self::with_cache(ctx, Cache::default_dir().map(Cache::new))
    }

    fn with_cache(ctx: &egui::Context, cache: Option<Cache>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .https_only(true)
            .user_agent(concat!("GitKrill/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        let (sender, results) = channel();
        Avatars {
            ctx: ctx.clone(),
            by_email: HashMap::new(),
            by_key: HashMap::new(),
            slots: Vec::new(),
            textures: 0,
            frame: u64::MAX,
            pool: Arc::new(Pool {
                jobs: Mutex::new(Vec::new()),
                wake: Condvar::new(),
                quit: AtomicBool::new(false),
                network: AtomicBool::new(true),
                cache,
                tried: Mutex::new(HashMap::new()),
                breaker: Mutex::new(Breaker::new()),
                agent,
            }),
            results,
            sender: Some(sender),
        }
    }

    /// The picture for `email` if it is loaded; otherwise `None`, and the
    /// picture is requested in the background (the UI is repainted when it
    /// arrives). Call it only for avatars that are actually being drawn.
    ///
    /// `enabled` is the user's "show author pictures" setting: when off,
    /// nothing is read, requested or returned.
    pub fn get(&mut self, email: &str, enabled: bool) -> Option<TextureId> {
        if !enabled {
            // Also stops queued downloads at the disk cache.
            self.pool.network.store(false, Ordering::Relaxed);
            return None;
        }
        self.pool.network.store(true, Ordering::Relaxed);
        let frame = self.ctx.cumulative_frame_nr();
        if frame != self.frame {
            self.frame = frame;
            self.collect();
        }

        let index = match self.by_email.get(email) {
            Some(index) => *index,
            None => self.slot_for(email),
        };
        let slot = &mut self.slots[index as usize];
        match &mut slot.state {
            State::Ready { texture, last_used } => {
                *last_used = frame;
                return Some(texture.id());
            }
            State::Loading | State::Missing => return None,
            State::Retry(at) if Instant::now() < *at => return None,
            State::Idle | State::Retry(_) => {}
        }
        match slot.source.clone() {
            Some(source) => {
                slot.state = State::Loading;
                self.enqueue(Job { slot: index, source });
            }
            None => slot.state = State::Missing,
        }
        None
    }

    /// First sighting of this spelling of an address.
    fn slot_for(&mut self, email: &str) -> u32 {
        let source = Source::of(email);
        let key = source.as_ref().map(Source::key).unwrap_or_default();
        let index = match self.by_key.get(&key) {
            Some(index) => *index,
            None => {
                let index = self.slots.len() as u32;
                self.slots.push(Slot { source, state: State::Idle });
                self.by_key.insert(key, index);
                index
            }
        };
        self.by_email.insert(email.to_owned(), index);
        index
    }

    fn enqueue(&mut self, job: Job) {
        if let Some(sender) = self.sender.take() {
            for n in 0..WORKERS {
                let (pool, sender, ctx) = (self.pool.clone(), sender.clone(), self.ctx.clone());
                let spawned = std::thread::Builder::new()
                    .name(format!("avatars-{n}"))
                    .spawn(move || worker(pool, sender, ctx));
                if spawned.is_err() {
                    break;
                }
            }
        }
        lock(&self.pool.jobs).push(job);
        self.pool.wake.notify_one();
    }

    /// Turns finished downloads into textures.
    fn collect(&mut self) {
        while let Ok((index, outcome)) = self.results.try_recv() {
            let Some(slot) = self.slots.get_mut(index as usize) else { continue };
            if matches!(slot.state, State::Ready { .. }) {
                self.textures -= 1;
            }
            slot.state = match outcome {
                Outcome::Picture(image) => {
                    let options = TextureOptions::LINEAR.with_mipmap_mode(Some(TextureFilter::Linear));
                    let texture = self.ctx.load_texture(format!("avatar-{index}"), image, options);
                    self.textures += 1;
                    State::Ready { texture, last_used: self.frame }
                }
                Outcome::None => State::Missing,
                Outcome::Later => State::Retry(Instant::now() + RETRY_AFTER),
            };
        }
        while self.textures > MAX_TEXTURES {
            // Drop the picture that has gone unseen the longest. Should it
            // be drawn again, it is asked for again and comes off the disk.
            let oldest = self
                .slots
                .iter_mut()
                .filter_map(|slot| match slot.state {
                    State::Ready { last_used, .. } if last_used != self.frame => Some((last_used, slot)),
                    _ => None,
                })
                .min_by_key(|(last_used, _)| *last_used);
            match oldest {
                Some((_, slot)) => {
                    slot.state = State::Idle;
                    self.textures -= 1;
                }
                None => break,
            }
        }
    }
}

impl Drop for Avatars {
    fn drop(&mut self) {
        self.pool.quit.store(true, Ordering::Relaxed);
        self.pool.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_trimmed_and_lower_cased() {
        assert_eq!(normalize("  Ada.Lovelace@Example.COM \n"), "ada.lovelace@example.com");
        assert_eq!(normalize("ada@example.com"), "ada@example.com");
    }

    #[test]
    fn gravatar_hash_is_sha256_of_the_normalised_address() {
        // SHA-256 of the empty string and of "abc": the published test vectors.
        assert_eq!(
            gravatar_hash("   "),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            gravatar_hash("ABC"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Spelling does not matter.
        assert_eq!(gravatar_hash(" Ada@Example.com "), gravatar_hash("ada@example.com"));
        assert_ne!(gravatar_hash("ada@example.com"), gravatar_hash("grace@example.com"));
    }

    #[test]
    fn github_noreply_with_id_uses_the_numeric_id() {
        let source = Source::of("1024025+torvalds@users.noreply.github.com").unwrap();
        assert_eq!(source, Source::GithubId(1024025));
        assert_eq!(source.url(), "https://avatars.githubusercontent.com/u/1024025?s=64");
        assert_eq!(source.key(), "github-id-1024025");
        // Bots have ids too, and logins we could not put in a URL.
        assert_eq!(
            Source::of("49699333+dependabot[bot]@users.noreply.github.com"),
            Some(Source::GithubId(49699333))
        );
        // Case and whitespace are not significant.
        assert_eq!(
            Source::of(" 1024025+Torvalds@Users.NoReply.GitHub.com "),
            Some(Source::GithubId(1024025))
        );
    }

    #[test]
    fn older_github_noreply_uses_the_login() {
        let source = Source::of("Octocat@users.noreply.github.com").unwrap();
        assert_eq!(source, Source::GithubLogin("octocat".to_owned()));
        assert_eq!(source.url(), "https://avatars.githubusercontent.com/octocat?s=64");
        assert_eq!(source.key(), "github-login-octocat");
        // A non-numeric prefix still leaves a usable login.
        assert_eq!(
            Source::of("x+mona-lisa@users.noreply.github.com"),
            Some(Source::GithubLogin("mona-lisa".to_owned()))
        );
    }

    #[test]
    fn other_addresses_go_to_gravatar() {
        let source = Source::of("Ada@Example.com").unwrap();
        let hash = gravatar_hash("ada@example.com");
        assert_eq!(source, Source::Gravatar(hash.clone()));
        assert_eq!(source.url(), format!("https://gravatar.com/avatar/{hash}?s=64&d=404"));
        assert_eq!(source.key(), format!("gravatar-{hash}"));
        // Not a GitHub account name: nothing that could end up in a URL path.
        assert!(matches!(
            Source::of("some/../thing[bot]@users.noreply.github.com"),
            Some(Source::Gravatar(_))
        ));
        // A look-alike domain is not GitHub.
        assert!(matches!(Source::of("a@users.noreply.github.com.evil.org"), Some(Source::Gravatar(_))));
    }

    #[test]
    fn text_that_is_not_an_address_has_no_source() {
        assert_eq!(Source::of(""), None);
        assert_eq!(Source::of("   "), None);
        assert_eq!(Source::of("nobody"), None);
        assert_eq!(Source::of("@example.com"), None);
        assert_eq!(Source::of("ada@"), None);
    }

    fn temp_cache(name: &str) -> Cache {
        let dir = std::env::temp_dir()
            .join("gitkrill-tests")
            .join(format!("avatars-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Cache::new(dir)
    }

    #[test]
    fn cache_remembers_pictures_and_marks_old_ones_stale() {
        let cache = temp_cache("hit");
        let now = SystemTime::now();
        assert_eq!(cache.lookup("github-id-1", now), Cached::Unknown);

        cache.store_hit("github-id-1", b"png bytes");
        assert_eq!(
            cache.lookup("github-id-1", now),
            Cached::Hit { png: b"png bytes".to_vec(), stale: false }
        );
        // Other keys are unaffected.
        assert_eq!(cache.lookup("github-id-2", now), Cached::Unknown);

        // Still shown after a month, but due for a refresh.
        let later = now + HIT_TTL + Duration::from_secs(3600);
        assert_eq!(
            cache.lookup("github-id-1", later),
            Cached::Hit { png: b"png bytes".to_vec(), stale: true }
        );
        let _ = std::fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn cache_remembers_misses_until_they_expire() {
        let cache = temp_cache("miss");
        let now = SystemTime::now();
        cache.store_miss("gravatar-abc");
        assert_eq!(cache.lookup("gravatar-abc", now), Cached::Miss);
        assert_eq!(cache.lookup("gravatar-abc", now + MISS_TTL - Duration::from_secs(3600)), Cached::Miss);
        // After the expiry the address is worth asking about again.
        assert_eq!(
            cache.lookup("gravatar-abc", now + MISS_TTL + Duration::from_secs(3600)),
            Cached::Unknown
        );
        let _ = std::fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn a_new_answer_replaces_the_old_one() {
        let cache = temp_cache("replace");
        let now = SystemTime::now();
        cache.store_miss("k");
        cache.store_hit("k", b"picture");
        assert!(matches!(cache.lookup("k", now), Cached::Hit { .. }));
        // The owner removed their picture.
        cache.store_miss("k");
        assert_eq!(cache.lookup("k", now), Cached::Miss);
        let _ = std::fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn a_cache_that_cannot_be_written_is_harmless() {
        // A file where the directory should be: every write fails quietly.
        let cache = temp_cache("blocked");
        std::fs::create_dir_all(cache.dir.parent().unwrap()).unwrap();
        std::fs::write(&cache.dir, b"in the way").unwrap();
        cache.store_hit("k", b"picture");
        cache.store_miss("k");
        assert_eq!(cache.lookup("k", SystemTime::now()), Cached::Unknown);
        let _ = std::fs::remove_file(&cache.dir);
    }

    #[test]
    fn pictures_are_scaled_and_cropped_to_a_square() {
        // A wide red picture with a blue right half.
        let mut wide = image::RgbaImage::from_pixel(200, 100, image::Rgba([255, 0, 0, 255]));
        for x in 100..200 {
            for y in 0..100 {
                wide.put_pixel(x, y, image::Rgba([0, 0, 255, 255]));
            }
        }
        let png = encode_png(&wide).unwrap();
        let small = thumbnail(&png).unwrap();
        assert_eq!((small.width(), small.height()), (SIZE, SIZE));
        // The crop keeps the middle: red on the left, blue on the right.
        assert_eq!(small.get_pixel(2, 32).0, [255, 0, 0, 255]);
        assert_eq!(small.get_pixel(61, 32).0, [0, 0, 255, 255]);

        // The stored thumbnail reads back unchanged.
        let again = thumbnail(&encode_png(&small).unwrap()).unwrap();
        assert_eq!(again.as_raw(), small.as_raw());
    }

    #[test]
    fn garbage_is_not_a_picture() {
        assert!(thumbnail(b"").is_none());
        assert!(thumbnail(b"<html>rate limited</html>").is_none());
        // A PNG signature with nothing behind it.
        assert!(thumbnail(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0]).is_none());
    }

    #[test]
    fn the_network_is_left_alone_after_repeated_failures() {
        let mut breaker = Breaker::new();
        let start = Instant::now();
        assert!(breaker.allows(start));
        for _ in 0..BREAKER_FAILURES - 1 {
            breaker.unanswered(start);
            assert!(breaker.allows(start));
        }
        breaker.unanswered(start);
        assert!(!breaker.allows(start + Duration::from_secs(1)));
        assert!(breaker.allows(start + BREAKER_FIRST_PAUSE));

        // Still offline: the next pause is longer.
        let again = start + BREAKER_FIRST_PAUSE;
        for _ in 0..BREAKER_FAILURES {
            breaker.unanswered(again);
        }
        assert!(!breaker.allows(again + BREAKER_FIRST_PAUSE + Duration::from_secs(1)));
        assert!(breaker.allows(again + BREAKER_FIRST_PAUSE * 2));

        // One answer and the slate is clean.
        breaker.answered();
        breaker.unanswered(again);
        assert!(breaker.allows(again));
    }

    #[test]
    fn only_so_many_pictures_are_kept_in_memory() {
        let ctx = egui::Context::default();
        let mut avatars = Avatars::with_cache(&ctx, None);
        let sender = avatars.sender.clone().unwrap();
        let picture = || ColorImage::filled([2, 2], egui::Color32::WHITE);

        // Frame 1 draws a full house of pictures.
        avatars.frame = 1;
        for index in 0..MAX_TEXTURES as u32 {
            avatars.slots.push(Slot { source: None, state: State::Loading });
            sender.send((index, Outcome::Picture(picture()))).unwrap();
        }
        avatars.collect();
        assert_eq!(avatars.textures, MAX_TEXTURES);

        // Frame 2 keeps drawing all but the first, and three more arrive.
        avatars.frame = 2;
        for slot in &mut avatars.slots[1..] {
            if let State::Ready { last_used, .. } = &mut slot.state {
                *last_used = 2;
            }
        }
        for index in 0..3 {
            avatars.slots.push(Slot { source: None, state: State::Loading });
            sender.send((MAX_TEXTURES as u32 + index, Outcome::Picture(picture()))).unwrap();
        }
        avatars.collect();
        // The one nobody looked at went; what is on screen is never dropped,
        // even if that means going over for a moment.
        assert!(matches!(avatars.slots[0].state, State::Idle));
        assert_eq!(avatars.textures, MAX_TEXTURES + 2);

        // Frame 3 scrolled away from everything: back under the limit.
        avatars.frame = 3;
        avatars.collect();
        assert_eq!(avatars.textures, MAX_TEXTURES);
        let ready = avatars.slots.iter().filter(|slot| matches!(slot.state, State::Ready { .. })).count();
        assert_eq!(ready, MAX_TEXTURES);
    }

    /// The whole path without a network: pictures and known misses come out
    /// of the disk cache, spellings of one address share a slot, and nothing
    /// is requested when the setting is off.
    #[test]
    fn pictures_are_served_from_the_disk_cache() {
        let cache = temp_cache("store");
        let picture = image::RgbaImage::from_pixel(SIZE, SIZE, image::Rgba([10, 200, 30, 255]));
        let with_picture = "583231+octocat@users.noreply.github.com";
        cache.store_hit(&Source::of(with_picture).unwrap().key(), &encode_png(&picture).unwrap());
        let without = "nobody@example.com";
        cache.store_miss(&Source::of(without).unwrap().key());
        let dir = cache.dir.clone();

        let ctx = egui::Context::default();
        let mut avatars = Avatars::with_cache(&ctx, Some(cache));
        // Anything not in the cache must not reach the network from a test.
        avatars.pool.breaker.lock().unwrap().closed_until = Some(Instant::now() + Duration::from_secs(3600));

        // Off: nothing happens at all.
        assert_eq!(avatars.get(with_picture, false), None);
        assert!(avatars.slots.is_empty());

        assert_eq!(avatars.get(with_picture, true), None, "loads in the background");
        assert_eq!(avatars.get(without, true), None);
        assert_eq!(avatars.get("", true), None);
        assert_eq!(avatars.get("unknown@example.com", true), None);
        // Another spelling of the same address does not start a second load.
        assert_eq!(avatars.get(" 583231+Octocat@users.noreply.github.com", true), None);
        assert_eq!(avatars.slots.len(), 4);

        // Wait for the workers, then collect as a new frame would.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            avatars.collect();
            let loading = avatars.slots.iter().any(|slot| matches!(slot.state, State::Loading));
            if !loading {
                break;
            }
            assert!(Instant::now() < deadline, "avatar workers did not answer");
            std::thread::sleep(Duration::from_millis(10));
        }

        let texture = avatars.get(with_picture, true);
        assert!(texture.is_some());
        assert_eq!(avatars.get(" 583231+Octocat@users.noreply.github.com", true), texture);
        assert_eq!(avatars.textures, 1);
        assert_eq!(avatars.get(without, true), None);
        assert!(matches!(avatars.slots[avatars.by_email[without] as usize].state, State::Missing));
        assert!(matches!(avatars.slots[avatars.by_email[""] as usize].state, State::Missing));
        // Unknown and unreachable: to be tried again later, not written off.
        assert!(matches!(
            avatars.slots[avatars.by_email["unknown@example.com"] as usize].state,
            State::Retry(_)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
