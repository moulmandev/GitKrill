//! Hosting-service integration: pull requests, issues and CI runs for the
//! repository a remote points at.
//!
//! * The model ([`PullRequest`], [`Issue`], [`Run`], [`Feed`], [`Problem`]),
//!   the rate-limit bookkeeping ([`Limiter`]) and the per-tab
//!   [`HostingState`] know nothing about a particular service.
//! * [`github`] holds everything GitHub-specific: recognising remote URLs,
//!   finding a token, the REST endpoints and their JSON. Another service is a
//!   sibling module implementing [`Backend`], plus a [`Service`] variant.
//!
//! All network and process work happens on one background thread per tab
//! (`worker`); the UI thread only sends it orders and drains its events.
//! Nothing in this module logs or writes to disk, and a token is only ever
//! attached to a request by `github::Wire`.
//!
//! Development aid: with `GITKRILL_HOSTING_FIXTURE=<dir>` set, answers come
//! from `<dir>/pulls.json`, `issues.json` and `runs.json` instead of the
//! network (see `Canned`) and no token is looked for, so screenshots of
//! every state can be taken offline.

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;

use crate::git::RemoteInfo;
use crate::git::ops::{GitCmd, UndoKind};

// ---- what is hosted where -------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    GitHub,
}

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Service::GitHub => "GitHub",
        }
    }

    /// Sidebar title of the CI section.
    pub fn ci_title(self) -> &'static str {
        match self {
            Service::GitHub => "GITHUB ACTIONS",
        }
    }

    fn web(self) -> &'static str {
        match self {
            Service::GitHub => "https://github.com",
        }
    }

    /// The ref on the remote that follows a pull request's head commit.
    fn pull_ref(self, number: u64) -> String {
        match self {
            Service::GitHub => format!("pull/{number}/head"),
        }
    }
}

/// A repository on a hosting service, as named by a remote URL.
///
/// `owner` and `name` only ever contain letters, digits, `.`, `_` and `-`
/// (the parsers reject everything else), so the links built here are safe to
/// hand to the operating system as they are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoId {
    pub service: Service,
    pub owner: String,
    pub name: String,
}

impl RepoId {
    /// `owner/name`
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    fn web_url(&self) -> String {
        format!("{}/{}/{}", self.service.web(), self.owner, self.name)
    }

    pub fn pulls_url(&self) -> String {
        format!("{}/pulls", self.web_url())
    }

    pub fn pull_url(&self, number: u64) -> String {
        format!("{}/pull/{number}", self.web_url())
    }

    pub fn issues_url(&self) -> String {
        format!("{}/issues", self.web_url())
    }

    pub fn issue_url(&self, number: u64) -> String {
        format!("{}/issues/{number}", self.web_url())
    }

    pub fn runs_url(&self) -> String {
        format!("{}/actions", self.web_url())
    }

    pub fn run_url(&self, id: u64) -> String {
        format!("{}/actions/runs/{id}", self.web_url())
    }
}

/// The hosted repository a remote URL points at, if it is on a service we know.
pub fn parse_remote(url: &str) -> Option<RepoId> {
    github::parse_remote(url)
}

/// Picks the remote to show: `origin` when it is hosted, else the first
/// remote that is. Returns the remote's name with the repository.
pub fn detect(remotes: &[RemoteInfo]) -> Option<(String, RepoId)> {
    let hosted = |remote: &RemoteInfo| Some((remote.name.clone(), parse_remote(&remote.url)?));
    remotes
        .iter()
        .find(|remote| remote.name == "origin")
        .and_then(hosted)
        .or_else(|| remotes.iter().find_map(hosted))
}

// ---- model ----------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub draft: bool,
    /// Branch the changes come from, and the commit at its tip.
    pub head_ref: String,
    pub head_sha: String,
    /// `owner/name` of the repository the head branch lives in. `None` when
    /// the fork it came from has been deleted.
    pub head_repo: Option<String>,
    pub base_ref: String,
    /// Seconds since the epoch; 0 when unknown.
    pub updated: i64,
}

impl PullRequest {
    /// Whether the head branch lives in `repo` itself rather than in a fork.
    pub fn is_from(&self, repo: &RepoId) -> bool {
        self.head_repo.as_deref().is_some_and(|full| full.eq_ignore_ascii_case(&repo.slug()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub labels: Vec<String>,
    pub comments: u64,
    pub updated: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Queued,
    Running,
    Success,
    Failure,
    Cancelled,
    /// Finished without a verdict: skipped, neutral or stale.
    Skipped,
    /// Waiting for someone to approve it.
    NeedsAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub id: u64,
    /// Workflow name ("CI").
    pub workflow: String,
    /// What triggered it, usually the commit or pull request title.
    pub title: String,
    pub number: u64,
    pub branch: String,
    pub event: String,
    pub head_sha: String,
    pub state: RunState,
    /// The service's own word for the state ("timed out", "in progress").
    pub outcome: String,
    /// When the run was created, seconds since the epoch; 0 when unknown.
    pub when: i64,
}

/// Combined CI verdict for one commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checks {
    Passing,
    Failing,
    Pending,
}

/// What the recent CI runs say about the commit `head_sha`: any failure
/// wins, then anything still in flight, then success. `None` when no run we
/// know of is about that commit.
pub fn checks_for(head_sha: &str, runs: &[Run]) -> Option<Checks> {
    let mut verdict = None;
    for run in runs.iter().filter(|run| !head_sha.is_empty() && run.head_sha == head_sha) {
        verdict = match (run.state, verdict) {
            (RunState::Failure, _) | (_, Some(Checks::Failing)) => Some(Checks::Failing),
            (RunState::Queued | RunState::Running | RunState::NeedsAction, _)
            | (_, Some(Checks::Pending)) => Some(Checks::Pending),
            (RunState::Success, _) => Some(Checks::Passing),
            (RunState::Cancelled | RunState::Skipped, kept) => kept,
        };
    }
    verdict
}

/// Why a list could not be refreshed. Shown as one dim line in its section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// No request before this time (seconds since the epoch).
    RateLimited { until: u64 },
    /// No token, and the service will not show the repository without one.
    NotSignedIn,
    /// Every token we found was refused, and anonymous access is not enough.
    SignInRejected,
    /// Signed in, but the account cannot see the repository (or it is gone).
    NoAccess,
    /// The service said no, in its own words (not a rate limit).
    Refused(String),
    /// The request never got an answer.
    Unreachable(String),
    /// Anything else: an unexpected status or an answer we cannot read.
    Failed(String),
}

impl Problem {
    /// The one-line summary.
    pub fn line(&self, service: Service) -> String {
        let host = service.name();
        match self {
            Problem::RateLimited { until } => format!("Rate limited until {}", clock(*until)),
            Problem::NotSignedIn => "Not signed in: private repository".to_owned(),
            Problem::SignInRejected => "Sign-in refused: private repository".to_owned(),
            Problem::NoAccess => "No access with this sign-in".to_owned(),
            Problem::Refused(message) => format!("{host} refused: {message}"),
            Problem::Unreachable(message) => format!("{host} unreachable: {message}"),
            Problem::Failed(message) => message.clone(),
        }
    }

    /// The summary again in two or three words, for when the sidebar is too
    /// narrow for [`Problem::line`].
    pub fn brief(&self, service: Service) -> String {
        let host = service.name();
        match self {
            Problem::RateLimited { until } => format!("Limited until {}", clock(*until)),
            Problem::NotSignedIn => "Not signed in".to_owned(),
            Problem::SignInRejected => "Sign-in refused".to_owned(),
            Problem::NoAccess => "No access".to_owned(),
            Problem::Refused(_) => format!("{host} refused"),
            Problem::Unreachable(_) => format!("{host} unreachable"),
            Problem::Failed(_) => "Refresh failed".to_owned(),
        }
    }

    /// The summary plus what to do about it, for a tooltip.
    pub fn detail(&self, service: Service) -> String {
        let host = service.name();
        let sign_in = "Set GITHUB_TOKEN, run `gh auth login`, or sign in through Git's \
                       credential manager (any HTTPS fetch or push does it), then refresh.";
        let advice = match self {
            Problem::RateLimited { .. } => format!(
                "{host} asked us to slow down. What is shown is the last good answer; \
                 refreshing resumes by itself. Signing in raises the limit."
            ),
            Problem::NotSignedIn => format!(
                "{host} only shows public repositories without a sign-in, and it does not \
                 show this one. {sign_in}"
            ),
            Problem::SignInRejected => format!(
                "{host} refused the saved sign-in (expired or revoked?), and does not show \
                 this repository without one. {sign_in}"
            ),
            Problem::NoAccess => format!(
                "{host} answered \"not found\": the repository is gone, or the signed-in \
                 account is not allowed to see it."
            ),
            Problem::Refused(_) | Problem::Unreachable(_) | Problem::Failed(_) => {
                "What is shown is the last good answer. Click to try again.".to_owned()
            }
        };
        format!("{}\n\n{advice}", self.line(service))
    }

