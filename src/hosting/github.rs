//! GitHub: remote URLs, sign-in, REST endpoints and their JSON.

use serde::Deserialize;

use super::*;

/// The only place a token is ever sent.
pub const API: &str = "https://api.github.com";

pub(super) fn is_api(url: &str) -> bool {
    url.strip_prefix(API).is_some_and(|rest| rest.starts_with('/'))
}

/// Understands `https://github.com/o/r(.git)`, `git@github.com:o/r.git`,
/// `ssh://git@github.com/o/r` and their variations (user names, ports,
/// trailing slashes, `ssh.github.com`). Anything else, including other
/// hosts whose name merely contains "github.com", is `None`.
pub fn parse_remote(url: &str) -> Option<RepoId> {
    let url = url.trim();
    let (host, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            let known = ["https", "http", "ssh", "git", "git+ssh", "ssh+git"];
            if !known.contains(&scheme.to_ascii_lowercase().as_str()) {
                return None;
            }
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
            (host.split_once(':').map_or(host, |(host, _port)| host), path)
        }
        // scp-like: [user@]host:path. A slash before the colon means a
        // local path (`C:/x` has a one-letter "host" and fails below).
        None => {
            let (left, path) = url.split_once(':')?;
            if left.contains('/') || left.contains('\\') {
                return None;
            }
            (left.rsplit_once('@').map_or(left, |(_, host)| host), path)
        }
    };
    let hosts = ["github.com", "www.github.com", "ssh.github.com"];
    if !hosts.contains(&host.to_ascii_lowercase().as_str()) {
        return None;
    }

    let mut parts = path.trim_matches('/').split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let name = name.strip_suffix(".git").unwrap_or(name);
    let valid = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    (valid(owner) && valid(name)).then(|| RepoId {
        service: Service::GitHub,
        owner: owner.to_owned(),
        name: name.to_owned(),
    })
}

// ---- sign-in ----

/// The real environment: variables, `gh`, and `git credential`.
pub struct System {
    workdir: PathBuf,
}

impl System {
    pub fn new(workdir: &Path) -> Self {
        System { workdir: workdir.to_path_buf() }
    }
}

impl Secrets for System {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn cli(&self) -> Option<String> {
        let mut cmd = Command::new("gh");
        cmd.args(["auth", "token", "--hostname", "github.com"]).env("GH_PROMPT_DISABLED", "1");
        capture(cmd, b"", Duration::from_secs(10))
    }

    fn credential_helper(&self) -> Option<String> {
        // Asked so that nothing can open a window or wait for a
        // keyboard: the credential manager is told not to interact, and
        // every askpass fallback and the terminal prompt are disabled.
        // With nothing stored, git simply fails.
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.workdir)
            .args(["-c", "core.askPass=", "credential", "fill"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "");
        let answer = capture(cmd, b"protocol=https\nhost=github.com\n\n", Duration::from_secs(15))?;
        password_from(&answer)
    }
}

/// The `password=` line of `git credential fill` output.
pub fn password_from(answer: &str) -> Option<String> {
    answer.lines().find_map(|line| line.strip_prefix("password=")).map(str::to_owned)
}

// ---- the wire ----

/// The real network, through `ureq`.
pub struct Wire {
    agent: ureq::Agent,
}

impl Wire {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            // Statuses are data here (304, 403, 404 all mean something).
            .http_status_as_error(false)
            // Redirects come back to the client, which only follows
            // them within the API host.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .timeout_global(Some(Duration::from_secs(25)))
            .user_agent(concat!("GitKrill/", env!("CARGO_PKG_VERSION")))
            .build();
        Wire { agent: config.into() }
    }
}

impl Transport for Wire {
    fn get(&mut self, request: &HttpRequest<'_>) -> Result<HttpResponse, String> {
        let mut call = self
            .agent
            .get(request.url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(etag) = request.etag {
            call = call.header("If-None-Match", etag);
        }
        // The one line where a token leaves the process, and only
        // towards the API host over HTTPS.
        if let Some(token) = request.token
            && is_api(request.url)
        {
            call = call.header("Authorization", token.authorization());
        }

        let mut response = call.call().map_err(explain)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned())))
            .collect();
        let body = match status {
            304 => String::new(),
            _ => response
                .body_mut()
                .with_config()
                .limit(32 * 1024 * 1024)
                .read_to_string()
                .map_err(explain)?,
        };
        Ok(HttpResponse { status, headers, body })
    }
}

