//! GitHub, on github.com and on Enterprise servers: the REST endpoints and
//! their JSON. Both speak the same API; only its address differs.

use serde::Deserialize;

use super::*;

/// github.com's API. Enterprise servers have theirs at `/api/v3` on their
/// own host (see [`RepoId::api_base`]).
pub const API: &str = "https://api.github.com";

/// Sent with every request, to github.com and to Enterprise servers alike.
pub const HEADERS: &[(&str, &str)] =
    &[("Accept", "application/vnd.github+json"), ("X-GitHub-Api-Version", "2022-11-28")];

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
    fn url(self, api: &str, repo: &RepoId) -> String {
        let base = format!("{api}/repos/{}/{}", repo.owner, repo.name);
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
    /// [`RepoId::api_base`]: every request goes below it.
    api: String,
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
            api: repo.api_base(),
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
        let mut url = list.url(&self.api, &self.repo);
        let mut hops = 0;
        loop {
            if !self.looked {
                let found = find_token(self.secrets.as_ref(), &self.repo, &self.rejected);
                self.sign_in(found);
            }
            if let Some(until) = self.limiter.blocked(now) {
                return Err(Problem::RateLimited { until });
            }
            // Every address requested is below the API's; redirects
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
                    Some(location) if within(&self.api, location) && hops < 3 => {
                        url = location.to_owned();
                        hops += 1;
                        continue;
                    }
                    _ => Err(Problem::Failed(format!("{} sent an unexpected redirect", self.repo.service.name()))),
                },
                // A bad or expired token: forget it and try the next
                // source, ending with no token at all.
                401 if self.token.is_some() => {
                    if let Some((token, _)) = self.token.take() {
                        self.rejected.push(token);
                    }
                    let found = find_token(self.secrets.as_ref(), &self.repo, &self.rejected);
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
                status => Err(Problem::Failed(format!("{} answered {status}", self.repo.service.name()))),
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
                    let service = self.repo.service.name();
                    Outcome::Failed(Problem::Failed(format!("Unreadable answer from {service} ({error})")))
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