    /// The same problem with every occurrence of `tokens` blanked out. Every
    /// problem passes through here before it leaves the backend, so no
    /// message can carry a token to the screen whatever the server or the
    /// network stack put in it.
    fn scrubbed(self, tokens: &[&Token]) -> Problem {
        let clean = |text: String| tidy(&tokens.iter().fold(text, |text, token| token.scrub(&text)));
        match self {
            Problem::Refused(text) => Problem::Refused(clean(text)),
            Problem::Unreachable(text) => Problem::Unreachable(clean(text)),
            Problem::Failed(text) => Problem::Failed(clean(text)),
            other => other,
        }
    }
}

/// One line, no control characters, bounded length: fit for a sidebar row.
fn tidy(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().filter(|c| !c.is_control()).take(160).collect();
    if flat.chars().count() > 160 {
        out.push('…');
    }
    out
}

/// The result of asking for one list.
#[derive(Debug, PartialEq)]
pub enum Outcome<T> {
    Fresh {
        items: Vec<T>,
        /// The service has more than it sent.
        more: bool,
    },
    /// Nothing changed since the last answer (a conditional request hit).
    Unchanged,
    Failed(Problem),
}

/// One list as the UI shows it: the last good answer plus what went wrong
/// since, if anything.
#[derive(Clone, Debug)]
pub struct Feed<T> {
    pub items: Vec<T>,
    pub more: bool,
    /// At least one answer has arrived.
    pub loaded: bool,
    pub problem: Option<Problem>,
}

impl<T> Default for Feed<T> {
    fn default() -> Self {
        Feed { items: Vec::new(), more: false, loaded: false, problem: None }
    }
}

impl<T> Feed<T> {
    pub fn apply(&mut self, outcome: Outcome<T>) {
        match outcome {
            Outcome::Fresh { items, more } => {
                *self = Feed { items, more, loaded: true, problem: None };
            }
            Outcome::Unchanged => self.problem = None,
            // Keep showing the last good answer.
            Outcome::Failed(problem) => self.problem = Some(problem),
        }
    }
}

/// Where the token in use came from. Never the token itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenSource {
    Env(&'static str),
    Cli,
    CredentialHelper,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Auth {
    /// Nothing has been requested yet.
    #[default]
    Unknown,
    Anonymous,
    SignedIn(TokenSource),
    /// Tokens were found but the service refused them; anonymous since.
    Rejected,
}

impl Auth {
    pub fn describe(self) -> String {
        match self {
            Auth::Unknown => String::new(),
            Auth::Anonymous => "Not signed in: public repositories only".to_owned(),
            Auth::SignedIn(TokenSource::Env(name)) => format!("Signed in with ${name}"),
            Auth::SignedIn(TokenSource::Cli) => "Signed in through the GitHub CLI".to_owned(),
            Auth::SignedIn(TokenSource::CredentialHelper) => {
                "Signed in through Git's credential helper".to_owned()
            }
            Auth::Rejected => "The saved sign-in was refused: public repositories only".to_owned(),
        }
    }
}

/// Everything the sidebar shows for one repository.
#[derive(Clone, Default)]
pub struct View {
    pub pulls: Feed<PullRequest>,
    pub issues: Feed<Issue>,
    pub runs: Feed<Run>,
    pub auth: Auth,
    /// A refresh is in flight.
    pub busy: bool,
    /// When the last refresh finished, seconds since the epoch.
    pub updated: Option<u64>,
}

// ---- time -----------------------------------------------------------------

pub fn epoch_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `14:32` in local time.
pub fn clock(epoch: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(epoch as i64, 0).single() {
        Some(time) => time.format("%H:%M").to_string(),
        None => "later".to_owned(),
    }
}

/// How long ago `then` was, short enough for a sidebar row: `5m`, `3h`, `2d`.
pub fn ago(then: i64, now: i64) -> String {
    let secs = now - then;
    if then <= 0 || secs < 0 {
        String::new()
    } else if secs < 60 {
        "now".to_owned()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 86_400 * 30 {
        format!("{}d", secs / 86_400)
    } else if secs < 86_400 * 365 {
        format!("{}mo", secs / (86_400 * 30))
    } else {
        format!("{}y", secs / (86_400 * 365))
    }
}

/// RFC 3339 (`2026-11-10T12:00:00Z`) to seconds since the epoch; 0 if absent
/// or malformed.
fn timestamp(text: Option<&str>) -> i64 {
    text.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map_or(0, |t| t.timestamp())
}

// ---- tokens ---------------------------------------------------------------

/// An access token. Deliberately opaque: it cannot be printed, compared
/// against text or serialised, only attached to a request by a [`Transport`].
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// Accepts anything that can be a token: printable ASCII without spaces.
    fn new(raw: &str) -> Option<Token> {
        let raw = raw.trim();
        let plausible = (8..=1024).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_graphic());
        plausible.then(|| Token(raw.to_owned()))
    }

    fn authorization(&self) -> String {
        format!("Bearer {}", self.0)
    }

    /// `text` with the token blanked out.
    fn scrub(&self, text: &str) -> String {
        text.replace(&self.0, "***")
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(***)")
    }
}

/// The places a token can come from. A trait so the lookup order can be
/// tested without touching the real environment.
pub trait Secrets: Send {
    fn env(&self, name: &str) -> Option<String>;
    /// The service's own command line tool, when installed and signed in.
    fn cli(&self) -> Option<String>;
    /// Git's credential helper, asked in a way that can never prompt.
    fn credential_helper(&self) -> Option<String>;
}

/// Looks for a token, cheapest and most explicit source first. Tokens in
/// `rejected` (already refused by the service) are passed over, so a stale
/// environment variable does not hide a working sign-in further down.
pub fn find_token(secrets: &dyn Secrets, rejected: &[Token]) -> Option<(Token, TokenSource)> {
    let usable = |raw: Option<String>| {
        raw.and_then(|raw| Token::new(&raw)).filter(|token| !rejected.contains(token))
    };
    for name in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Some(token) = usable(secrets.env(name)) {
            return Some((token, TokenSource::Env(name)));
        }
    }
    if let Some(token) = usable(secrets.cli()) {
        return Some((token, TokenSource::Cli));
    }
    usable(secrets.credential_helper()).map(|token| (token, TokenSource::CredentialHelper))
}

/// No token from anywhere: for the offline fixture.
struct NoSecrets;

impl Secrets for NoSecrets {
    fn env(&self, _name: &str) -> Option<String> {
        None
    }
    fn cli(&self) -> Option<String> {
        None
    }
    fn credential_helper(&self) -> Option<String> {
        None
    }
}

/// Runs a helper program to completion and returns its standard output, or
/// `None` if it is missing, fails or takes longer than `timeout`. It gets no
/// console window, and its error output is discarded unread.
fn capture(mut cmd: Command, stdin: &[u8], timeout: Duration) -> Option<String> {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    crate::git::cli::hide_window(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    if let Some(mut pipe) = child.stdin.take() {
        let _ = pipe.write_all(stdin);
    }

    // Read on a thread of its own: a helper can leave a grandchild holding
    // the pipe open long after it has answered and exited.
    let mut stdout = child.stdout.take()?;
    let collected = Arc::new(Mutex::new(Vec::new()));
    let (finished_tx, finished_rx) = channel();
    {
        let collected = collected.clone();
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut all) = collected.lock() {
                    all.extend_from_slice(&chunk[..n]);
                }
            }
            let _ = finished_tx.send(());
        });
    }

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let _ = finished_rx.recv_timeout(Duration::from_millis(500));
    let bytes = std::mem::take(&mut *collected.lock().ok()?);
    String::from_utf8(bytes).ok()
}

// ---- transport ------------------------------------------------------------

pub struct HttpRequest<'a> {
    pub url: &'a str,
    /// Sent as `If-None-Match`, so an unchanged list costs no transfer.
    pub etag: Option<&'a str>,
    pub token: Option<&'a Token>,
}

#[derive(Clone, Debug, Default)]
pub struct HttpResponse {
    pub status: u16,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// One HTTP GET. Any status is a response; `Err` means no answer at all.
pub trait Transport: Send {
    fn get(&mut self, request: &HttpRequest<'_>) -> Result<HttpResponse, String>;
}

/// Development stand-in for the network: answers from files in a folder.
///
/// `<list>.json` is the body for `pulls`, `issues` and `runs`. An optional
/// `<list>.head` gives a status code on its first line and `name: value`
/// headers after it, which is how to stage a rate limit or an outage.
struct Canned {
    dir: PathBuf,
}

impl Transport for Canned {
    fn get(&mut self, request: &HttpRequest<'_>) -> Result<HttpResponse, String> {
        let path = request.url.split('?').next().unwrap_or_default();
        let list = ["pulls", "issues", "runs"].into_iter().find(|list| path.ends_with(list));
        let list = list.ok_or("no fixture for this address")?;

        let mut response = HttpResponse { status: 200, ..Default::default() };
        if let Ok(head) = std::fs::read_to_string(self.dir.join(format!("{list}.head"))) {
            let mut lines = head.lines();
            response.status = lines.next().and_then(|line| line.trim().parse().ok()).unwrap_or(200);
            response.headers = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
                .collect();
        }
        match std::fs::read_to_string(self.dir.join(format!("{list}.json"))) {
            Ok(body) => response.body = body,
            Err(_) if response.status == 200 => response.status = 404,
            Err(_) => {}
        }
        Ok(response)
    }
}

// ---- rate limits and backoff ----------------------------------------------

/// What a response's headers say about our request budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rate {
    /// Requests left in the current window.
    pub remaining: Option<u64>,
    /// When the window resets, seconds since the epoch.
    pub reset: Option<u64>,
    /// Seconds the server asked us to stay away.
    pub retry_after: Option<u64>,
}

impl Rate {
    pub fn of(response: &HttpResponse) -> Rate {
        let number = |name: &str| response.header(name).and_then(|v| v.trim().parse().ok());
        Rate {
            remaining: number("x-ratelimit-remaining"),
            reset: number("x-ratelimit-reset"),
            retry_after: number("retry-after"),
        }
    }
}

/// Wait after the `strikes`-th consecutive refusal that came without a
/// usable hint: one minute, doubling, capped at an hour.
pub fn backoff(strikes: u32) -> u64 {
    (60u64 << strikes.min(6)).min(3600)
}

/// If a response is the service telling us to slow down, the time (seconds
/// since the epoch) before which we must not ask again; `None` otherwise.
///
/// 429 always is. 403 is when the budget is spent, when a `Retry-After`
/// came with it, or when the message says so (secondary limits come without
/// the headers); any other 403 is a plain "forbidden".
pub fn limited_until(status: u16, rate: &Rate, message: &str, strikes: u32, now: u64) -> Option<u64> {
    let message = message.to_ascii_lowercase();
    let says_so = message.contains("rate limit") || message.contains("abuse");
    let limited = status == 429
        || (status == 403 && (rate.remaining == Some(0) || rate.retry_after.is_some() || says_so));
    if !limited {
        return None;
    }
    let asked = match rate.retry_after {
        Some(seconds) => Some(now + seconds),
        None => rate.reset.filter(|_| rate.remaining == Some(0)),
    };
    Some(match asked {
        // Never trust a hint for more than a day.
        Some(until) if until > now => until.min(now + 86_400),
        // No hint, or one already in the past (a skewed clock): back off.
        _ => now + backoff(strikes),
    })
}

/// Remembers when the service last told us to stay away.
#[derive(Debug, Default)]
pub struct Limiter {
    until: Option<u64>,
    /// Consecutive rate-limit refusals.
    strikes: u32,
}

impl Limiter {
    /// The time requests are held back until, if they still are.
    pub fn blocked(&self, now: u64) -> Option<u64> {
        self.until.filter(|until| *until > now)
    }

    /// Takes note of a response. Returns the time to stay away until when
    /// the response was a rate-limit refusal.
    pub fn record(&mut self, status: u16, rate: &Rate, message: &str, now: u64) -> Option<u64> {
        if let Some(until) = limited_until(status, rate, message, self.strikes, now) {
            self.strikes += 1;
            self.until = Some(until);
            return Some(until);
        }
        if status < 400 {
            self.strikes = 0;
            // That was the last request of the window: the next one would
            // only be refused, so do not send it.
            self.until = rate.reset.filter(|reset| rate.remaining == Some(0) && *reset > now);
        }
        None
    }
}

/// How long to wait before the next automatic refresh.
///
/// Signed in, an unchanged list costs nothing (conditional requests are
/// free), so three minutes is polite. Anonymous access gets 60 requests an
/// hour in total and a refresh is three of them: ten minutes. Rounds that
/// fail stretch the wait, and a rate limit is waited out in full.
pub fn next_refresh(signed_in: bool, blocked_until: Option<u64>, failures: u32, now: u64) -> Duration {
    let base: u64 = if signed_in { 180 } else { 600 };
    let wait = (base << failures.min(4)).clamp(base, 1800);
    let wait = match blocked_until {
        Some(until) if until > now => wait.max(until - now + 5),
        _ => wait,
    };
    Duration::from_secs(wait)
}

// ---- backends -------------------------------------------------------------

/// A hosting service's API for one repository. Runs on the worker thread;
/// every method may block on the network.
pub trait Backend: Send {
    fn pulls(&mut self, now: u64) -> Outcome<PullRequest>;
    fn issues(&mut self, now: u64) -> Outcome<Issue>;
    fn runs(&mut self, now: u64) -> Outcome<Run>;
    fn auth(&self) -> Auth;
    /// The user asked for a refresh: worth looking for a sign-in again.
    fn look_again(&mut self);
    fn blocked_until(&self, now: u64) -> Option<u64>;
    /// Open a pull request (a merge request on GitLab). Blocking: called on
    /// the hosting worker thread, never on the UI thread.
    fn create_pull(&mut self, new: &NewPull, now: u64) -> Result<PullRequest, Problem> {
        let _ = (new, now);
        Err(Problem::Failed("Opening pull requests is not supported here yet".to_owned()))
    }
}

/// What the "Create pull request" dialog asks the service for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPull {
    /// Branch name on the remote that holds the changes.
    pub head: String,
    /// Branch the changes should land in.
    pub base: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
}

fn connect(repo: &RepoId, workdir: &Path) -> Box<dyn Backend> {
    match repo.service {
        Service::GitHub => match std::env::var_os("GITKRILL_HOSTING_FIXTURE") {
            Some(dir) => Box::new(github::Client::new(
                repo.clone(),
                Box::new(Canned { dir: dir.into() }),
                Box::new(NoSecrets),
            )),
            None => Box::new(github::Client::new(
                repo.clone(),
                Box::new(github::Wire::new()),
                Box::new(github::System::new(workdir)),
            )),
        },
    }
}