/// A transport failure in a few plain words. Built from the kind of
/// error, never from request contents.
fn explain(error: ureq::Error) -> String {
    match error {
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => "no connection".to_owned(),
        ureq::Error::Timeout(_) => "timed out".to_owned(),
        ureq::Error::Io(io) => format!("network error ({})", io.kind()),
        other => other.to_string(),
    }
}

// ---- JSON ----

#[derive(Deserialize)]
struct RawUser {
    #[serde(default)]
    login: String,
}

fn login(user: Option<RawUser>) -> String {
    user.map(|user| user.login).filter(|login| !login.is_empty()).unwrap_or_else(|| "ghost".to_owned())
}

#[derive(Deserialize)]
struct RawRepo {
    full_name: Option<String>,
}

#[derive(Deserialize)]
struct RawBranch {
    #[serde(rename = "ref", default)]
    name: String,
    #[serde(default)]
    sha: String,
    repo: Option<RawRepo>,
}

#[derive(Deserialize)]
struct RawPull {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    draft: bool,
    user: Option<RawUser>,
    head: RawBranch,
    base: RawBranch,
    updated_at: Option<String>,
}

pub fn parse_pulls(json: &str) -> Result<Vec<PullRequest>, String> {
    let raw: Vec<RawPull> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(raw
        .into_iter()
        .map(|pull| PullRequest {
            number: pull.number,
            title: pull.title,
            author: login(pull.user),
            draft: pull.draft,
            head_ref: pull.head.name,
            head_sha: pull.head.sha,
            head_repo: pull.head.repo.and_then(|repo| repo.full_name),
            base_ref: pull.base.name,
            updated: timestamp(pull.updated_at.as_deref()),
        })
        .collect())
}