pub mod github;

// ---- the background thread ------------------------------------------------

enum Order {
    Refresh {
        /// Asked for by the user rather than by the timer.
        manual: bool,
    },
}

enum Event {
    Auth(Auth),
    Pulls(Outcome<PullRequest>),
    Issues(Outcome<Issue>),
    Runs(Outcome<Run>),
    /// The round is over; refresh again by itself after `next`.
    Done { next: Duration },
}

fn troubled<T>(outcome: &Outcome<T>) -> bool {
    matches!(outcome, Outcome::Failed(problem) if !matches!(problem, Problem::RateLimited { .. }))
}

/// Serves refresh orders until the tab goes away (`orders` closes) or the
/// integration is switched off (`stop`). `wake` asks the UI for a frame.
fn worker(
    mut backend: Box<dyn Backend>,
    orders: Receiver<Order>,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
    wake: impl Fn(),
) {
    let mut failures = 0u32;
    while let Ok(Order::Refresh { mut manual }) = orders.recv() {
        // One round answers every order that queued up meanwhile.
        while let Ok(Order::Refresh { manual: also }) = orders.try_recv() {
            manual |= also;
        }
        if manual {
            backend.look_again();
        }

        let mut trouble = false;
        // One list at a time, so each section fills in as soon as it can and
        // switching the integration off stops the round between requests.
        for list in 0..3 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let now = epoch_now();
            let event = match list {
                0 => {
                    let outcome = backend.pulls(now);
                    trouble |= troubled(&outcome);
                    Event::Pulls(outcome)
                }
                1 => {
                    let outcome = backend.issues(now);
                    trouble |= troubled(&outcome);
                    Event::Issues(outcome)
                }
                _ => {
                    let outcome = backend.runs(now);
                    trouble |= troubled(&outcome);
                    Event::Runs(outcome)
                }
            };
            if events.send(Event::Auth(backend.auth())).is_err() || events.send(event).is_err() {
                return;
            }
            wake();
        }

        failures = if trouble { failures + 1 } else { 0 };
        let now = epoch_now();
        let signed_in = matches!(backend.auth(), Auth::SignedIn(_));
        let next = next_refresh(signed_in, backend.blocked_until(now), failures, now);
        if events.send(Event::Done { next }).is_err() {
            return;
        }
        wake();
    }
}

// ---- per-tab state --------------------------------------------------------

/// What the sidebar needs for one frame.
pub struct Shown {
    /// Name of the remote the repository was found through.
    pub remote: String,
    pub repo: RepoId,
    pub view: Arc<View>,
}

/// A running integration: one repository, one background thread.
struct Session {
    remote: String,
    repo: RepoId,
    orders: Sender<Order>,
    events: Receiver<Event>,
    stop: Arc<AtomicBool>,
    view: Arc<View>,
    /// When the next automatic refresh is due.
    next_auto: Option<Instant>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing `orders` ends the thread once it is idle; this stops a
        // round that is under way before its next request.
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Session {
    fn start(remote: String, repo: RepoId, backend: Box<dyn Backend>, wake: impl Fn() + Send + 'static) -> Self {
        let (orders, orders_rx) = channel();
        let (events_tx, events) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            // If the thread cannot start, its channel ends close with the
            // closure and `poll` reports the worker as stopped.
            let _ = thread::Builder::new()
                .name("hosting".into())
                .spawn(move || worker(backend, orders_rx, events_tx, stop, wake));
        }
        let mut session = Session {
            remote,
            repo,
            orders,
            events,
            stop,
            view: Arc::default(),
            next_auto: None,
        };
        session.request(false);
        session
    }

    fn request(&mut self, manual: bool) {
        if self.view.busy {
            return;
        }
        self.next_auto = None;
        if self.orders.send(Order::Refresh { manual }).is_ok() {
            Arc::make_mut(&mut self.view).busy = true;
        } else {
            self.stopped();
        }
    }

    /// The worker is gone, which only a panic or a failed thread start can
    /// cause. Say so instead of spinning forever.
    fn stopped(&mut self) {
        let view = Arc::make_mut(&mut self.view);
        let problem = Problem::Failed("The background worker stopped".to_owned());
        view.busy = false;
        view.pulls.problem = Some(problem.clone());
        view.issues.problem = Some(problem.clone());
        view.runs.problem = Some(problem);
    }

    fn poll(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(event) => {
                    let view = Arc::make_mut(&mut self.view);
                    match event {
                        Event::Auth(auth) => view.auth = auth,
                        Event::Pulls(outcome) => view.pulls.apply(outcome),
                        Event::Issues(outcome) => view.issues.apply(outcome),
                        Event::Runs(outcome) => view.runs.apply(outcome),
                        Event::Done { next } => {
                            view.busy = false;
                            view.updated = Some(epoch_now());
                            self.next_auto = Some(Instant::now() + next);
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.view.busy {
                        self.stopped();
                    }
                    break;
                }
            }
        }
    }
}

/// Per-repository hosting state, owned by the tab.
#[derive(Default)]
pub struct HostingState {
    session: Option<Session>,
}

impl HostingState {
    /// Called every frame the sidebar is drawn, which is only for the active
    /// tab. Starts the integration the first time a hosted remote is seen,
    /// takes in whatever the background thread has produced, and asks for a
    /// refresh when one is due.
    ///
    /// Returns `None`, having made no request, when the integration is
    /// switched off or no remote points at a known service.
    pub fn sync(
        &mut self,
        ctx: &egui::Context,
        workdir: &Path,
        remotes: &[RemoteInfo],
        enabled: bool,
    ) -> Option<Shown> {
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        // Only poll while someone is looking at the window.
        let attended = ctx.input(|input| input.focused);
        let shown = self.sync_with(remotes, enabled, attended, |repo| connect(repo, workdir), wake);
        if let Some(due) = self.session.as_ref().and_then(|session| session.next_auto)
            && attended
        {
            ctx.request_repaint_after(due.saturating_duration_since(Instant::now()));
        }
        shown
    }

    fn sync_with(
        &mut self,
        remotes: &[RemoteInfo],
        enabled: bool,
        attended: bool,
        connect: impl FnOnce(&RepoId) -> Box<dyn Backend>,
        wake: impl Fn() + Send + 'static,
    ) -> Option<Shown> {
        let target = if enabled { detect(remotes) } else { None };
        let Some((remote, repo)) = target else {
            // Dropping the session stops its thread.
            self.session = None;
            return None;
        };

        // First sight, or the remote now points somewhere else.
        if self.session.as_ref().is_none_or(|session| session.repo != repo) {
            let backend = connect(&repo);
            self.session = Some(Session::start(remote.clone(), repo, backend, wake));
        }
        let session = self.session.as_mut()?;
        session.remote = remote;
        session.poll();
        if attended && session.next_auto.is_some_and(|due| Instant::now() >= due) {
            session.request(false);
        }
        Some(Shown {
            remote: session.remote.clone(),
            repo: session.repo.clone(),
            view: session.view.clone(),
        })
    }

    /// Refreshes now, at the user's request.
    pub fn refresh(&mut self) {
        if let Some(session) = &mut self.session {
            session.request(true);
        }
    }
}

// ---- checking out a pull request ------------------------------------------

/// The local branch a pull request is checked out as.
pub fn pull_branch(number: u64) -> String {
    format!("pr/{number}")
}

/// Fetches a pull request's head from `remote` into [`pull_branch`] and
/// checks it out. With `on_branch` (that branch is the current one, which
/// git refuses to fetch into) the branch is brought forward instead.
///
/// Neither form forces: if the local branch has commits the pull request
/// does not, git says so and nothing is lost. Only the pull request's
/// commits are fetched, not the remote's tags.
pub fn checkout_pull(remote: &str, service: Service, number: u64, on_branch: bool) -> GitCmd {
    let branch = pull_branch(number);
    let head = service.pull_ref(number);
    if on_branch {
        GitCmd::new(format!("Update pull request #{number}"), &["fetch", "--no-tags", remote, &head])
            .step(&["merge", "--ff-only", "FETCH_HEAD"])
            .undoable(UndoKind::HeadMove)
            .done(format!("Updated {branch} to pull request #{number}"))
    } else {
        GitCmd::new(
            format!("Check out pull request #{number}"),
            &["fetch", "--no-tags", remote, &format!("{head}:{branch}")],
        )
        .step(&["checkout", &branch])
        .undoable(UndoKind::Checkout)
        .done(format!("Checked out pull request #{number} as {branch}"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicU32;

    use super::github::{Client, is_api, parse_issues, parse_pulls, parse_runs, password_from, run_state};
    use super::*;

    const NOW: u64 = 1_800_000_000;
    /// Shaped like a real token; never a real one.
    const SECRET: &str = "ghp_FAKEfakeFAKEfakeFAKEfake0123456789";
    const OTHER: &str = "gho_ANOTHERfakeTOKENforTESTS9876543210";

    // ---- fakes ----

    /// One request as the transport saw it.
    struct Sent {
        url: String,
        etag: Option<String>,
        token: Option<String>,
    }

    type Log = Arc<Mutex<Vec<Sent>>>;

    /// A transport that plays back scripted answers and records requests.
    struct Script {
        replies: VecDeque<Result<HttpResponse, String>>,
        sent: Log,
    }

    impl Transport for Script {
        fn get(&mut self, request: &HttpRequest<'_>) -> Result<HttpResponse, String> {
            self.sent.lock().unwrap().push(Sent {
                url: request.url.to_owned(),
                etag: request.etag.map(str::to_owned),
                token: request.token.map(|token| token.0.clone()),
            });
            self.replies.pop_front().unwrap_or_else(|| Err("script exhausted".to_owned()))
        }
    }

    fn reply(status: u16, headers: &[(&str, &str)], body: &str) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status,
            headers: headers.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            body: body.to_owned(),
        })
    }

    /// Token sources with fixed contents that record the order they are asked in.
    #[derive(Clone, Default)]
    struct Fake {
        env: Vec<(&'static str, &'static str)>,
        cli: Option<&'static str>,
        helper: Option<&'static str>,
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl Fake {
        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Secrets for Fake {
        fn env(&self, name: &str) -> Option<String> {
            self.asked.lock().unwrap().push(name.to_owned());
            self.env.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned())
        }
        fn cli(&self) -> Option<String> {
            self.asked.lock().unwrap().push("gh".to_owned());
            self.cli.map(str::to_owned)
        }
        fn credential_helper(&self) -> Option<String> {
            self.asked.lock().unwrap().push("git".to_owned());
            self.helper.map(str::to_owned)
        }
    }

    fn repo() -> RepoId {
        RepoId { service: Service::GitHub, owner: "octo".to_owned(), name: "app".to_owned() }
    }

    fn scripted(replies: Vec<Result<HttpResponse, String>>, secrets: Fake) -> (Client, Log) {
        let sent = Log::default();
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        (Client::new(repo(), Box::new(transport), Box::new(secrets)), sent)
    }

    fn with_token() -> Fake {
        Fake { env: vec![("GITHUB_TOKEN", SECRET)], ..Default::default() }
    }

    fn remote(name: &str, url: &str) -> RemoteInfo {
        RemoteInfo { name: name.to_owned(), url: url.to_owned() }
    }

    fn failure<T: std::fmt::Debug>(outcome: Outcome<T>) -> Problem {
        match outcome {
            Outcome::Failed(problem) => problem,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    // ---- fixtures: trimmed-down API answers ----

    const PULLS: &str = r#"[
      {"number": 12, "title": "Add dark mode", "draft": false, "state": "open",
       "html_url": "https://github.com/octo/app/pull/12", "user": {"login": "ada", "id": 1},
       "head": {"ref": "feature/dark", "sha": "1111111111111111111111111111111111111111",
                "repo": {"full_name": "Octo/App"}},
       "base": {"ref": "main", "sha": "0000000000000000000000000000000000000000",
                "repo": {"full_name": "octo/app"}},
       "updated_at": "1970-01-02T00:00:00Z"},
      {"number": 9, "title": "WIP: parser", "draft": true, "user": null,
       "head": {"ref": "main", "sha": "2222222222222222222222222222222222222222", "repo": null},
       "base": {"ref": "main", "sha": "0000000000000000000000000000000000000000"}}
    ]"#;

    const ISSUES: &str = r#"[
      {"number": 31, "title": "Crash on empty repo", "user": {"login": "grace"}, "comments": 4,
       "labels": [{"name": "bug"}, {"name": "good first issue"}], "updated_at": "1970-01-01T00:01:00Z"},
      {"number": 12, "title": "Add dark mode", "user": {"login": "ada"}, "comments": 0, "labels": [],
       "pull_request": {"url": "https://api.github.com/repos/octo/app/pulls/12"}},
      {"number": 30, "title": "Docs typo", "user": {"login": "linus"}}
    ]"#;

    const RUNS: &str = r#"{"total_count": 57, "workflow_runs": [
      {"id": 9001, "name": "CI", "display_title": "Add dark mode", "run_number": 140,
       "head_branch": "feature/dark", "head_sha": "1111111111111111111111111111111111111111",
       "event": "pull_request", "status": "completed", "conclusion": "success",
       "created_at": "1970-01-01T01:00:00Z"},
      {"id": 9002, "name": "Release", "display_title": "v1.2", "run_number": 7,
       "head_branch": null, "head_sha": "3333333333333333333333333333333333333333",
       "event": "release", "status": "in_progress", "conclusion": null},
      {"id": 9003, "name": "CI", "run_number": 139, "head_branch": "main",
       "head_sha": "4444444444444444444444444444444444444444", "event": "push",
       "status": "completed", "conclusion": "timed_out"}
    ]}"#;

    // ---- remote URLs ----

    #[test]
    fn remote_urls_are_recognised_in_every_common_form() {
        for url in [
            "https://github.com/octo/app",
            "https://github.com/octo/app.git",
            "https://github.com/octo/app/",
            "https://github.com/octo/app.git/",
            "http://github.com/octo/app",
            "https://www.github.com/octo/app",
            "https://GitHub.com/octo/app.git",
            "https://ada@github.com/octo/app.git",
            "https://ada:hunter2@github.com/octo/app.git",
            "https://github.com:443/octo/app.git",
            "git@github.com:octo/app.git",
            "git@github.com:octo/app",
            "git@github.com:/octo/app.git",
            "org-1234@github.com:octo/app.git",
            "github.com:octo/app.git",
            "ssh://git@github.com/octo/app",
            "ssh://git@github.com/octo/app.git",
            "ssh://git@github.com:22/octo/app.git",
            "ssh://git@ssh.github.com:443/octo/app.git",
            "git://github.com/octo/app.git",
            "git+ssh://git@github.com/octo/app.git",
            "  https://github.com/octo/app.git\n",
        ] {
            assert_eq!(parse_remote(url), Some(repo()), "{url}");
        }

        let dotted = parse_remote("git@github.com:rust-lang/rust.vim.git").unwrap();
        assert_eq!((dotted.owner.as_str(), dotted.name.as_str()), ("rust-lang", "rust.vim"));
        assert_eq!(dotted.slug(), "rust-lang/rust.vim");
    }

    #[test]
    fn other_hosts_and_malformed_urls_are_rejected() {
        for url in [
            "",
            "https://gitlab.com/octo/app.git",
            "git@bitbucket.org:octo/app.git",
            "https://github.com.evil.example/octo/app.git",
            "https://notgithub.com/octo/app.git",
            "https://evil.example/github.com/octo/app",
            "https://github.com@evil.example/octo/app.git",
            "https://gist.github.com/octo/abcdef",
            "https://github.example.com/octo/app.git",
            "git@github.com.evil.example:octo/app.git",
            "https://github.com/octo",
            "https://github.com/octo/",
            "https://github.com/octo/app/pulls",
            "https://github.com//app",
            "https://github.com/octo/..",
            "https://github.com/octo/app?x=1",
            "https://github.com/oc to/app",
            "https://github.com/octo/app&calc.exe",
            "git@github.com:octo",
            "ftp://github.com/octo/app.git",
            "file:///srv/github.com/octo/app.git",
            "C:/Users/ada/github.com/octo/app",
            "C:\\repos\\github.com:octo\\app",
            "/srv/git/github.com:octo/app.git",
            "../github.com/octo/app",
            "E:/demo-origin.git",
        ] {
            assert_eq!(parse_remote(url), None, "{url}");
        }
    }

    #[test]
    fn origin_is_preferred_and_other_remotes_are_a_fallback() {
        let fork = remote("fork", "git@github.com:ada/app.git");
        let origin = remote("origin", "https://github.com/octo/app.git");
        let found = detect(&[fork.clone(), origin]).unwrap();
        assert_eq!((found.0.as_str(), found.1.slug().as_str()), ("origin", "octo/app"));

        // origin elsewhere: the first hosted remote stands in.
        let local = remote("origin", "E:/demo-origin.git");
        let found = detect(&[local.clone(), fork]).unwrap();
        assert_eq!((found.0.as_str(), found.1.slug().as_str()), ("fork", "ada/app"));

        assert_eq!(detect(&[local, remote("lab", "https://gitlab.com/octo/app.git")]), None);
        assert_eq!(detect(&[]), None);
    }

    #[test]
    fn links_are_built_only_from_validated_parts() {
        let repo = parse_remote("git@github.com:rust-lang/rust.vim.git").unwrap();
        let links = [
            repo.pulls_url(),
            repo.pull_url(12),
            repo.issues_url(),
            repo.issue_url(31),
            repo.runs_url(),
            repo.run_url(9001),
        ];
        assert_eq!(links[1], "https://github.com/rust-lang/rust.vim/pull/12");
        assert_eq!(links[3], "https://github.com/rust-lang/rust.vim/issues/31");
        assert_eq!(links[5], "https://github.com/rust-lang/rust.vim/actions/runs/9001");
        // They are handed to the operating system, so nothing a shell could
        // read as syntax may appear in them.
        for link in links {
            assert!(link.starts_with("https://github.com/"), "{link}");
            assert!(link.bytes().all(|b| b.is_ascii_alphanumeric() || b"/:._-".contains(&b)), "{link}");
        }
    }

    // ---- JSON ----

    #[test]
    fn pull_request_json_maps_to_the_model() {
        let pulls = parse_pulls(PULLS).unwrap();
        assert_eq!(pulls.len(), 2);

        let first = &pulls[0];
        assert_eq!(first.number, 12);
        assert_eq!(first.title, "Add dark mode");
        assert_eq!(first.author, "ada");
        assert!(!first.draft);
        assert_eq!(first.head_ref, "feature/dark");
        assert_eq!(first.head_sha, "1111111111111111111111111111111111111111");
        assert_eq!(first.base_ref, "main");
        assert_eq!(first.updated, 86_400);
        // Same repository whatever the capitalisation.
        assert!(first.is_from(&repo()));

        // A draft from a deleted fork by a deleted account.
        let second = &pulls[1];
        assert!(second.draft);
        assert_eq!(second.author, "ghost");
        assert_eq!(second.head_repo, None);
        assert!(!second.is_from(&repo()));
        assert_eq!(second.updated, 0);

        assert_eq!(parse_pulls("[]").unwrap(), Vec::new());
        assert!(parse_pulls(r#"{"message": "Not Found"}"#).is_err());
        assert!(parse_pulls("<html>").is_err());
    }

    #[test]
    fn issues_leave_out_the_pull_requests_the_endpoint_mixes_in() {
        let issues = parse_issues(ISSUES).unwrap();
        assert_eq!(issues.iter().map(|issue| issue.number).collect::<Vec<_>>(), [31, 30]);

        let first = &issues[0];
        assert_eq!(first.title, "Crash on empty repo");
        assert_eq!(first.author, "grace");
        assert_eq!(first.comments, 4);
        assert_eq!(first.labels, ["bug", "good first issue"]);
        assert_eq!(first.updated, 60);
        // Sparse entries still come through.
        assert_eq!((issues[1].comments, issues[1].labels.len()), (0, 0));
    }

    #[test]
    fn workflow_runs_map_status_and_conclusion() {
        let (runs, total) = parse_runs(RUNS).unwrap();
        assert_eq!(total, 57);
        assert_eq!(runs.len(), 3);

        let first = &runs[0];
        assert_eq!((first.id, first.number), (9001, 140));
        assert_eq!((first.workflow.as_str(), first.branch.as_str()), ("CI", "feature/dark"));
        assert_eq!(first.title, "Add dark mode");
        assert_eq!(first.event, "pull_request");
        assert_eq!((first.state, first.outcome.as_str()), (RunState::Success, "success"));
        assert_eq!(first.when, 3600);

        // Still running, no branch (a tag or release), no dates.
        assert_eq!((runs[1].state, runs[1].outcome.as_str()), (RunState::Running, "in progress"));
        assert_eq!((runs[1].branch.as_str(), runs[1].when), ("", 0));
        assert_eq!((runs[2].state, runs[2].outcome.as_str()), (RunState::Failure, "timed out"));

        for (status, conclusion, expected) in [
            ("completed", Some("success"), RunState::Success),
            ("completed", Some("failure"), RunState::Failure),
            ("completed", Some("startup_failure"), RunState::Failure),
            ("completed", Some("cancelled"), RunState::Cancelled),
            ("completed", Some("skipped"), RunState::Skipped),
            ("completed", Some("neutral"), RunState::Skipped),
            ("completed", Some("action_required"), RunState::NeedsAction),
            ("completed", None, RunState::Skipped),
            ("in_progress", None, RunState::Running),
            ("queued", None, RunState::Queued),
            ("waiting", None, RunState::Queued),
            ("requested", None, RunState::Queued),
            ("pending", None, RunState::Queued),
            ("something_new", None, RunState::Queued),
        ] {
            assert_eq!(run_state(status, conclusion), expected, "{status} {conclusion:?}");
        }

        assert!(parse_runs("[]").is_err());
        assert_eq!(parse_runs(r#"{"total_count": 0, "workflow_runs": []}"#).unwrap().0, Vec::new());
    }

    #[test]
    fn a_pull_request_gets_its_checks_from_runs_on_the_same_commit() {
        let run = |sha: &str, state| Run {
            id: 1,
            workflow: "CI".to_owned(),
            title: String::new(),
            number: 1,
            branch: String::new(),
            event: String::new(),
            head_sha: sha.to_owned(),
            state,
            outcome: String::new(),
            when: 0,
        };
        let runs = [
            run("aaa", RunState::Success),
            run("bbb", RunState::Success),
            run("bbb", RunState::Running),
            run("ccc", RunState::Queued),
            run("ccc", RunState::Failure),
            run("ccc", RunState::Success),
            run("ddd", RunState::Cancelled),
            run("eee", RunState::Skipped),
            run("eee", RunState::Success),
        ];
        assert_eq!(checks_for("aaa", &runs), Some(Checks::Passing));
        assert_eq!(checks_for("bbb", &runs), Some(Checks::Pending));
        assert_eq!(checks_for("ccc", &runs), Some(Checks::Failing));
        assert_eq!(checks_for("ddd", &runs), None);
        assert_eq!(checks_for("eee", &runs), Some(Checks::Passing));
        assert_eq!(checks_for("zzz", &runs), None);
        assert_eq!(checks_for("", &[run("", RunState::Success)]), None);
    }

    // ---- rate limits and backoff ----

    #[test]
    fn rate_limit_answers_are_told_apart_from_plain_refusals() {
        let none = Rate::default();
        let spent = Rate { remaining: Some(0), reset: Some(NOW + 1200), retry_after: None };
        let left = Rate { remaining: Some(41), reset: Some(NOW + 1200), retry_after: None };
        let retry = Rate { remaining: None, reset: None, retry_after: Some(90) };

        // The primary limit: wait for the window to reset.
        assert_eq!(limited_until(403, &spent, "API rate limit exceeded", 0, NOW), Some(NOW + 1200));
        assert_eq!(limited_until(429, &spent, "", 0, NOW), Some(NOW + 1200));
        // Retry-After wins over everything else.
        assert_eq!(limited_until(403, &retry, "", 0, NOW), Some(NOW + 90));
        assert_eq!(limited_until(429, &retry, "", 3, NOW), Some(NOW + 90));
        let both = Rate { retry_after: Some(30), ..spent };
        assert_eq!(limited_until(403, &both, "", 0, NOW), Some(NOW + 30));

        // Secondary limits come without numbers: back off, longer each time.
        let secondary = "You have exceeded a secondary rate limit. Please wait.";
        assert_eq!(limited_until(403, &left, secondary, 0, NOW), Some(NOW + 60));
        assert_eq!(limited_until(403, &left, secondary, 2, NOW), Some(NOW + 240));
        assert_eq!(limited_until(429, &none, "", 1, NOW), Some(NOW + 120));
        assert_eq!(limited_until(403, &none, "abuse detection mechanism", 0, NOW), Some(NOW + 60));

        // A reset time already in the past (a skewed clock) is not believed.
        let stale = Rate { remaining: Some(0), reset: Some(NOW - 5), retry_after: None };
        assert_eq!(limited_until(403, &stale, "", 1, NOW), Some(NOW + 120));
        // Nor is an absurdly long one.
        let forever = Rate { remaining: None, reset: None, retry_after: Some(10_000_000) };
        assert_eq!(limited_until(429, &forever, "", 0, NOW), Some(NOW + 86_400));

        // Not rate limits at all.
        assert_eq!(limited_until(403, &left, "Resource not accessible by integration", 0, NOW), None);
        assert_eq!(limited_until(403, &none, "", 0, NOW), None);
        assert_eq!(limited_until(404, &spent, "Not Found", 0, NOW), None);
        assert_eq!(limited_until(200, &spent, "", 0, NOW), None);
        assert_eq!(limited_until(500, &none, "rate limit", 0, NOW), None);
    }

    #[test]
    fn backoff_doubles_from_a_minute_and_stops_at_an_hour() {
        let waits: Vec<u64> = (0..9).map(backoff).collect();
        assert_eq!(waits, [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600]);
        assert_eq!(backoff(u32::MAX), 3600);
    }

    #[test]
    fn the_limiter_holds_requests_back_until_the_time_given() {
        let mut limiter = Limiter::default();
        assert_eq!(limiter.blocked(NOW), None);

        let spent = Rate { remaining: Some(0), reset: Some(NOW + 600), retry_after: None };
        assert_eq!(limiter.record(403, &spent, "API rate limit exceeded", NOW), Some(NOW + 600));
        assert_eq!(limiter.blocked(NOW + 1), Some(NOW + 600));
        assert_eq!(limiter.blocked(NOW + 599), Some(NOW + 600));
        assert_eq!(limiter.blocked(NOW + 600), None);

        // Refused again without a hint: the wait grows with each strike.
        let none = Rate::default();
        assert_eq!(limiter.record(429, &none, "", NOW + 600), Some(NOW + 600 + 120));
        assert_eq!(limiter.record(429, &none, "", NOW + 720), Some(NOW + 720 + 240));

        // A good answer clears the strikes.
        let plenty = Rate { remaining: Some(4999), reset: Some(NOW + 4000), retry_after: None };
        assert_eq!(limiter.record(200, &plenty, "", NOW + 1000), None);
        assert_eq!(limiter.blocked(NOW + 1000), None);
        assert_eq!(limiter.record(429, &none, "", NOW + 1001), Some(NOW + 1001 + 60));
    }

    #[test]
    fn the_last_request_of_a_window_blocks_the_next_one_in_advance() {
        let mut limiter = Limiter::default();
        let last = Rate { remaining: Some(0), reset: Some(NOW + 900), retry_after: None };
        // The answer itself is fine...
        assert_eq!(limiter.record(200, &last, "", NOW), None);
        // ...but asking again before the reset would only be refused.
        assert_eq!(limiter.blocked(NOW + 10), Some(NOW + 900));
        assert_eq!(limiter.blocked(NOW + 900), None);

        // An ordinary failure neither blocks nor unblocks.
        let mut limiter = Limiter::default();
        assert_eq!(limiter.record(404, &Rate::default(), "Not Found", NOW), None);
        assert_eq!(limiter.blocked(NOW), None);
    }

    #[test]
    fn automatic_refreshes_are_paced_by_sign_in_failures_and_limits() {
        let secs = |signed_in, blocked, failures| next_refresh(signed_in, blocked, failures, NOW).as_secs();
        assert_eq!(secs(true, None, 0), 180);
        assert_eq!(secs(false, None, 0), 600);
        // Failing rounds stretch the wait, up to half an hour.
        assert_eq!(secs(true, None, 1), 360);
        assert_eq!(secs(true, None, 2), 720);
        assert_eq!(secs(true, None, 9), 1800);
        assert_eq!(secs(false, None, 1), 1200);
        assert_eq!(secs(false, None, 5), 1800);
        // A rate limit is waited out in full, plus a little.
        assert_eq!(secs(true, Some(NOW + 3000), 0), 3005);
        assert_eq!(secs(false, Some(NOW + 3000), 0), 3005);
        // One that ends sooner than the normal wait changes nothing.
        assert_eq!(secs(true, Some(NOW + 20), 0), 180);
        assert_eq!(secs(true, Some(NOW - 20), 0), 180);
    }

    // ---- sign-in ----

    #[test]
    fn tokens_are_looked_for_in_order_and_no_further_than_needed() {
        // GITHUB_TOKEN wins and nothing else is consulted.
        let all = Fake {
            env: vec![("GITHUB_TOKEN", SECRET), ("GH_TOKEN", OTHER)],
            cli: Some("gh-cli-token-123456"),
            helper: Some("helper-token-123456"),
            ..Default::default()
        };
        let (token, source) = find_token(&all, &[]).unwrap();
        assert_eq!((token.0.as_str(), source), (SECRET, TokenSource::Env("GITHUB_TOKEN")));
        assert_eq!(all.asked(), ["GITHUB_TOKEN"]);

        // GH_TOKEN next.
        let fake = Fake { env: vec![("GH_TOKEN", OTHER)], cli: Some("gh-cli-token-123456"), ..Default::default() };
        let (token, source) = find_token(&fake, &[]).unwrap();
        assert_eq!((token.0.as_str(), source), (OTHER, TokenSource::Env("GH_TOKEN")));
        assert_eq!(fake.asked(), ["GITHUB_TOKEN", "GH_TOKEN"]);

        // Then the GitHub CLI, before git is ever started.
        let fake = Fake { cli: Some("gh-cli-token-123456\n"), helper: Some("helper-token-123456"), ..Default::default() };
        let (token, source) = find_token(&fake, &[]).unwrap();
        assert_eq!((token.0.as_str(), source), ("gh-cli-token-123456", TokenSource::Cli));
        assert_eq!(fake.asked(), ["GITHUB_TOKEN", "GH_TOKEN", "gh"]);

        // Then git's credential helper.
        let fake = Fake { helper: Some("helper-token-123456"), ..Default::default() };
        let (token, source) = find_token(&fake, &[]).unwrap();
        assert_eq!((token.0.as_str(), source), ("helper-token-123456", TokenSource::CredentialHelper));
        assert_eq!(fake.asked(), ["GITHUB_TOKEN", "GH_TOKEN", "gh", "git"]);

        // Nothing anywhere: anonymous.
        let fake = Fake::default();
        assert!(find_token(&fake, &[]).is_none());
        assert_eq!(fake.asked(), ["GITHUB_TOKEN", "GH_TOKEN", "gh", "git"]);
    }

    #[test]
    fn unusable_and_rejected_tokens_are_passed_over() {
        // Empty, blank or mangled values are not tokens.
        let fake = Fake {
            env: vec![("GITHUB_TOKEN", "   "), ("GH_TOKEN", "two words here")],
            cli: Some("line one\nline two"),
            helper: Some("helper-token-123456"),
            ..Default::default()
        };
        let (token, source) = find_token(&fake, &[]).unwrap();
        assert_eq!((token.0.as_str(), source), ("helper-token-123456", TokenSource::CredentialHelper));

        // A token the service already refused does not hide the next source.
        let stale = Token::new(SECRET).unwrap();
        let fake = Fake { env: vec![("GITHUB_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() };
        let (token, source) = find_token(&fake, std::slice::from_ref(&stale)).unwrap();
        assert_eq!((token.0.as_str(), source), (OTHER, TokenSource::Cli));
        assert!(find_token(&fake, &[stale, Token::new(OTHER).unwrap()]).is_none());

        assert!(Token::new("").is_none());
        assert!(Token::new("short").is_none());
        assert!(Token::new("has\ttab-inside-it").is_none());
        assert!(Token::new("caf\u{e9}-caf\u{e9}-caf\u{e9}").is_none());
        assert!(Token::new(&"x".repeat(5000)).is_none());
    }

    #[test]
    fn the_password_line_of_git_credential_output_is_the_token() {
        let answer = "protocol=https\nhost=github.com\nusername=ada\npassword=helper-token-123456\n";
        assert_eq!(password_from(answer).as_deref(), Some("helper-token-123456"));
        assert_eq!(password_from("protocol=https\nhost=github.com\nusername=ada\n"), None);
        assert_eq!(password_from(""), None);
    }

    #[test]
    fn helper_programs_are_captured_quietly_and_never_waited_on_forever() {
        // Standard input reaches the program and its output comes back.
        let mut cmd = Command::new("git");
        cmd.args(["hash-object", "--stdin"]);
        let hash = capture(cmd, b"hello
", Duration::from_secs(30));
        assert_eq!(hash.as_deref().map(str::trim), Some("ce013625030ba8dba906f756967f9e9ca394464a"));

        // A failing program, or one that is not installed, is simply "nothing".
        let mut cmd = Command::new("git");
        cmd.args(["rev-parse", "--verify", "--quiet", "refs/heads/no-such-branch-anywhere"]);
        cmd.current_dir(std::env::temp_dir());
        assert_eq!(capture(cmd, b"", Duration::from_secs(30)), None);
        let missing = Command::new("gitkrill-no-such-helper-program");
        assert_eq!(capture(missing, b"", Duration::from_secs(30)), None);

        // One that hangs is killed at the deadline.
        #[cfg(windows)]
        let slow = {
            let mut cmd = Command::new("ping");
            cmd.args(["-n", "30", "127.0.0.1"]);
            cmd
        };
        #[cfg(not(windows))]
        let slow = {
            let mut cmd = Command::new("sleep");
            cmd.arg("30");
            cmd
        };
        let started = Instant::now();
        assert_eq!(capture(slow, b"", Duration::from_millis(300)), None);
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    }

    #[test]
    fn a_token_is_opaque_when_printed() {
        let token = Token::new(SECRET).unwrap();
        assert_eq!(format!("{token:?}"), "Token(***)");
        assert_eq!(format!("{:?}", Some((token.clone(), TokenSource::Cli))), "Some((Token(***), Cli))");
        assert_eq!(token.scrub(&format!("Bearer {SECRET} was refused")), "Bearer *** was refused");
        for auth in [
            Auth::Unknown,
            Auth::Anonymous,
            Auth::Rejected,
            Auth::SignedIn(TokenSource::Env("GITHUB_TOKEN")),
            Auth::SignedIn(TokenSource::Cli),
            Auth::SignedIn(TokenSource::CredentialHelper),
        ] {
            assert!(!auth.describe().contains("ghp_"));
        }
    }

    // ---- the client ----

    #[test]
    fn requests_go_to_the_api_with_the_token_and_only_there() {
        assert!(is_api("https://api.github.com/repos/octo/app/pulls"));
        for url in [
            "http://api.github.com/repos/octo/app/pulls",
            "https://api.github.com.evil.example/repos/octo/app/pulls",
            "https://api.github.com@evil.example/x",
            "https://api.github.com:8443/x",
            "https://api.github.com",
            "https://github.com/octo/app",
            "https://evil.example/https://api.github.com/",
        ] {
            assert!(!is_api(url), "{url}");
        }

        let (mut client, sent) = scripted(
            vec![reply(200, &[], PULLS), reply(200, &[], ISSUES), reply(200, &[], RUNS)],
            with_token(),
        );
        assert_eq!(client.auth(), Auth::Unknown);
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, more: false } if items.len() == 2));
        assert!(matches!(client.issues(NOW), Outcome::Fresh { ref items, more: false } if items.len() == 2));
        // 57 runs exist, three came back.
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, more: true } if items.len() == 3));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("GITHUB_TOKEN")));

        let sent = sent.lock().unwrap();
        let urls: Vec<&str> = sent.iter().map(|request| request.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://api.github.com/repos/octo/app/pulls?state=open&per_page=30",
                "https://api.github.com/repos/octo/app/issues?state=open&per_page=50",
                "https://api.github.com/repos/octo/app/actions/runs?per_page=20&exclude_pull_requests=true",
            ]
        );
        assert!(sent.iter().all(|request| is_api(&request.url)));
        assert!(sent.iter().all(|request| request.token.as_deref() == Some(SECRET)));
    }

    #[test]
    fn redirects_are_followed_inside_the_api_and_nowhere_else() {
        // A renamed repository: the API points at its permanent address.
        let moved = "https://api.github.com/repositories/42/pulls?state=open&per_page=30";
        let (mut client, sent) =
            scripted(vec![reply(301, &[("location", moved)], ""), reply(200, &[], PULLS)], with_token());
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(sent.lock().unwrap()[1].url, moved);

        // A redirect off the API host is not followed: no request, no token.
        for target in [
            "https://evil.example/collect",
            "http://api.github.com/repositories/42/pulls",
            "https://api.github.com.evil.example/x",
        ] {
            let (mut client, sent) = scripted(
                vec![reply(302, &[("location", target)], ""), reply(200, &[], PULLS)],
                with_token(),
            );
            let problem = failure(client.pulls(NOW));
            assert_eq!(problem, Problem::Failed("GitHub sent an unexpected redirect".to_owned()));
            let sent = sent.lock().unwrap();
            assert_eq!(sent.len(), 1, "{target}");
            assert!(is_api(&sent[0].url));
        }

        // Nor is an endless chain.
        let replies = (0..10).map(|_| reply(301, &[("location", moved)], "")).collect();
        let (mut client, sent) = scripted(replies, with_token());
        assert!(matches!(client.pulls(NOW), Outcome::Failed(Problem::Failed(_))));
        assert_eq!(sent.lock().unwrap().len(), 4);
    }

    #[test]
    fn unchanged_lists_are_asked_for_conditionally() {
        let (mut client, sent) = scripted(
            vec![
                reply(200, &[("etag", "W/\"v1\""), ("link", "<https://api.github.com/x?page=2>; rel=\"next\"")], PULLS),
                reply(304, &[], ""),
                reply(200, &[("etag", "W/\"v2\"")], "[]"),
                reply(304, &[], ""),
            ],
            Fake::default(),
        );
        let mut feed = Feed::default();

        // First answer: no validator to send yet; a next page exists.
        feed.apply(client.pulls(NOW));
        assert!(feed.loaded && feed.more && feed.items.len() == 2);

        // Second: validated, nothing transferred, the list stays.
        let outcome = client.pulls(NOW + 200);
        assert_eq!(outcome, Outcome::Unchanged);
        feed.apply(outcome);
        assert_eq!(feed.items.len(), 2);

        // Third: changed. Fourth: validated against the new answer.
        feed.apply(client.pulls(NOW + 400));
        assert!(feed.items.is_empty() && !feed.more && feed.loaded);
        assert_eq!(client.pulls(NOW + 600), Outcome::Unchanged);

        let sent = sent.lock().unwrap();
        let etags: Vec<Option<&str>> = sent.iter().map(|request| request.etag.as_deref()).collect();
        assert_eq!(etags, [None, Some("W/\"v1\""), Some("W/\"v1\""), Some("W/\"v2\"")]);
        // Anonymous throughout.
        assert!(sent.iter().all(|request| request.token.is_none()));
        assert_eq!(client.auth(), Auth::Anonymous);
    }

    #[test]
    fn each_list_keeps_its_own_validator() {
        let (mut client, sent) = scripted(
            vec![
                reply(200, &[("etag", "\"pulls\"")], PULLS),
                reply(200, &[("etag", "\"issues\"")], ISSUES),
                reply(200, &[], RUNS),
                reply(304, &[], ""),
                reply(304, &[], ""),
                reply(200, &[], RUNS),
            ],
            Fake::default(),
        );
        for _ in 0..2 {
            client.pulls(NOW);
            client.issues(NOW);
            client.runs(NOW);
        }
        let sent = sent.lock().unwrap();
        let etags: Vec<Option<&str>> = sent.iter().map(|request| request.etag.as_deref()).collect();
        assert_eq!(etags, [None, None, None, Some("\"pulls\""), Some("\"issues\""), None]);
    }

    #[test]
    fn a_rate_limit_stops_requests_until_it_lifts() {
        let reset = (NOW + 1800).to_string();
        let limited = [("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", reset.as_str())];
        let body = r#"{"message": "API rate limit exceeded for 203.0.113.7."}"#;
        let (mut client, sent) = scripted(
            vec![reply(200, &[], PULLS), reply(403, &limited, body), reply(200, &[], ISSUES)],
            Fake::default(),
        );
        let mut pulls = Feed::default();
        let mut issues: Feed<Issue> = Feed::default();
        pulls.apply(client.pulls(NOW));

        // The refusal names the time...
        issues.apply(client.issues(NOW));
        assert_eq!(issues.problem, Some(Problem::RateLimited { until: NOW + 1800 }));
        assert_eq!(client.blocked_until(NOW), Some(NOW + 1800));
        // ...and until then nothing more is sent, for any list.
        pulls.apply(client.pulls(NOW + 60));
        assert_eq!(failure(client.runs(NOW + 1799)), Problem::RateLimited { until: NOW + 1800 });
        assert_eq!(sent.lock().unwrap().len(), 2);

        // What was loaded before stays on screen, with the reason beside it.
        assert_eq!(pulls.items.len(), 2);
        assert!(pulls.loaded);
        let line = pulls.problem.as_ref().unwrap().line(Service::GitHub);
        assert_eq!(line, format!("Rate limited until {}", clock(NOW + 1800)));
        let brief = pulls.problem.as_ref().unwrap().brief(Service::GitHub);
        assert_eq!(brief, format!("Limited until {}", clock(NOW + 1800)));

        // Once the window has reset, requests resume and the line goes away.
        issues.apply(client.issues(NOW + 1800));
        assert_eq!(sent.lock().unwrap().len(), 3);
        assert!(issues.problem.is_none() && issues.items.len() == 2);
    }

    #[test]
    fn spending_the_last_request_blocks_the_next_without_asking() {
        let reset = (NOW + 900).to_string();
        let last = [("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", reset.as_str())];
        let (mut client, sent) = scripted(vec![reply(200, &last, PULLS), reply(200, &[], ISSUES)], Fake::default());
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(failure(client.issues(NOW + 1)), Problem::RateLimited { until: NOW + 900 });
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_good_data() {
        let (mut client, _) = scripted(
            vec![
                reply(200, &[], PULLS),
                Err("no connection".to_owned()),
                reply(502, &[], "<html>Bad Gateway</html>"),
                reply(200, &[], "{ not json"),
                reply(200, &[], "[]"),
            ],
            Fake::default(),
        );
        let mut feed = Feed::default();
        feed.apply(client.pulls(NOW));
        assert_eq!(feed.items.len(), 2);

        feed.apply(client.pulls(NOW));
        assert_eq!(feed.problem, Some(Problem::Unreachable("no connection".to_owned())));
        assert_eq!(feed.problem.as_ref().unwrap().line(Service::GitHub), "GitHub unreachable: no connection");
        assert_eq!(feed.items.len(), 2);

        feed.apply(client.pulls(NOW));
        assert_eq!(feed.problem, Some(Problem::Failed("GitHub answered 502".to_owned())));
        assert_eq!(feed.items.len(), 2);

        feed.apply(client.pulls(NOW));
        assert!(matches!(&feed.problem, Some(Problem::Failed(text)) if text.starts_with("Unreadable answer")));
        assert_eq!(feed.items.len(), 2);

        // Recovery replaces both the data and the complaint.
        feed.apply(client.pulls(NOW));
        assert!(feed.problem.is_none() && feed.items.is_empty());
    }

    #[test]
    fn an_unreadable_answer_is_not_vouched_for_by_its_validator() {
        let (mut client, sent) = scripted(
            vec![reply(200, &[("etag", "\"bad\"")], "{ not json"), reply(200, &[], PULLS)],
            Fake::default(),
        );
        assert!(matches!(client.pulls(NOW), Outcome::Failed(_)));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(sent.lock().unwrap()[1].etag, None);
    }

    #[test]
    fn without_a_token_a_hidden_repository_reads_as_not_signed_in() {
        let not_found = r#"{"message": "Not Found", "documentation_url": "https://docs.github.com/rest"}"#;
        let (mut client, _) = scripted(vec![reply(404, &[], not_found)], Fake::default());
        let problem = failure(client.pulls(NOW));
        assert_eq!(problem, Problem::NotSignedIn);
        assert_eq!(problem.line(Service::GitHub), "Not signed in: private repository");
        assert!(problem.detail(Service::GitHub).contains("GITHUB_TOKEN"));

        // Signed in and still not visible: a different story.
        let (mut client, _) = scripted(vec![reply(404, &[], not_found)], with_token());
        assert_eq!(failure(client.pulls(NOW)), Problem::NoAccess);
    }

    #[test]
    fn a_refused_token_is_dropped_and_the_next_source_tried() {
        let bad = r#"{"message": "Bad credentials"}"#;
        // GITHUB_TOKEN is stale, the CLI has a good one.
        let secrets = Fake { env: vec![("GITHUB_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() };
        let (mut client, sent) = scripted(
            vec![reply(401, &[], bad), reply(200, &[("etag", "\"a\"")], PULLS), reply(304, &[], "")],
            secrets,
        );
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Cli));
        assert_eq!(client.pulls(NOW), Outcome::Unchanged);
        {
            let sent = sent.lock().unwrap();
            let tokens: Vec<Option<&str>> = sent.iter().map(|request| request.token.as_deref()).collect();
            assert_eq!(tokens, [Some(SECRET), Some(OTHER), Some(OTHER)]);
        }

        // Every token refused: carry on anonymously, which is enough for a
        // public repository...
        let (mut client, sent) =
            scripted(vec![reply(401, &[], bad), reply(200, &[], PULLS), reply(404, &[], "")], with_token());
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(client.auth(), Auth::Rejected);
        assert_eq!(sent.lock().unwrap()[1].token, None);
        // ...and says why when it is not.
        let problem = failure(client.issues(NOW));
        assert_eq!(problem, Problem::SignInRejected);
        assert_eq!(problem.line(Service::GitHub), "Sign-in refused: private repository");
        // The refused token is not tried again, even on a manual refresh.
        client.look_again();
        client.issues(NOW);
        assert!(sent.lock().unwrap().iter().skip(1).all(|request| request.token.is_none()));
    }

    #[test]
    fn validators_and_limits_do_not_carry_over_to_another_identity() {
        let reset = (NOW + 1800).to_string();
        let limited = [("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", reset.as_str())];
        // Anonymous at first and rate limited; then the user signs in.
        let secrets = Fake::default();
        let sent = Log::default();
        let transport = Script {
            replies: vec![
                reply(200, &[("etag", "\"anon\"")], PULLS),
                reply(403, &limited, r#"{"message": "API rate limit exceeded"}"#),
                reply(200, &[], PULLS),
            ]
            .into(),
            sent: sent.clone(),
        };
        // Shares `asked` with the client's copy; the token appears later.
        struct Late(Fake, Arc<AtomicBool>);
        impl Secrets for Late {
            fn env(&self, name: &str) -> Option<String> {
                (self.1.load(Ordering::Relaxed) && name == "GITHUB_TOKEN").then(|| SECRET.to_owned())
            }
            fn cli(&self) -> Option<String> {
                self.0.cli()
            }
            fn credential_helper(&self) -> Option<String> {
                self.0.credential_helper()
            }
        }
        let signed_in = Arc::new(AtomicBool::new(false));
        let mut client = Client::new(repo(), Box::new(transport), Box::new(Late(secrets, signed_in.clone())));

        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert!(matches!(client.pulls(NOW), Outcome::Failed(Problem::RateLimited { .. })));

        // Without a manual refresh nothing changes: still blocked.
        signed_in.store(true, Ordering::Relaxed);
        assert!(matches!(client.pulls(NOW + 5), Outcome::Failed(Problem::RateLimited { .. })));
        assert_eq!(sent.lock().unwrap().len(), 2);

        // A manual refresh finds the sign-in; the anonymous limit and the
        // anonymous validator no longer apply.
        client.look_again();
        assert!(matches!(client.pulls(NOW + 10), Outcome::Fresh { .. }));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("GITHUB_TOKEN")));
        let sent = sent.lock().unwrap();
        assert_eq!((sent[2].etag.as_deref(), sent[2].token.as_deref()), (None, Some(SECRET)));
    }

    #[test]
    fn a_plain_refusal_is_shown_in_the_services_words() {
        let sso = r#"{"message": "Resource protected by organization SAML enforcement."}"#;
        let (mut client, _) = scripted(vec![reply(403, &[("x-ratelimit-remaining", "4990")], sso)], with_token());
        let problem = failure(client.pulls(NOW));
        assert_eq!(problem, Problem::Refused("Resource protected by organization SAML enforcement.".to_owned()));
        assert_eq!(
            problem.line(Service::GitHub),
            "GitHub refused: Resource protected by organization SAML enforcement."
        );
        // Not a rate limit: nothing is held back.
        assert_eq!(client.blocked_until(NOW), None);
    }

    #[test]
    fn issues_switched_off_is_an_empty_list_not_an_error() {
        let gone = r#"{"message": "Issues are disabled for this repo"}"#;
        let (mut client, _) = scripted(vec![reply(410, &[], gone)], Fake::default());
        assert_eq!(client.issues(NOW), Outcome::Fresh { items: Vec::new(), more: false });
    }

    #[test]
    fn a_token_never_reaches_a_displayed_error() {
        let echo = format!(r#"{{"message": "Bad token {SECRET} for Bearer {SECRET}"}}"#);
        let both = format!(r#"{{"message": "saw {SECRET} and {OTHER}"}}"#);
        let cases: Vec<(Fake, Vec<Result<HttpResponse, String>>)> = vec![
            // The network stack quotes the request.
            (with_token(), vec![Err(format!("tls: failed sending Authorization: Bearer {SECRET}"))]),
            // The server echoes the header back in an error message.
            (with_token(), vec![reply(403, &[], &echo)]),
            (with_token(), vec![reply(500, &[], &echo)]),
            (with_token(), vec![reply(418, &[], &echo)]),
            // ...or in a body that is not what we expect.
            (with_token(), vec![reply(200, &[], &echo)]),
            (with_token(), vec![reply(200, &[], &format!("[{{\"number\": \"{SECRET}\"}}]"))]),
            (with_token(), vec![reply(200, &[], SECRET)]),
            (with_token(), vec![reply(301, &[("location", format!("https://evil.example/{SECRET}").as_str())], "")]),
            // A token that was refused earlier is scrubbed as well.
            (with_token(), vec![reply(401, &[], &echo), reply(403, &[], &echo)]),
            (with_token(), vec![reply(401, &[], &echo), Err(format!("proxy said: {SECRET}"))]),
            (
                Fake { env: vec![("GITHUB_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() },
                vec![reply(401, &[], &both), reply(403, &[], &both)],
            ),
            // Rate-limit wording passes through no free text at all.
            (with_token(), vec![reply(429, &[("retry-after", "60")], &echo)]),
            (with_token(), vec![reply(404, &[], &echo)]),
        ];

        for (index, (secrets, replies)) in cases.into_iter().enumerate() {
            let (mut client, _) = scripted(replies, secrets);
            for outcome in [
                client.pulls(NOW).map_failure(),
                client.issues(NOW + 100_000).map_failure(),
                client.runs(NOW + 200_000).map_failure(),
            ] {
                let Some(problem) = outcome else { continue };
                let shown = [
                    problem.line(Service::GitHub),
                    problem.brief(Service::GitHub),
                    problem.detail(Service::GitHub),
                    format!("{problem:?}"),
                    client.auth().describe(),
                    format!("{:?}", client.auth()),
                ];
                for text in shown {
                    assert!(!text.contains(SECRET), "case {index}: {text}");
                    assert!(!text.contains(OTHER), "case {index}: {text}");
                    assert!(!text.contains("FAKEfake"), "case {index}: {text}");
                }
            }
        }

        // The scrubbing is what did it, not luck: the message survives.
        let (mut client, _) = scripted(vec![reply(403, &[], &echo)], with_token());
        assert_eq!(
            failure(client.pulls(NOW)),
            Problem::Refused("Bad token *** for Bearer ***".to_owned())
        );
    }

    impl<T> Outcome<T> {
        fn map_failure(self) -> Option<Problem> {
            match self {
                Outcome::Failed(problem) => Some(problem),
                _ => None,
            }
        }
    }

    #[test]
    fn error_lines_are_flattened_and_bounded() {
        let long = format!("first line\r\n\tsecond\u{7} line {}", "x".repeat(500));
        let tidy = tidy(&long);
        assert!(tidy.starts_with("first line second line xxx"));
        assert_eq!(tidy.chars().count(), 161);
        assert!(tidy.ends_with('…'));
        assert_eq!(super::tidy("  already   fine "), "already fine");
    }

    // ---- small things ----

    #[test]
    fn ages_are_compact() {
        let now = 2_000_000_000;
        for (secs, expected) in [
            (0, "now"),
            (59, "now"),
            (60, "1m"),
            (3599, "59m"),
            (3600, "1h"),
            (86_399, "23h"),
            (86_400, "1d"),
            (86_400 * 29, "29d"),
            (86_400 * 30, "1mo"),
            (86_400 * 364, "12mo"),
            (86_400 * 365, "1y"),
            (86_400 * 800, "2y"),
        ] {
            assert_eq!(ago(now - secs, now), expected, "{secs}");
        }
        // Unknown or future times say nothing rather than something wrong.
        assert_eq!(ago(0, now), "");
        assert_eq!(ago(now + 30, now), "");
        assert_eq!(timestamp(Some("2026-11-10T12:00:00+02:00")), timestamp(Some("2026-11-10T10:00:00Z")));
        assert_eq!(timestamp(Some("yesterday")), 0);
        assert_eq!(timestamp(None), 0);
        assert_eq!(clock(NOW).len(), 5);
    }

    #[test]
    fn the_offline_fixture_serves_files_and_staged_failures() {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-canned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pulls.json"), PULLS).unwrap();
        std::fs::write(dir.join("issues.head"), "403\nRetry-After: 120\n").unwrap();
        std::fs::write(dir.join("issues.json"), r#"{"message": "slow down"}"#).unwrap();

        let mut client = Client::new(repo(), Box::new(Canned { dir: dir.clone() }), Box::new(NoSecrets));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
        assert_eq!(failure(client.issues(NOW)), Problem::RateLimited { until: NOW + 120 });
        // No runs.json: the list does not exist.
        assert_eq!(failure(client.runs(NOW + 120)), Problem::NotSignedIn);
        assert_eq!(client.auth(), Auth::Anonymous);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the background thread and the per-tab state ----

    /// A backend with fixed answers that counts what it is asked.
    #[derive(Clone, Default)]
    struct Stub {
        rounds: Arc<AtomicU32>,
        looked_again: Arc<AtomicU32>,
        fail: Arc<AtomicBool>,
    }

    impl Backend for Stub {
        fn pulls(&mut self, _now: u64) -> Outcome<PullRequest> {
            self.rounds.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Outcome::Failed(Problem::Unreachable("no connection".to_owned()));
            }
            Outcome::Fresh { items: parse_pulls(PULLS).unwrap(), more: false }
        }
        fn issues(&mut self, _now: u64) -> Outcome<Issue> {
            if self.fail.load(Ordering::SeqCst) {
                return Outcome::Failed(Problem::Unreachable("no connection".to_owned()));
            }
            Outcome::Fresh { items: parse_issues(ISSUES).unwrap(), more: true }
        }
        fn runs(&mut self, _now: u64) -> Outcome<Run> {
            Outcome::Unchanged
        }
        fn auth(&self) -> Auth {
            Auth::SignedIn(TokenSource::Cli)
        }
        fn look_again(&mut self) {
            self.looked_again.fetch_add(1, Ordering::SeqCst);
        }
        fn blocked_until(&self, _now: u64) -> Option<u64> {
            None
        }
    }

    fn hosted() -> Vec<RemoteInfo> {
        vec![remote("origin", "git@github.com:octo/app.git")]
    }

    /// Syncs like the UI does each frame until the round in flight is over.
    fn settle(state: &mut HostingState, remotes: &[RemoteInfo]) -> Shown {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let connect = |_: &RepoId| -> Box<dyn Backend> { panic!("already connected") };
            let shown = state.sync_with(remotes, true, false, connect, || {}).expect("still hosted");
            if !shown.view.busy {
                return shown;
            }
            assert!(Instant::now() < deadline, "the worker never finished");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn nothing_is_started_when_switched_off_or_not_hosted() {
        let mut state = HostingState::default();
        let never = |_: &RepoId| -> Box<dyn Backend> { panic!("must not connect") };
        // The setting is off: a GitHub remote changes nothing.
        assert!(state.sync_with(&hosted(), false, true, never, || {}).is_none());
        assert!(state.session.is_none());
        // On, but the remotes are elsewhere.
        let elsewhere = [remote("origin", "E:/demo-origin.git"), remote("lab", "https://gitlab.com/octo/app.git")];
        assert!(state.sync_with(&elsewhere, true, true, never, || {}).is_none());
        assert!(state.sync_with(&[], true, true, never, || {}).is_none());
        assert!(state.session.is_none());
        // A manual refresh with nothing running is a no-op too.
        state.refresh();
    }

    #[test]
    fn the_first_sync_loads_everything_in_the_background() {
        let stub = Stub::default();
        let woken = Arc::new(AtomicU32::new(0));
        let mut state = HostingState::default();
        let shown = {
            let (stub, woken) = (stub.clone(), woken.clone());
            let wake = move || {
                woken.fetch_add(1, Ordering::SeqCst);
            };
            state.sync_with(&hosted(), true, false, move |_| Box::new(stub), wake).unwrap()
        };
        // Returned at once, before any answer: nothing blocks the frame.
        assert_eq!((shown.remote.as_str(), shown.repo.slug().as_str()), ("origin", "octo/app"));
        assert!(shown.view.busy);

        let shown = settle(&mut state, &hosted());
        let view = &shown.view;
        assert_eq!((view.pulls.items.len(), view.issues.items.len()), (2, 2));
        assert!(view.pulls.loaded && view.issues.loaded && view.issues.more);
        // "Unchanged" for a list never loaded leaves it empty and quiet.
        assert!(view.runs.items.is_empty() && view.runs.problem.is_none());
        assert_eq!(view.auth, Auth::SignedIn(TokenSource::Cli));
        assert!(view.updated.is_some());
        assert_eq!(stub.rounds.load(Ordering::SeqCst), 1);
        // The UI was asked to repaint for each list and at the end (the last
        // call may still be on its way when the data is already in).
        let deadline = Instant::now() + Duration::from_secs(20);
        while woken.load(Ordering::SeqCst) < 4 {
            assert!(Instant::now() < deadline, "woken {} times", woken.load(Ordering::SeqCst));
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(woken.load(Ordering::SeqCst), 4);
        // The next refresh is scheduled, a few minutes out.
        let due = state.session.as_ref().unwrap().next_auto.expect("scheduled");
        let wait = due.saturating_duration_since(Instant::now());
        assert!(wait > Duration::from_secs(170) && wait <= Duration::from_secs(180), "{wait:?}");
    }

    #[test]
    fn refreshes_happen_on_demand_and_when_due_while_attended() {
        let stub = Stub::default();
        let mut state = HostingState::default();
        {
            let stub = stub.clone();
            state.sync_with(&hosted(), true, true, move |_| Box::new(stub), || {});
        }
        settle(&mut state, &hosted());
        let rounds = || stub.rounds.load(Ordering::SeqCst);
        assert_eq!(rounds(), 1);

        // Not due yet: syncing every frame asks for nothing.
        let connect = |_: &RepoId| -> Box<dyn Backend> { panic!("already connected") };
        for _ in 0..50 {
            assert!(!state.sync_with(&hosted(), true, true, connect, || {}).unwrap().view.busy);
        }
        assert_eq!(rounds(), 1);

        // Due, but nobody is looking at the window: still nothing.
        state.session.as_mut().unwrap().next_auto = Some(Instant::now());
        assert!(!state.sync_with(&hosted(), true, false, connect, || {}).unwrap().view.busy);
        // Due and attended: one automatic round, without a new sign-in search.
        assert!(state.sync_with(&hosted(), true, true, connect, || {}).unwrap().view.busy);
        settle(&mut state, &hosted());
        assert_eq!((rounds(), stub.looked_again.load(Ordering::SeqCst)), (2, 0));

        // On demand: at once, and worth looking for a sign-in again. Asking
        // twice while it runs does not queue a second round.
        state.refresh();
        state.refresh();
        settle(&mut state, &hosted());
        assert_eq!((rounds(), stub.looked_again.load(Ordering::SeqCst)), (3, 1));
    }

    #[test]
    fn a_failing_round_keeps_the_data_and_waits_longer() {
        let stub = Stub::default();
        let mut state = HostingState::default();
        {
            let stub = stub.clone();
            state.sync_with(&hosted(), true, true, move |_| Box::new(stub), || {});
        }
        settle(&mut state, &hosted());

        stub.fail.store(true, Ordering::SeqCst);
        state.refresh();
        let shown = settle(&mut state, &hosted());
        let view = &shown.view;
        assert_eq!(view.pulls.items.len(), 2);
        assert_eq!(view.pulls.problem, Some(Problem::Unreachable("no connection".to_owned())));
        assert_eq!(view.issues.items.len(), 2);
        assert!(view.runs.problem.is_none());
        // Twice the usual wait after one bad round.
        let due = state.session.as_ref().unwrap().next_auto.unwrap();
        assert!(due.saturating_duration_since(Instant::now()) > Duration::from_secs(350));

        stub.fail.store(false, Ordering::SeqCst);
        state.refresh();
        let shown = settle(&mut state, &hosted());
        assert!(shown.view.pulls.problem.is_none() && shown.view.issues.problem.is_none());
    }

    #[test]
    fn switching_off_or_repointing_the_remote_ends_the_session() {
        /// Dropped with the worker thread that owns it.
        struct Tracked(Stub, #[allow(dead_code)] Arc<()>);
        impl Backend for Tracked {
            fn pulls(&mut self, now: u64) -> Outcome<PullRequest> {
                self.0.pulls(now)
            }
            fn issues(&mut self, now: u64) -> Outcome<Issue> {
                self.0.issues(now)
            }
            fn runs(&mut self, now: u64) -> Outcome<Run> {
                self.0.runs(now)
            }
            fn auth(&self) -> Auth {
                self.0.auth()
            }
            fn look_again(&mut self) {
                self.0.look_again();
            }
            fn blocked_until(&self, now: u64) -> Option<u64> {
                self.0.blocked_until(now)
            }
        }
        let wait_until_alone = |alive: &Arc<()>| {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Arc::strong_count(alive) > 1 {
                assert!(Instant::now() < deadline, "the worker thread is still running");
                thread::sleep(Duration::from_millis(5));
            }
        };

        let alive = Arc::new(());
        let mut state = HostingState::default();
        {
            let alive = alive.clone();
            state.sync_with(&hosted(), true, true, move |_| Box::new(Tracked(Stub::default(), alive)), || {});
        }
        settle(&mut state, &hosted());

        // The remote now names another repository: a fresh session for it,
        // with none of the old repository's data.
        let moved = [remote("origin", "https://github.com/octo/other.git")];
        let second = Arc::new(());
        let shown = {
            let second = second.clone();
            let connect = move |_: &RepoId| -> Box<dyn Backend> { Box::new(Tracked(Stub::default(), second)) };
            state.sync_with(&moved, true, true, connect, || {}).unwrap()
        };
        assert_eq!(shown.repo.slug(), "octo/other");
        assert!(shown.view.pulls.items.is_empty() && !shown.view.pulls.loaded);
        wait_until_alone(&alive);

        // Switched off: the session and its thread go away.
        let never = |_: &RepoId| -> Box<dyn Backend> { panic!("must not connect") };
        assert!(state.sync_with(&moved, false, true, never, || {}).is_none());
        assert!(state.session.is_none());
        wait_until_alone(&second);
    }

    #[test]
    fn a_dead_worker_is_reported_instead_of_spinning_forever() {
        struct Broken;
        impl Backend for Broken {
            fn pulls(&mut self, _now: u64) -> Outcome<PullRequest> {
                panic!("simulated bug in a backend (expected in this test)")
            }
            fn issues(&mut self, _now: u64) -> Outcome<Issue> {
                Outcome::Unchanged
            }
            fn runs(&mut self, _now: u64) -> Outcome<Run> {
                Outcome::Unchanged
            }
            fn auth(&self) -> Auth {
                Auth::Anonymous
            }
            fn look_again(&mut self) {}
            fn blocked_until(&self, _now: u64) -> Option<u64> {
                None
            }
        }
        let mut state = HostingState::default();
        state.sync_with(&hosted(), true, true, |_| Box::new(Broken), || {});
        let shown = settle(&mut state, &hosted());
        let stopped = Some(Problem::Failed("The background worker stopped".to_owned()));
        assert_eq!(shown.view.pulls.problem, stopped);
        assert_eq!(shown.view.runs.problem, stopped);
        // Asking again does not hang or panic either.
        state.refresh();
        assert!(!settle(&mut state, &hosted()).view.busy);
    }

    // ---- checking out a pull request, against a real repository ----

    #[test]
    fn a_pull_request_is_checked_out_updated_and_never_forced() {
        use crate::tests::{Fixture, git};

        let fx = Fixture::new("hosting-pr");
        let origin = fx.dir.with_file_name(format!("hosting-pr-origin-{}.git", std::process::id()));
        let _ = std::fs::remove_dir_all(&origin);
        let origin_arg = origin.to_string_lossy().into_owned();

        fx.write("a.txt", "base\n");
        fx.commit_all("base");
        git(&fx.dir, &["clone", "-q", "--bare", ".", &origin_arg]);
        git(&fx.dir, &["remote", "add", "origin", &origin_arg]);

        // The remote offers the pull request the way GitHub does: as
        // refs/pull/<n>/head, outside the branches a normal fetch brings.
        let publish = |content: &str, message: &str, force: bool| {
            git(&fx.dir, &["checkout", "-q", "--detach"]);
            fx.write("pr.txt", content);
            fx.commit_all(message);
            let spec = format!("{}HEAD:refs/pull/7/head", if force { "+" } else { "" });
            git(&fx.dir, &["push", "-q", "origin", &spec]);
            git(&fx.dir, &["rev-parse", "HEAD"])
        };
        let first = publish("one\n", "PR: first", false);
        git(&fx.dir, &["checkout", "-q", "main"]);

        // Check out: a new local branch at the pull request's head.
        let done = fx.run(checkout_pull("origin", Service::GitHub, 7, false));
        assert_eq!(done.snap.head.branch.as_deref(), Some("pr/7"));
        assert_eq!(done.snap.head.oid.unwrap().to_string(), first);
        assert_eq!(fx.read("pr.txt"), "one\n");
        // Undo goes back to where we were; the branch stays.
        let undo = done.undo.expect("checking out is undoable");
        let back = fx.run_as(undo.undo.clone(), crate::worker::Origin::Undo(undo));
        assert_eq!(back.snap.head.branch.as_deref(), Some("main"));
        git(&fx.dir, &["checkout", "-q", "pr/7"]);

        // The pull request gains a commit; we are on its branch, which git
        // will not fetch into, so it is fast-forwarded instead.
        let second = publish("one\ntwo\n", "PR: second", false);
        git(&fx.dir, &["checkout", "-q", "pr/7"]);
        let done = fx.run(checkout_pull("origin", Service::GitHub, 7, true));
        assert_eq!(done.snap.head.branch.as_deref(), Some("pr/7"));
        assert_eq!(done.snap.head.oid.unwrap().to_string(), second);
        assert!(done.undo.is_some());

        // From another branch the same update goes through the fetch.
        let third = publish("one\ntwo\nthree\n", "PR: third", false);
        git(&fx.dir, &["checkout", "-q", "main"]);
        let done = fx.run(checkout_pull("origin", Service::GitHub, 7, false));
        assert_eq!(done.snap.head.oid.unwrap().to_string(), third);

        // A local commit on pr/7, then the author force-pushes something
        // unrelated: neither form throws the local commit away.
        fx.write("mine.txt", "local work\n");
        fx.commit_all("my own commit on the PR branch");
        let mine = git(&fx.dir, &["rev-parse", "HEAD"]);
        git(&fx.dir, &["checkout", "-q", "main"]);
        publish("rewritten\n", "PR: rewritten", true);

        git(&fx.dir, &["checkout", "-q", "main"]);
        let refused = fx.run_as(checkout_pull("origin", Service::GitHub, 7, false), crate::worker::Origin::User);
        assert!(refused.result.is_err(), "a non-fast-forward update must be refused");
        assert_eq!(refused.snap.head.branch.as_deref(), Some("main"));
        assert_eq!(git(&fx.dir, &["rev-parse", "pr/7"]), mine);

        git(&fx.dir, &["checkout", "-q", "pr/7"]);
        let refused = fx.run_as(checkout_pull("origin", Service::GitHub, 7, true), crate::worker::Origin::User);
        assert!(refused.result.is_err());
        assert_eq!(git(&fx.dir, &["rev-parse", "pr/7"]), mine);

        // A pull request that does not exist is an error, not a branch.
        git(&fx.dir, &["checkout", "-q", "main"]);
        let missing = fx.run_as(checkout_pull("origin", Service::GitHub, 99, false), crate::worker::Origin::User);
        assert!(missing.result.is_err());
        assert!(!missing.snap.branches.iter().any(|branch| branch.name == "pr/99"));

        drop(fx);
        let _ = std::fs::remove_dir_all(&origin);
    }
}