#[derive(Deserialize)]
struct RawLabel {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct RawIssue {
    number: u64,
    #[serde(default)]
    title: String,
    user: Option<RawUser>,
    #[serde(default)]
    labels: Vec<RawLabel>,
    #[serde(default)]
    comments: u64,
    updated_at: Option<String>,
    /// Present exactly when the "issue" is really a pull request.
    pull_request: Option<serde_json::Value>,
}

/// The issues endpoint lists pull requests too; they are left out here.
pub fn parse_issues(json: &str) -> Result<Vec<Issue>, String> {
    let raw: Vec<RawIssue> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(raw
        .into_iter()
        .filter(|issue| issue.pull_request.is_none())
        .map(|issue| Issue {
            number: issue.number,
            title: issue.title,
            author: login(issue.user),
            labels: issue.labels.into_iter().map(|label| label.name).collect(),
            comments: issue.comments,
            updated: timestamp(issue.updated_at.as_deref()),
        })
        .collect())
}

#[derive(Deserialize)]
struct RawRun {
    id: u64,
    name: Option<String>,
    display_title: Option<String>,
    #[serde(default)]
    run_number: u64,
    head_branch: Option<String>,
    #[serde(default)]
    head_sha: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    status: Option<String>,
    conclusion: Option<String>,
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct RawRuns {
    #[serde(default)]
    total_count: u64,
    workflow_runs: Vec<RawRun>,
}

/// GitHub reports a `status` (queued, in_progress, completed, ...) and,
/// once completed, a `conclusion`.
pub fn run_state(status: &str, conclusion: Option<&str>) -> RunState {
    match (status, conclusion) {
        ("completed", Some("success")) => RunState::Success,
        ("completed", Some("failure" | "timed_out" | "startup_failure")) => RunState::Failure,
        ("completed", Some("cancelled")) => RunState::Cancelled,
        ("completed", Some("action_required")) | ("action_required", _) => RunState::NeedsAction,
        ("completed", _) => RunState::Skipped,
        ("in_progress", _) => RunState::Running,
        // queued, requested, waiting, pending and whatever comes next.
        _ => RunState::Queued,
    }
}

/// Returns the runs and how many the repository has in total.
pub fn parse_runs(json: &str) -> Result<(Vec<Run>, u64), String> {
    let raw: RawRuns = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let runs = raw
        .workflow_runs
        .into_iter()
        .map(|run| {
            let status = run.status.unwrap_or_default();
            let state = run_state(&status, run.conclusion.as_deref());
            Run {
                id: run.id,
                workflow: run.name.filter(|name| !name.is_empty()).unwrap_or_else(|| "Workflow".to_owned()),
                title: run.display_title.unwrap_or_default(),
                number: run.run_number,
                branch: run.head_branch.unwrap_or_default(),
                event: run.event,
                head_sha: run.head_sha,
                state,
                outcome: run.conclusion.unwrap_or(status).replace('_', " "),
                when: timestamp(run.created_at.as_deref()),
            }
        })
        .collect();
    Ok((runs, raw.total_count))
}

/// The `message` of an API error body, or nothing.
fn api_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct RawError {
        message: String,
    }
    serde_json::from_str::<RawError>(body).map(|error| error.message).unwrap_or_default()
}

// ---- the client ----

#[derive(Clone, Copy)]
enum List {
    Pulls,
    Issues,
    Runs,
}

impl List {
    fn url(self, repo: &RepoId) -> String {
        let base = format!("{API}/repos/{}/{}", repo.owner, repo.name);
        match self {
            List::Pulls => format!("{base}/pulls?state=open&per_page=30"),
            // Pull requests come back mixed in and are filtered out, so
            // ask for a bigger page.
            List::Issues => format!("{base}/issues?state=open&per_page=50"),
            List::Runs => format!("{base}/actions/runs?per_page=20&exclude_pull_requests=true"),
        }
    }
}

enum Got {
    Body {
        text: String,
        /// The `Link` header names a next page.
        next: bool,
    },
    Unchanged,
    /// The feature is switched off for this repository (410 Gone).
    Nothing,
}

pub struct Client {
    repo: RepoId,
    transport: Box<dyn Transport>,
    secrets: Box<dyn Secrets>,
    token: Option<(Token, TokenSource)>,
    /// Tokens the API answered 401 to.
    rejected: Vec<Token>,
    /// Whether `secrets` has been consulted since the last reason to.
    looked: bool,
    limiter: Limiter,
    /// Last `ETag` per list, for conditional requests.
    etags: [Option<String>; 3],
}

impl Client {
    pub fn new(repo: RepoId, transport: Box<dyn Transport>, secrets: Box<dyn Secrets>) -> Self {
        Client {
            repo,
            transport,
            secrets,
            token: None,
            rejected: Vec::new(),
            looked: false,
            limiter: Limiter::default(),
            etags: [None, None, None],
        }
    }

    /// Replaces the identity requests are made with. Validators and
    /// rate limits belong to an identity, so both start over.
    fn sign_in(&mut self, token: Option<(Token, TokenSource)>) {
        if self.token.as_ref().map(|(token, _)| token) != token.as_ref().map(|(token, _)| token) {
            self.etags = [None, None, None];
            self.limiter = Limiter::default();
        }
        self.token = token;
        self.looked = true;
    }

    fn get(&mut self, list: List, now: u64) -> Result<Got, Problem> {
        let mut url = list.url(&self.repo);
        let mut hops = 0;
        loop {
            if !self.looked {
                let found = find_token(self.secrets.as_ref(), &self.rejected);
                self.sign_in(found);
            }
            if let Some(until) = self.limiter.blocked(now) {
                return Err(Problem::RateLimited { until });
            }
            // Every address requested is on the API host; redirects
            // elsewhere are refused below before anything is sent.
            let request = HttpRequest {
                url: &url,
                etag: self.etags[list as usize].as_deref(),
                token: self.token.as_ref().map(|(token, _)| token),
            };
            let response = self.transport.get(&request).map_err(Problem::Unreachable)?;
            let message = api_message(&response.body);
            let rate = Rate::of(&response);
            if let Some(until) = self.limiter.record(response.status, &rate, &message, now) {
                return Err(Problem::RateLimited { until });
            }

            return match response.status {
                200 => {
                    self.etags[list as usize] = response.header("etag").map(str::to_owned);
                    let next = response.header("link").is_some_and(|link| link.contains("rel=\"next\""));
                    Ok(Got::Body { text: response.body, next })
                }
                304 => Ok(Got::Unchanged),
                // A renamed or transferred repository answers with a
                // redirect to its permanent API address.
                301 | 302 | 307 | 308 => match response.header("location") {
                    Some(location) if is_api(location) && hops < 3 => {
                        url = location.to_owned();
                        hops += 1;
                        continue;
                    }
                    _ => Err(Problem::Failed("GitHub sent an unexpected redirect".to_owned())),
                },
                // A bad or expired token: forget it and try the next
                // source, ending with no token at all.
                401 if self.token.is_some() => {
                    if let Some((token, _)) = self.token.take() {
                        self.rejected.push(token);
                    }
                    let found = find_token(self.secrets.as_ref(), &self.rejected);
                    self.sign_in(found);
                    continue;
                }
                // Private repositories look like missing ones from outside.
                401 | 404 if self.token.is_none() => Err(if self.rejected.is_empty() {
                    Problem::NotSignedIn
                } else {
                    Problem::SignInRejected
                }),
                404 => Err(Problem::NoAccess),
                410 => Ok(Got::Nothing),
                403 if !message.is_empty() => Err(Problem::Refused(message)),
                status => Err(Problem::Failed(format!("GitHub answered {status}"))),
            };
        }
    }

    fn fetch<T>(
        &mut self,
        list: List,
        now: u64,
        parse: impl FnOnce(&str) -> Result<(Vec<T>, bool), String>,
    ) -> Outcome<T> {
        let outcome = match self.get(list, now) {
            Ok(Got::Unchanged) => Outcome::Unchanged,
            Ok(Got::Nothing) => Outcome::Fresh { items: Vec::new(), more: false },
            Ok(Got::Body { text, next }) => match parse(&text) {
                Ok((items, more)) => Outcome::Fresh { items, more: more || next },
                Err(error) => {
                    // Do not let a later "not modified" vouch for an
                    // answer we could not read.
                    self.etags[list as usize] = None;
                    Outcome::Failed(Problem::Failed(format!("Unreadable answer from GitHub ({error})")))
                }
            },
            Err(problem) => Outcome::Failed(problem),
        };
        match outcome {
            Outcome::Failed(problem) => {
                let mut tokens: Vec<&Token> = self.rejected.iter().collect();
                tokens.extend(self.token.as_ref().map(|(token, _)| token));
                Outcome::Failed(problem.scrubbed(&tokens))
            }
            fine => fine,
        }
    }
}

impl Backend for Client {
    fn pulls(&mut self, now: u64) -> Outcome<PullRequest> {
        self.fetch(List::Pulls, now, |json| Ok((parse_pulls(json)?, false)))
    }

    fn issues(&mut self, now: u64) -> Outcome<Issue> {
        self.fetch(List::Issues, now, |json| Ok((parse_issues(json)?, false)))
    }

    fn runs(&mut self, now: u64) -> Outcome<Run> {
        self.fetch(List::Runs, now, |json| {
            let (runs, total) = parse_runs(json)?;
            let more = total > runs.len() as u64;
            Ok((runs, more))
        })
    }

    fn auth(&self) -> Auth {
        match &self.token {
            Some((_, source)) => Auth::SignedIn(*source),
            None if !self.looked => Auth::Unknown,
            None if self.rejected.is_empty() => Auth::Anonymous,
            None => Auth::Rejected,
        }
    }

    fn look_again(&mut self) {
        // The user may have signed in since (`gh auth login`, a push
        // through the credential manager). A working token is kept.
        if self.token.is_none() {
            self.looked = false;
        }
    }

    fn blocked_until(&self, now: u64) -> Option<u64> {
        self.limiter.blocked(now)
    }
}
