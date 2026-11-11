//! Gitea and Forgejo (its fork, which codeberg.org runs): the REST API (v1)
//! endpoints and their JSON.
//!
//! The API follows GitHub's closely: `/repos/{owner}/{repo}/pulls`,
//! `/issues` (with `type=issues`, so pull requests stay out) and, for CI,
//! `/actions/runs` (Gitea 1.24 and Forgejo 10, each in its own shape) or the
//! older `/actions/tasks`, tried in that order. A server without Actions has
//! neither, and its CI section stays empty.
//!
//! After the lists ([`Backend::details`]): the repository itself once (a
//! fork's `parent`), and the reviews of each open pull request that changed
//! since they were last asked (`/pulls/{index}/reviews`), which the list
//! does not carry. A pull request into a fork's parent names its branch
//! `owner:branch`, as the API's `head` takes it.
//!
//! Sign-ins: `CODEBERG_TOKEN` for codeberg.org, `GITEA_TOKEN` for the Gitea
//! server `GITEA_HOST` names, `FORGEJO_TOKEN` for the Forgejo server (or
//! codeberg.org) `FORGEJO_HOST` names, then the login `tea` keeps for the
//! host in its configuration file, then Git's credential helper.

use serde::Deserialize;

use super::signin::{Api, Got, json, plain_message};
use super::*;

/// Sent with every request.
pub const HEADERS: &[(&str, &str)] = &[("Accept", "application/json")];

/// The token variables for a server of `service`, in the order tried.
/// `names(variable)` says whether the host variable names this server
/// (`None` when it is not set): a variable is only used for the host it
/// belongs to.
pub fn env_names(service: Service, names: impl Fn(&str) -> Option<bool>) -> &'static [&'static str] {
    match service {
        Service::Codeberg if names("FORGEJO_HOST") == Some(true) => &["CODEBERG_TOKEN", "FORGEJO_TOKEN"],
        Service::Codeberg => &["CODEBERG_TOKEN"],
        Service::Forgejo if names("FORGEJO_HOST") == Some(true) => &["FORGEJO_TOKEN"],
        Service::Gitea if names("GITEA_HOST") == Some(true) => &["GITEA_TOKEN"],
        _ => &[],
    }
}

// ---- tea's sign-ins ----

/// The token `tea` (Gitea's command line tool, which serves Forgejo too)
/// keeps for `host`. tea prints none, so its configuration file is read:
/// the first place one exists, as tea looks for it.
pub fn tea_token(host: &str) -> Option<String> {
    tea_config_paths().into_iter().find_map(|path| {
        // A sign-in file, not something to read megabytes of.
        let size = std::fs::metadata(&path).ok()?.len();
        if size > 1024 * 1024 {
            return None;
        }
        tea_login_token(&std::fs::read_to_string(path).ok()?, host)
    })
}

/// Where tea keeps its configuration: `$XDG_CONFIG_HOME/tea/config.yml`,
/// else the system's configuration folder (the local one on Windows), then
/// the places older versions used.
fn tea_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        paths.push(PathBuf::from(xdg).join("tea").join("config.yml"));
    }
    let system = if cfg!(windows) { dirs::data_local_dir() } else { dirs::config_dir() };
    if let Some(dir) = system {
        paths.push(dir.join("tea").join("config.yml"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".config").join("tea").join("config.yml"));
        paths.push(home.join(".tea").join("tea.yml"));
    }
    paths
}

/// The token of the login for `host` in tea's configuration:
///
/// ```yaml
/// logins:
///   - name: codeberg.org
///     url: https://codeberg.org
///     token: 0123abcd...
/// ```
///
/// Only the `url` and `token` of each entry under `logins` are read; a
/// login whose address is on another host (or below a path) is not this
/// host's.
pub fn tea_login_token(config: &str, host: &str) -> Option<String> {
    let unquote = |value: &str| {
        let value = value.trim();
        let quoted = value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\'')));
        if quoted { value[1..value.len() - 1].to_owned() } else { value.to_owned() }
    };
    let mut in_logins = false;
    // (url, token) of each login.
    let mut logins: Vec<(Option<String>, Option<String>)> = Vec::new();
    for line in config.lines() {
        let content = line.trim_end();
        if content.trim().is_empty() || content.trim_start().starts_with('#') {
            continue;
        }
        if !content.starts_with([' ', '\t', '-']) {
            in_logins = content == "logins:";
            continue;
        }
        if !in_logins {
            continue;
        }
        let mut item = content.trim_start();
        if item == "-" {
            logins.push((None, None));
            continue;
        }
        if let Some(rest) = item.strip_prefix("- ") {
            logins.push((None, None));
            item = rest.trim_start();
        }
        let (Some((key, value)), Some(login)) = (item.split_once(':'), logins.last_mut()) else { continue };
        match key.trim() {
            "url" => login.0 = Some(unquote(value)),
            "token" => login.1 = Some(unquote(value)),
            _ => {}
        }
    }
    logins
        .into_iter()
        .find(|(url, token)| {
            url.as_deref().is_some_and(|url| same_host(url, host)) && token.as_deref().is_some_and(|token| !token.is_empty())
        })
        .and_then(|(_, token)| token)
}

// ---- JSON ----

#[derive(Deserialize)]
struct RawUser {
    #[serde(default)]
    login: String,
}

/// Gitea credits a deleted account's work to "Ghost".
fn login(user: Option<RawUser>) -> String {
    user.map(|user| user.login).filter(|login| !login.is_empty()).unwrap_or_else(|| "Ghost".to_owned())
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
    /// Newer servers say so; older ones only have the title's prefix.
    draft: Option<bool>,
    user: Option<RawUser>,
    head: RawBranch,
    base: RawBranch,
    updated_at: Option<String>,
}

/// The title prefixes Gitea and Forgejo read as "work in progress" by
/// default, and the one GitKrill writes.
const DRAFT_PREFIXES: &[&str] = &["wip:", "[wip]", "draft:", "[draft]"];

fn marked_draft(title: &str) -> bool {
    let lower = title.trim_start().to_lowercase();
    DRAFT_PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
}

// `..Default::default()` keeps fields added to `PullRequest` later at "unknown".
#[allow(clippy::needless_update, reason = "PullRequest gains fields this backend leaves unknown")]
fn pull_request(pull: RawPull) -> PullRequest {
    PullRequest {
        number: pull.number,
        draft: pull.draft.unwrap_or(false) || marked_draft(&pull.title),
        title: pull.title,
        author: login(pull.user),
        head_ref: pull.head.name,
        head_sha: pull.head.sha,
        head_repo: pull.head.repo.and_then(|repo| repo.full_name),
        base_ref: pull.base.name,
        updated: timestamp(pull.updated_at.as_deref()),
        ..Default::default()
    }
}

pub fn parse_pulls(text: &str) -> Result<Vec<PullRequest>, String> {
    Ok(json::<Vec<RawPull>>(text)?.into_iter().map(pull_request).collect())
}

/// One pull request, as the API answers its creation.
pub fn parse_pull(text: &str) -> Result<PullRequest, String> {
    json::<RawPull>(text).map(pull_request)
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
    /// Set when the "issue" is a pull request (`type=issues` should leave
    /// those out already).
    pull_request: Option<serde_json::Value>,
}

pub fn parse_issues(text: &str) -> Result<Vec<Issue>, String> {
    Ok(json::<Vec<RawIssue>>(text)?
        .into_iter()
        .filter(|issue| issue.pull_request.as_ref().is_none_or(serde_json::Value::is_null))
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

/// One run, in any of the shapes the servers send: Gitea's `/actions/runs`
/// (GitHub's fields: `run_number`, `status` with a `conclusion`, `path`),
/// Forgejo's (`index`, `title`, `workflow_id`, `prettyref`, `commit_sha`,
/// one `status` word) and the older `/actions/tasks` (one row per job, with
/// `name`, `run_number` and one `status` word).
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRun {
    id: u64,
    run_number: Option<u64>,
    index: Option<u64>,
    name: Option<String>,
    workflow_id: Option<serde_json::Value>,
    path: Option<String>,
    display_title: Option<String>,
    title: Option<String>,
    head_branch: Option<String>,
    prettyref: Option<String>,
    head_sha: Option<String>,
    commit_sha: Option<String>,
    event: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    need_approval: bool,
    created_at: Option<String>,
    created: Option<String>,
    started_at: Option<String>,
    run_started_at: Option<String>,
}

#[derive(Deserialize)]
struct RawRuns {
    #[serde(default)]
    total_count: u64,
    workflow_runs: Vec<RawRun>,
}

/// A run's state from one `status` word (Forgejo, and tasks), or from a
/// `status` and `conclusion` pair (Gitea's runs, as GitHub has them).
pub fn run_state(status: &str, conclusion: Option<&str>) -> RunState {
    match (status, conclusion) {
        ("success", None) => RunState::Success,
        ("failure", None) => RunState::Failure,
        ("cancelled", None) => RunState::Cancelled,
        ("skipped", None) => RunState::Skipped,
        ("running", None) => RunState::Running,
        // Waiting for someone to approve a run from a fork.
        ("blocked", None) => RunState::NeedsAction,
        _ => github::run_state(status, conclusion),
    }
}

/// `ci.yml` (or `ci.yml@refs/heads/main`) as "ci".
fn workflow_name(file: &str) -> String {
    let file = file.split('@').next().unwrap_or(file);
    let file = file.rsplit('/').next().unwrap_or(file);
    let stem = file.strip_suffix(".yml").or_else(|| file.strip_suffix(".yaml")).unwrap_or(file);
    stem.to_owned()
}

fn run_model(raw: RawRun) -> Run {
    let number = raw.run_number.or(raw.index).unwrap_or(raw.id);
    let file = match raw.workflow_id {
        Some(serde_json::Value::String(file)) => Some(file),
        _ => raw.path,
    };
    let workflow = raw
        .name
        .filter(|name| !name.is_empty())
        .or_else(|| file.as_deref().map(workflow_name).filter(|name| !name.is_empty()))
        .unwrap_or_else(|| "Workflow".to_owned());
    let status = raw.status.unwrap_or_default();
    let state = if raw.need_approval { RunState::NeedsAction } else { run_state(&status, raw.conclusion.as_deref()) };
    Run {
        // The web page of a run is numbered by its number in the repository.
        id: number,
        workflow,
        title: raw.display_title.or(raw.title).unwrap_or_default(),
        number,
        branch: raw.head_branch.or(raw.prettyref).unwrap_or_default(),
        event: raw.event.unwrap_or_default().replace('_', " "),
        head_sha: raw.head_sha.or(raw.commit_sha).unwrap_or_default(),
        state,
        outcome: raw.conclusion.unwrap_or(status).replace('_', " "),
        when: timestamp(raw.created_at.or(raw.created).or(raw.started_at).or(raw.run_started_at).as_deref()),
    }
}

/// How bad a state is, to merge a run's jobs: failure first, then waiting
/// on someone, running, queued, cancelled, skipped, success.
fn severity(state: RunState) -> u8 {
    match state {
        RunState::Failure => 6,
        RunState::NeedsAction => 5,
        RunState::Running => 4,
        RunState::Queued => 3,
        RunState::Cancelled => 2,
        RunState::Skipped => 1,
        RunState::Success => 0,
    }
}

/// The runs, and whether the server has more. `tasks` lists jobs: those of
/// one run become one row, in the state of its worst job.
pub fn parse_runs(text: &str, tasks: bool) -> Result<(Vec<Run>, bool), String> {
    let raw: RawRuns = json(text)?;
    let total = raw.total_count;
    let mut runs: Vec<Run> = Vec::new();
    let count = raw.workflow_runs.len() as u64;
    for run in raw.workflow_runs.into_iter().map(run_model) {
        match runs.iter_mut().find(|seen| tasks && seen.number == run.number) {
            Some(seen) if severity(run.state) > severity(seen.state) => {
                seen.state = run.state;
                seen.outcome = run.outcome;
            }
            Some(_) => {}
            None => runs.push(run),
        }
    }
    Ok((runs, total > count))
}

/// What a pull request's reviews (`GET /repos/{owner}/{repo}/pulls/{index}/
/// reviews`) come to: each reviewer's latest approval or request for
/// changes, unless it was dismissed since. Only official reviews count
/// (those of reviewers the branch's rules, or write access, let count);
/// servers too old to say are taken at their word. Comments, pending
/// reviews and requests for a review leave a reviewer's word as it was.
/// The rules' required number of approvals is not in the answer, so a pull
/// request is never "review required" here.
pub fn parse_reviews(text: &str) -> Result<(Review, Approvals), String> {
    #[derive(Deserialize)]
    struct RawReview {
        #[serde(default)]
        id: u64,
        user: Option<RawUser>,
        #[serde(default)]
        state: String,
        #[serde(default)]
        dismissed: bool,
        official: Option<bool>,
    }
    let mut raw: Vec<RawReview> = json(text)?;
    // Oldest first, whatever order the server lists them in.
    raw.sort_by_key(|review| review.id);
    // Each reviewer's last word, and whether it was dismissed.
    let mut words: Vec<(String, &'static str, bool)> = Vec::new();
    for review in &raw {
        let Some(login) = review.user.as_ref().map(|user| user.login.clone()).filter(|login| !login.is_empty()) else {
            continue;
        };
        let word = match review.state.as_str() {
            "APPROVED" => "APPROVED",
            "REQUEST_CHANGES" => "CHANGES_REQUESTED",
            _ => continue,
        };
        if !review.official.unwrap_or(true) {
            continue;
        }
        match words.iter_mut().find(|(who, ..)| *who == login) {
            Some(last) => *last = (login, word, review.dismissed),
            None => words.push((login, word, review.dismissed)),
        }
    }
    let states: Vec<String> =
        words.into_iter().filter(|(_, _, dismissed)| !dismissed).map(|(_, word, _)| word.to_owned()).collect();
    Ok(github::review_of(None, &states))
}

/// The body of a new pull request. Gitea and Forgejo take no draft flag:
/// a "WIP:" title is what makes one.
pub fn new_pull_json(new: &NewPull) -> String {
    let title = new.title.trim();
    let title = if new.draft && !marked_draft(title) { format!("WIP: {title}") } else { title.to_owned() };
    serde_json::json!({ "head": new.head, "base": new.base, "title": title, "body": new.body }).to_string()
}

// ---- the client ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum List {
    Pulls,
    Issues,
    Runs,
}

/// Where the server lists its runs, once known.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RunsAt {
    Unknown,
    Runs,
    Tasks,
    /// Neither endpoint exists: no Actions here.
    Nowhere,
}

/// Review requests at most per round, one per pull request: the whole list
/// signed in, fewer without a sign-in, whose allowance is smaller.
const REVIEWS_AT_ONCE: usize = 30;
const PUBLIC_REVIEWS_AT_ONCE: usize = 10;

pub struct Client {
    api: Api,
    runs_at: RunsAt,
    /// Review states of the listed pull requests, as last asked.
    reviews: Reviews,
    /// The repository's parent once asked; `Some(None)` when it has none.
    parent: Option<Option<Parent>>,
}

impl Client {
    pub fn new(repo: RepoId, transport: Box<dyn Transport>, secrets: Box<dyn Secrets>) -> Self {
        Client {
            api: Api::new(repo, transport, secrets, 3, &[401], plain_message),
            runs_at: RunsAt::Unknown,
            reviews: Reviews::default(),
            parent: None,
        }
    }

    fn repo_url(&self, path: &str) -> String {
        Self::url_of(&self.api.base, &self.api.repo, path)
    }

    fn url_of(base: &str, repo: &RepoId, path: &str) -> String {
        format!("{base}/repos/{}/{}/{path}", repo.owner, repo.name)
    }

    /// `rel="next"` in the `Link` header.
    fn linked(response: &HttpResponse) -> bool {
        response.header("link").is_some_and(|link| link.contains("rel=\"next\""))
    }

    /// POST /repos/{owner}/{repo}/pulls of `into`: this repository, or
    /// another of the server (a fork's parent).
    fn create(&mut self, into: &RepoId, new: &NewPull, now: u64) -> Result<PullRequest, Problem> {
        // Not signed in so far: the user may have signed in since.
        self.api.look_again();
        let url = Self::url_of(&self.api.base, into, "pulls");
        let response = self.api.post(&url, &new_pull_json(new), true, now)?;
        let message = self.api.message(&response.body);
        let site = self.api.repo.site();
        let (head, base) = (&new.head, &new.base);
        match response.status {
            200 | 201 => parse_pull(&response.body).map_err(|error| self.api.unreadable(error)),
            409 => Err(Problem::Refused(format!("A pull request from {head} into {base} is already open"))),
            // The repository was listed a moment ago: what is missing is a branch.
            404 => Err(Problem::Refused(format!("{site} does not have the branch {head} or {base}: push it first"))),
            403 => Err(Problem::Refused(match message.is_empty() {
                true => format!("No permission to open pull requests in {}", into.slug()),
                false => format!("No permission to open pull requests in {} ({message})", into.slug()),
            })),
            400 | 422 if message.to_lowercase().contains("no changes") => {
                Err(Problem::Refused(format!("No commits between {base} and {head}: there is nothing to merge")))
            }
            400 | 422 if !message.is_empty() => Err(Problem::Refused(message)),
            status => Err(Problem::Failed(format!("{} answered {status}", self.api.name()))),
        }
    }

    /// The runs from `at`, which may turn out not to exist (404): then the
    /// next place is tried.
    fn runs_from(&mut self, at: RunsAt, now: u64) -> Option<Outcome<Run>> {
        let (path, tasks) = match at {
            RunsAt::Runs | RunsAt::Unknown => ("actions/runs?limit=20", false),
            RunsAt::Tasks => ("actions/tasks?limit=30", true),
            RunsAt::Nowhere => return Some(Outcome::Fresh { items: Vec::new(), more: false }),
        };
        let mut missing = false;
        let url = self.repo_url(path);
        let outcome = self.api.fetch(
            url,
            List::Runs as usize,
            now,
            |response, _| {
                missing = response.status == 404;
                missing.then_some(Ok(Got::Nothing))
            },
            |response| parse_runs(&response.body, tasks),
        );
        (!missing).then_some(outcome)
    }

    /// GET /repos/{owner}/{repo}, for the parent of a fork. `None` when
    /// there was no answer worth keeping (no connection, a rate limit, the
    /// server failing): asked again next round.
    fn ask_parent(&mut self, now: u64) -> Option<Option<Parent>> {
        let url = self.repo_url("").trim_end_matches('/').to_owned();
        let response = self.api.get(url, None, now).ok()?;
        match response.status {
            200..=299 => Some(github::parse_parent(&response.body, &self.api.repo).unwrap_or(None)),
            500..=599 => None,
            // Moved, hidden or refused: no parent to offer.
            _ => Some(None),
        }
    }

    /// The reviews of the listed pull requests that changed since they were
    /// last asked, one request each and at most a list's worth per round
    /// (fewer without a sign-in). Stops at the first request that gets no
    /// answer worth keeping; the rest wait for the next round. Returns those
    /// whose state is not what it was.
    fn ask_reviews(&mut self, now: u64) -> Vec<PullRequest> {
        let limit = if self.api.signed_in() { REVIEWS_AT_ONCE } else { PUBLIC_REVIEWS_AT_ONCE };
        let mut changed = Vec::new();
        for (number, updated) in self.reviews.stale(limit) {
            let url = self.repo_url(&format!("pulls/{number}/reviews?limit=50"));
            let Ok(response) = self.api.get(url, None, now) else { break };
            let (review, approvals) = match response.status {
                200..=299 => match parse_reviews(&response.body) {
                    Ok((review, approvals)) => (review, Some(approvals)),
                    Err(_) => (Review::Unknown, None),
                },
                500..=599 => break,
                // Hidden or gone: unknown until the pull request changes.
                _ => (Review::Unknown, None),
            };
            changed.extend(self.reviews.learnt(number, updated, review, approvals));
        }
        changed
    }
}

impl Backend for Client {
    fn pulls(&mut self, now: u64) -> Outcome<PullRequest> {
        self.api.start_round();
        let url = self.repo_url("pulls?state=open&sort=recentupdate&limit=30");
        let outcome = self.api.fetch(url, List::Pulls as usize, now, |_, _| None, |response| {
            Ok((parse_pulls(&response.body)?, Self::linked(response)))
        });
        match outcome {
            Outcome::Fresh { mut items, more } => {
                self.reviews.listed(&mut items);
                Outcome::Fresh { items, more }
            }
            other => other,
        }
    }

    fn issues(&mut self, now: u64) -> Outcome<Issue> {
        let url = self.repo_url("issues?state=open&type=issues&limit=30");
        // With a sign-in, "not found" here means the tracker is off (or
        // elsewhere); access itself shows in the pull request list.
        let special = |response: &HttpResponse, signed_in: bool| (signed_in && response.status == 404).then_some(Ok(Got::Nothing));
        self.api.fetch(url, List::Issues as usize, now, special, |response| {
            Ok((parse_issues(&response.body)?, Self::linked(response)))
        })
    }

    fn runs(&mut self, now: u64) -> Outcome<Run> {
        let order: &[RunsAt] = match self.runs_at {
            RunsAt::Unknown => &[RunsAt::Runs, RunsAt::Tasks],
            known => &[known],
        };
        for &at in order {
            if let Some(outcome) = self.runs_from(at, now) {
                if !matches!(outcome, Outcome::Failed(_)) {
                    self.runs_at = at;
                }
                return outcome;
            }
        }
        self.runs_at = RunsAt::Nowhere;
        Outcome::Fresh { items: Vec::new(), more: false }
    }

    fn auth(&self) -> Auth {
        self.api.auth()
    }

    fn look_again(&mut self) {
        self.api.look_again();
        // Actions may have been switched on since.
        if self.runs_at == RunsAt::Nowhere {
            self.runs_at = RunsAt::Unknown;
        }
        // Reviews are asked about again, and shown as they were meanwhile.
        self.reviews.ask_again();
    }

    fn blocked_until(&self, now: u64) -> Option<u64> {
        self.api.blocked_until(now)
    }

    fn create_pull(&mut self, new: &NewPull, now: u64) -> Result<PullRequest, Problem> {
        let into = self.api.repo.clone();
        let created = self.create(&into, new, now);
        created.map_err(|problem| self.api.scrub(problem))
    }

    /// Opened in `across.into`; a branch of another repository (a fork) is
    /// `owner:branch` there.
    fn create_pull_across(&mut self, new: &NewPull, across: &Across, now: u64) -> Result<PullRequest, Problem> {
        let here = |repo: &RepoId| repo.service == self.api.repo.service && repo.host == self.api.repo.host;
        if !here(&across.from) || !here(&across.into) {
            return Err(Problem::Failed(format!("Both repositories must be on {}", self.api.repo.site())));
        }
        let head = match across.from.same(&across.into) {
            true => new.head.clone(),
            false => format!("{}:{}", across.from.owner, new.head),
        };
        let created = self.create(&across.into, &NewPull { head, ..new.clone() }, now);
        created.map_err(|problem| self.api.scrub(problem))
    }

    fn details(&mut self, now: u64) -> Option<Details> {
        let mut details = Details::default();
        if self.parent.is_none()
            && let Some(parent) = self.ask_parent(now)
        {
            details.parent = parent.clone();
            self.parent = Some(parent);
        }
        details.pulls = self.ask_reviews(now);
        (details != Details::default()).then_some(details)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fake, Log, NOW, OTHER, SECRET, Script, reply};
    use super::*;

    fn codeberg() -> RepoId {
        parse_remote("https://codeberg.org/team/app.git", &BTreeMap::new()).unwrap()
    }

    fn scripted(repo: RepoId, replies: Vec<Result<HttpResponse, String>>, secrets: Fake) -> (Client, Log) {
        let sent = Log::default();
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        (Client::new(repo, Box::new(transport), Box::new(secrets)), sent)
    }

    fn with_token() -> Fake {
        Fake { env: vec![("CODEBERG_TOKEN", SECRET)], ..Default::default() }
    }

    fn failure<T: std::fmt::Debug>(outcome: Outcome<T>) -> Problem {
        match outcome {
            Outcome::Failed(problem) => problem,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    fn urls(sent: &Log) -> Vec<String> {
        sent.lock().unwrap().iter().map(|request| request.url.clone()).collect()
    }

    // Trimmed-down answers, after Gitea's and Forgejo's API documentation.
    const PULLS: &str = r#"[
      {"id": 501, "number": 12, "title": "Add dark mode", "state": "open", "draft": false,
       "user": {"id": 1, "login": "ada", "full_name": "Ada"},
       "head": {"label": "feature/dark", "ref": "feature/dark", "sha": "1111111111111111111111111111111111111111",
                "repo_id": 7, "repo": {"id": 7, "full_name": "team/app"}},
       "base": {"label": "main", "ref": "main", "sha": "0000000000000000000000000000000000000000",
                "repo_id": 7, "repo": {"id": 7, "full_name": "team/app"}},
       "mergeable": true, "comments": 2, "updated_at": "1970-01-02T00:00:00Z",
       "html_url": "https://codeberg.org/team/app/pulls/12"},
      {"id": 502, "number": 9, "title": "WIP: parser", "user": null,
       "head": {"ref": "parser", "sha": "2222222222222222222222222222222222222222", "repo": {"full_name": "grace/app"}},
       "base": {"ref": "main", "sha": "0000000000000000000000000000000000000000"}},
      {"number": 8, "title": "Docs", "draft": true, "user": {"login": "linus"},
       "head": {"ref": "docs", "sha": "", "repo": null}, "base": {"ref": "main"}}
    ]"#;

    const ISSUES: &str = r#"[
      {"id": 1, "number": 31, "title": "Crash on empty repo", "user": {"login": "grace"},
       "labels": [{"id": 3, "name": "bug", "color": "ee0701"}], "comments": 4, "state": "open",
       "updated_at": "1970-01-01T00:01:00Z", "pull_request": null},
      {"id": 2, "number": 12, "title": "Add dark mode", "user": {"login": "ada"},
       "pull_request": {"merged": false, "html_url": "https://codeberg.org/team/app/pulls/12"}},
      {"id": 3, "number": 30, "title": "Docs typo"}
    ]"#;

    /// Gitea 1.24's `/actions/runs`, in GitHub's shape.
    const GITEA_RUNS: &str = r#"{"total_count": 41, "workflow_runs": [
      {"id": 9001, "display_title": "Add dark mode", "path": "ci.yml@refs/pull/12/head", "event": "pull_request",
       "run_attempt": 1, "run_number": 140, "head_sha": "1111111111111111111111111111111111111111",
       "head_branch": "feature/dark", "status": "completed", "conclusion": "success",
       "started_at": "1970-01-01T01:00:00Z", "html_url": "https://gitea.acme.com/team/app/actions/runs/140"},
      {"id": 9002, "display_title": "v1.2", "path": "release.yaml@refs/tags/v1.2", "event": "push",
       "run_number": 139, "head_sha": "3333333333333333333333333333333333333333", "head_branch": "v1.2",
       "status": "in_progress", "conclusion": null}
    ]}"#;

    /// Forgejo's `/actions/runs`, in its own shape.
    const FORGEJO_RUNS: &str = r#"{"total_count": 2, "workflow_runs": [
      {"id": 77, "title": "Add dark mode", "workflow_id": "ci.yml", "index": 18, "prettyref": "feature/dark",
       "commit_sha": "1111111111111111111111111111111111111111", "event": "pull_request",
       "status": "failure", "created": "1970-01-01T01:00:00Z", "need_approval": false},
      {"id": 78, "title": "From a fork", "workflow_id": "ci.yml", "index": 19, "prettyref": "fork/x",
       "commit_sha": "4444444444444444444444444444444444444444", "event": "pull_request",
       "status": "blocked", "need_approval": true}
    ]}"#;

    /// The older `/actions/tasks`: one row per job.
    const TASKS: &str = r#"{"total_count": 3, "workflow_runs": [
      {"id": 5, "name": "test", "head_branch": "main", "head_sha": "5555555555555555555555555555555555555555",
       "run_number": 7, "event": "push", "display_title": "Fix", "status": "success", "workflow_id": "ci.yml",
       "created_at": "1970-01-01T02:00:00Z"},
      {"id": 6, "name": "lint", "head_branch": "main", "head_sha": "5555555555555555555555555555555555555555",
       "run_number": 7, "event": "push", "display_title": "Fix", "status": "failure", "workflow_id": "ci.yml"},
      {"id": 4, "name": "test", "head_branch": "main", "head_sha": "6666666666666666666666666666666666666666",
       "run_number": 6, "event": "push", "status": "running", "workflow_id": "ci.yml"}
    ]}"#;

    #[test]
    fn remotes_on_codeberg_and_named_servers_are_recognised() {
        for url in [
            "https://codeberg.org/team/app",
            "https://codeberg.org/team/app.git",
            "https://www.codeberg.org/team/app.git/",
            "git@codeberg.org:team/app.git",
            "ssh://git@codeberg.org/team/app.git",
            "ssh://git@codeberg.org:22/team/app.git",
            "https://ada:token@codeberg.org/team/app.git",
        ] {
            assert_eq!(parse_remote(url, &BTreeMap::new()), Some(codeberg()), "{url}");
        }
        let repo = codeberg();
        assert_eq!((repo.service, repo.host.as_str(), repo.slug().as_str()), (Service::Codeberg, "codeberg.org", "team/app"));
        assert!(!repo.self_hosted());
        assert_eq!(repo.site(), "Codeberg");
        assert_eq!(repo.api_base(), "https://codeberg.org/api/v1");
        for url in ["https://codeberg.org/team", "https://codeberg.org/team/app/pulls", "https://codeberg.org.evil.example/team/app"] {
            assert_eq!(parse_remote(url, &BTreeMap::new()), None, "{url}");
        }

        // Self-hosted: only once named, and as what it was named.
        let servers: Servers = [("git.acme.com".to_owned(), ServerKind::Gitea), ("forge.acme.com:3000".to_owned(), ServerKind::Forgejo)]
            .into_iter()
            .collect();
        assert_eq!(parse_remote("git@git.acme.com:team/app.git", &BTreeMap::new()), None);
        let gitea = parse_remote("git@git.acme.com:team/app.git", &servers).unwrap();
        assert_eq!((gitea.service, gitea.api_base().as_str()), (Service::Gitea, "https://git.acme.com/api/v1"));
        let forgejo = parse_remote("https://forge.acme.com:3000/team/app", &servers).unwrap();
        assert_eq!((forgejo.service, forgejo.host.as_str()), (Service::Forgejo, "forge.acme.com:3000"));
        assert!(gitea.self_hosted() && forgejo.self_hosted());
        assert_eq!(parse_remote("https://git.acme.com/team/sub/app", &servers), None);
    }

    #[test]
    fn links_follow_giteas_pages() {
        let repo = codeberg();
        assert_eq!(repo.pulls_url(), "https://codeberg.org/team/app/pulls");
        assert_eq!(repo.pull_url(12), "https://codeberg.org/team/app/pulls/12");
        assert_eq!(repo.issue_url(31), "https://codeberg.org/team/app/issues/31");
        assert_eq!(repo.runs_url(), "https://codeberg.org/team/app/actions");
        assert_eq!(repo.run_url(140), "https://codeberg.org/team/app/actions/runs/140");
        assert_eq!(repo.compare_url("main", "feature/dark mode"), "https://codeberg.org/team/app/compare/main...feature/dark%20mode");
        assert_eq!(Service::Codeberg.ci_title(), "ACTIONS");
        assert_eq!(Service::Forgejo.pull_number(12), "#12");
        assert_eq!(pull_branch(Service::Gitea, 12), "pr/12");
        assert_eq!(checkout_pull("origin", Service::Codeberg, 12, false).label, "Check out pull request #12");
    }

    #[test]
    fn pull_request_json_maps_to_the_model() {
        let pulls = parse_pulls(PULLS).unwrap();
        assert_eq!(pulls.iter().map(|pr| pr.number).collect::<Vec<_>>(), [12, 9, 8]);
        let first = &pulls[0];
        assert_eq!((first.title.as_str(), first.author.as_str(), first.draft), ("Add dark mode", "ada", false));
        assert_eq!((first.head_ref.as_str(), first.base_ref.as_str(), first.updated), ("feature/dark", "main", 86_400));
        assert!(first.is_from(&codeberg()));
        // A "WIP:" title is a draft; a deleted account is the ghost; a fork
        // is not this repository.
        assert!(pulls[1].draft && pulls[1].author == "Ghost" && !pulls[1].is_from(&codeberg()));
        assert!(pulls[2].draft && pulls[2].head_repo.is_none());
        assert!(parse_pulls(r#"{"message": "not found"}"#).is_err());
    }

    #[test]
    fn issues_leave_out_pull_requests() {
        let issues = parse_issues(ISSUES).unwrap();
        assert_eq!(issues.iter().map(|issue| issue.number).collect::<Vec<_>>(), [31, 30]);
        assert_eq!((issues[0].labels.clone(), issues[0].comments, issues[0].updated), (vec!["bug".to_owned()], 4, 60));
        assert_eq!(issues[1].author, "Ghost");
    }

    #[test]
    fn runs_are_read_in_every_shape_the_servers_send() {
        let (runs, more) = parse_runs(GITEA_RUNS, false).unwrap();
        assert!(more);
        assert_eq!((runs[0].id, runs[0].number, runs[0].workflow.as_str()), (140, 140, "ci"));
        assert_eq!((runs[0].state, runs[0].outcome.as_str(), runs[0].when), (RunState::Success, "success", 3600));
        assert_eq!((runs[0].event.as_str(), runs[0].title.as_str()), ("pull request", "Add dark mode"));
        assert_eq!((runs[1].workflow.as_str(), runs[1].state, runs[1].branch.as_str()), ("release", RunState::Running, "v1.2"));

        let (runs, more) = parse_runs(FORGEJO_RUNS, false).unwrap();
        assert!(!more);
        assert_eq!((runs[0].id, runs[0].workflow.as_str(), runs[0].branch.as_str()), (18, "ci", "feature/dark"));
        assert_eq!((runs[0].state, runs[0].head_sha.as_str()), (RunState::Failure, "1111111111111111111111111111111111111111"));
        assert_eq!(runs[1].state, RunState::NeedsAction);

        // Jobs of one run make one row, as bad as its worst job.
        let (runs, _) = parse_runs(TASKS, true).unwrap();
        assert_eq!(runs.iter().map(|run| (run.number, run.state)).collect::<Vec<_>>(), [(7, RunState::Failure), (6, RunState::Running)]);
        assert_eq!(runs[0].workflow, "test");

        for (status, expected) in [
            ("success", RunState::Success),
            ("failure", RunState::Failure),
            ("cancelled", RunState::Cancelled),
            ("skipped", RunState::Skipped),
            ("running", RunState::Running),
            ("waiting", RunState::Queued),
            ("blocked", RunState::NeedsAction),
            ("unknown", RunState::Queued),
        ] {
            assert_eq!(run_state(status, None), expected, "{status}");
        }
        assert_eq!(run_state("completed", Some("failure")), RunState::Failure);
        assert!(parse_runs("[]", false).is_err());

        // A pull request gets its checks from the runs on its head.
        let pulls = parse_pulls(PULLS).unwrap();
        let (runs, _) = parse_runs(GITEA_RUNS, false).unwrap();
        assert_eq!(checks_for(&pulls[0].head_sha, &runs), Some(Checks::Passing));
    }

    #[test]
    fn lists_are_asked_for_below_the_api_with_the_hosts_own_token() {
        let (mut client, sent) = scripted(
            codeberg(),
            vec![
                reply(200, &[("link", "<https://codeberg.org/api/v1/repos/team/app/pulls?page=2>; rel=\"next\"")], PULLS),
                reply(200, &[], ISSUES),
                reply(200, &[], GITEA_RUNS),
            ],
            with_token(),
        );
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, more: true } if items.len() == 3));
        assert!(matches!(client.issues(NOW), Outcome::Fresh { ref items, more: false } if items.len() == 2));
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, more: true } if items.len() == 2));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("CODEBERG_TOKEN")));
        assert_eq!(
            urls(&sent),
            [
                "https://codeberg.org/api/v1/repos/team/app/pulls?state=open&sort=recentupdate&limit=30",
                "https://codeberg.org/api/v1/repos/team/app/issues?state=open&type=issues&limit=30",
                "https://codeberg.org/api/v1/repos/team/app/actions/runs?limit=20",
            ]
        );
        assert!(sent.lock().unwrap().iter().all(|request| request.token.as_deref() == Some(SECRET)));
    }

    #[test]
    fn older_servers_list_runs_as_tasks_and_servers_without_actions_none() {
        // No /actions/runs: /actions/tasks, remembered.
        let (mut client, sent) = scripted(
            codeberg(),
            vec![reply(404, &[], r#"{"message": "not found"}"#), reply(200, &[], TASKS), reply(200, &[], TASKS)],
            with_token(),
        );
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
        assert!(matches!(client.runs(NOW), Outcome::Fresh { .. }));
        let asked = urls(&sent);
        assert!(asked[0].ends_with("/actions/runs?limit=20") && asked[1].ends_with("/actions/tasks?limit=30"));
        assert!(asked[2].ends_with("/actions/tasks?limit=30"));

        // Neither: an empty section, and no more asking until a refresh by hand.
        let (mut client, sent) =
            scripted(codeberg(), vec![reply(404, &[], "{}"), reply(404, &[], "{}"), reply(200, &[], GITEA_RUNS)], with_token());
        assert_eq!(client.runs(NOW), Outcome::Fresh { items: Vec::new(), more: false });
        assert_eq!(client.runs(NOW), Outcome::Fresh { items: Vec::new(), more: false });
        assert_eq!(sent.lock().unwrap().len(), 2);
        client.look_again();
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
    }

    #[test]
    fn statuses_map_to_problems() {
        // Private repositories look missing from outside.
        let (mut client, _) = scripted(codeberg(), vec![reply(404, &[], r#"{"message": "not found"}"#)], Fake::default());
        assert_eq!(failure(client.pulls(NOW)), Problem::NotSignedIn);
        assert!(Problem::NotSignedIn.detail(Service::Codeberg).contains("CODEBERG_TOKEN"));
        let (mut client, _) = scripted(codeberg(), vec![reply(404, &[], "{}"), reply(404, &[], "{}")], with_token());
        assert_eq!(failure(client.pulls(NOW)), Problem::NoAccess);
        // With a sign-in, a missing tracker is an empty list.
        assert_eq!(client.issues(NOW), Outcome::Fresh { items: Vec::new(), more: false });

        let scope = r#"{"message": "token does not have at least one of required scope(s): [read:repository]", "url": "https://codeberg.org/api/swagger"}"#;
        let (mut client, _) = scripted(codeberg(), vec![reply(403, &[], scope)], with_token());
        assert_eq!(
            failure(client.pulls(NOW)).line(Service::Codeberg),
            "Codeberg refused: token does not have at least one of required scope(s): [read:repository]"
        );

        // A refused token gives way to tea's, then to none.
        let secrets = Fake { env: vec![("CODEBERG_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() };
        let (mut client, sent) = scripted(codeberg(), vec![reply(401, &[], "{}"), reply(200, &[], "[]")], secrets.clone());
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Cli));
        assert_eq!(client.auth().describe(Service::Codeberg), "Signed in through tea's sign-in");
        assert_eq!(sent.lock().unwrap()[1].token.as_deref(), Some(OTHER));
        assert_eq!(secrets.asked(), ["FORGEJO_HOST", "CODEBERG_TOKEN", "FORGEJO_HOST", "CODEBERG_TOKEN", "tea codeberg.org"]);

        let (mut client, _) = scripted(codeberg(), vec![reply(429, &[("retry-after", "120")], ""), reply(200, &[], "[]")], with_token());
        assert_eq!(failure(client.pulls(NOW)), Problem::RateLimited { until: NOW + 120 });
        assert_eq!(failure(client.issues(NOW + 60)), Problem::RateLimited { until: NOW + 120 });
        let (mut client, _) = scripted(codeberg(), vec![reply(502, &[], "<html>"), reply(200, &[], "{ nope")], Fake::default());
        assert_eq!(failure(client.pulls(NOW)), Problem::Failed("Codeberg answered 502".to_owned()));
        assert!(matches!(failure(client.pulls(NOW)), Problem::Failed(text) if text.starts_with("Unreadable answer from Codeberg")));
    }

    #[test]
    fn host_variables_keep_tokens_to_the_server_they_name() {
        let gitea = RepoId { service: Service::Gitea, host: "git.acme.com".to_owned(), ..codeberg() };
        let forgejo = RepoId { service: Service::Forgejo, host: "forge.acme.com".to_owned(), ..codeberg() };
        let token = |env: Vec<(&'static str, &'static str)>, repo: &RepoId| {
            find_token(&Fake { env, ..Default::default() }, repo, &[]).map(|(token, source)| (token.0, source))
        };
        assert_eq!(token(vec![("GITEA_TOKEN", SECRET)], &gitea), None);
        assert_eq!(
            token(vec![("GITEA_HOST", "https://git.acme.com/"), ("GITEA_TOKEN", SECRET)], &gitea),
            Some((SECRET.to_owned(), TokenSource::Env("GITEA_TOKEN")))
        );
        assert_eq!(token(vec![("GITEA_HOST", "git.other.example"), ("GITEA_TOKEN", SECRET)], &gitea), None);
        assert_eq!(token(vec![("GITEA_HOST", "forge.acme.com"), ("GITEA_TOKEN", SECRET)], &forgejo), None);
        assert_eq!(token(vec![("FORGEJO_HOST", "forge.acme.com"), ("FORGEJO_TOKEN", SECRET)], &forgejo).unwrap().0, SECRET);
        // Codeberg's own variable is Codeberg's only.
        assert_eq!(token(vec![("CODEBERG_TOKEN", SECRET)], &forgejo), None);
        assert_eq!(token(vec![("CODEBERG_TOKEN", SECRET)], &codeberg()).unwrap().1, TokenSource::Env("CODEBERG_TOKEN"));
        assert_eq!(token(vec![("FORGEJO_HOST", "codeberg.org"), ("FORGEJO_TOKEN", OTHER)], &codeberg()).unwrap().0, OTHER);
        // GitHub's and GitLab's never go here.
        assert_eq!(token(vec![("GITHUB_TOKEN", SECRET), ("GITLAB_TOKEN", SECRET)], &codeberg()), None);
    }

    #[test]
    fn teas_configuration_gives_the_token_of_the_hosts_login_only() {
        let config = "\
logins:
- name: work
  url: https://git.acme.com
  token: 0123456789abcdef0123
  default: true
  user: ada
- name: codeberg.org
  url: \"https://codeberg.org/\"
  token: 'fedcba9876543210fedc'
  ssh_host: codeberg.org
-
  name: sub
  url: https://code.example.org/gitea
  token: aaaaaaaaaaaaaaaaaaaa
preferences:
  editor: false
  token: not-a-login-token-here
";
        assert_eq!(tea_login_token(config, "codeberg.org").as_deref(), Some("fedcba9876543210fedc"));
        assert_eq!(tea_login_token(config, "git.acme.com").as_deref(), Some("0123456789abcdef0123"));
        // Below a path is not the server itself; unknown hosts get nothing.
        assert_eq!(tea_login_token(config, "code.example.org"), None);
        assert_eq!(tea_login_token(config, "evil.example"), None);
        assert_eq!(tea_login_token("logins: []\n", "codeberg.org"), None);
        assert_eq!(tea_login_token("", "codeberg.org"), None);
        let indented = "logins:\n  - name: x\n    url: https://git.acme.com:3000\n    token: 1111222233334444\n";
        assert_eq!(tea_login_token(indented, "git.acme.com:3000").as_deref(), Some("1111222233334444"));
        assert_eq!(tea_login_token(indented, "git.acme.com"), None);
    }

    // ---- opening a pull request ----

    fn new_pull(draft: bool) -> NewPull {
        NewPull {
            head: "feature/fast".to_owned(),
            base: "main".to_owned(),
            title: "Speed up the graph".to_owned(),
            body: "Caches lane colours.".to_owned(),
            draft,
        }
    }

    const CREATED: &str = r#"{"number": 13, "title": "WIP: Speed up the graph", "user": {"login": "ada"},
       "head": {"ref": "feature/fast", "sha": "5555555555555555555555555555555555555555", "repo": {"full_name": "team/app"}},
       "base": {"ref": "main", "sha": "0000000000000000000000000000000000000000", "repo": {"full_name": "team/app"}},
       "updated_at": "1970-01-03T00:00:00Z"}"#;

    #[test]
    fn a_pull_request_is_opened_with_one_post_and_a_draft_title() {
        let (mut client, sent) = scripted(codeberg(), vec![reply(201, &[], CREATED)], with_token());
        let created = client.create_pull(&new_pull(true), NOW).unwrap();
        assert!(created.draft && created.number == 13 && created.is_from(&codeberg()));
        let sent = sent.lock().unwrap();
        assert_eq!((sent.len(), sent[0].url.as_str()), (1, "https://codeberg.org/api/v1/repos/team/app/pulls"));
        let body: serde_json::Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"head": "feature/fast", "base": "main", "title": "WIP: Speed up the graph", "body": "Caches lane colours."})
        );
        let title = |title: &str| {
            let body = new_pull_json(&NewPull { title: title.to_owned(), ..new_pull(true) });
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["title"].as_str().unwrap().to_owned()
        };
        assert_eq!(title("[WIP] Speed up"), "[WIP] Speed up");
        assert_eq!(title("Draft: Speed up"), "Draft: Speed up");
    }

    #[test]
    fn opening_a_pull_request_says_why_it_failed() {
        let (mut client, sent) = scripted(codeberg(), vec![reply(201, &[], CREATED)], Fake::default());
        assert_eq!(client.create_pull(&new_pull(false), NOW), Err(Problem::NotSignedIn));
        assert!(sent.lock().unwrap().is_empty());
        assert!(create_problem(&Problem::NotSignedIn, Service::Codeberg).contains("CODEBERG_TOKEN"));

        let exists = r#"{"message": "pull request already exists for these targets", "url": "https://codeberg.org/api/swagger"}"#;
        for (status, body, expected) in [
            (409, exists, "A pull request from feature/fast into main is already open"),
            (404, r#"{"message": "The target couldn't be found."}"#, "Codeberg does not have the branch feature/fast or main: push it first"),
            (422, r#"{"message": "There are no changes between the head and the base"}"#, "No commits between main and feature/fast: there is nothing to merge"),
            (422, r#"{"message": "[Title]: Required"}"#, "[Title]: Required"),
        ] {
            let (mut client, _) = scripted(codeberg(), vec![reply(status, &[], body)], with_token());
            assert_eq!(client.create_pull(&new_pull(false), NOW), Err(Problem::Refused(expected.to_owned())), "{status}");
        }
        let echo = format!(r#"{{"message": "bad token {SECRET}"}}"#);
        let (mut client, _) = scripted(codeberg(), vec![reply(403, &[], &echo)], with_token());
        assert!(!format!("{:?}", client.create_pull(&new_pull(false), NOW)).contains(SECRET));
        let (mut client, _) = scripted(codeberg(), vec![reply(500, &[], "")], with_token());
        assert_eq!(client.create_pull(&new_pull(false), NOW), Err(Problem::Failed("Codeberg answered 500".to_owned())));
    }

    // ---- review states, the parent, pull requests across forks ----

    /// `GET /repos/{owner}/{repo}/pulls/{index}/reviews`, after the API's
    /// PullReview: a request for a review, a dismissed approval, a stale but
    /// standing one, and changes asked for then approved.
    const REVIEWS: &str = r#"[
      {"id": 31, "user": {"id": 2, "login": "grace"}, "team": null, "state": "REQUEST_REVIEW", "body": "",
       "commit_id": "", "stale": false, "official": false, "dismissed": false, "comments_count": 0,
       "submitted_at": "1970-01-02T00:00:00Z", "updated_at": "1970-01-02T00:00:00Z"},
      {"id": 34, "user": {"id": 3, "login": "linus"}, "state": "APPROVED", "body": "lgtm",
       "commit_id": "0000000000000000000000000000000000000000", "stale": true, "official": true, "dismissed": true,
       "submitted_at": "1970-01-02T01:00:00Z"},
      {"id": 33, "user": {"id": 4, "login": "ada"}, "state": "APPROVED", "commit_id": "0000000000000000000000000000000000000000",
       "stale": true, "official": true, "dismissed": false, "submitted_at": "1970-01-02T00:30:00Z"},
      {"id": 32, "user": {"id": 2, "login": "grace"}, "state": "REQUEST_CHANGES", "official": true, "dismissed": false},
      {"id": 35, "user": {"id": 2, "login": "grace"}, "state": "APPROVED", "official": true, "dismissed": false},
      {"id": 36, "user": {"id": 2, "login": "grace"}, "state": "COMMENT", "official": true, "dismissed": false}
    ]"#;

    /// `GET /repos/{owner}/{repo}` of a fork, trimmed: `parent` is a whole
    /// repository.
    const FORK: &str = r#"{"id": 70, "full_name": "ada/app", "name": "app", "fork": true, "default_branch": "main",
      "owner": {"login": "ada"},
      "parent": {"id": 7, "full_name": "team/app", "name": "app", "fork": false, "default_branch": "trunk",
                 "owner": {"login": "team"}, "parent": null}}"#;

    #[test]
    fn reviews_come_to_each_official_reviewers_last_word() {
        let given = |n| Approvals { given: n, left: 0 };
        // grace approved after asking for changes; ada's stale approval
        // stands; linus's was dismissed; the review request is no word.
        assert_eq!(parse_reviews(REVIEWS).unwrap(), (Review::Approved, given(2)));
        let changes = r#"[{"id": 1, "user": {"login": "grace"}, "state": "APPROVED", "official": true},
            {"id": 2, "user": {"login": "grace"}, "state": "REQUEST_CHANGES", "official": true},
            {"id": 3, "user": {"login": "ada"}, "state": "APPROVED", "official": true}]"#;
        assert_eq!(parse_reviews(changes).unwrap(), (Review::ChangesRequested, given(1)));
        // Someone the rules do not count; a pending review; a deleted account.
        let none = r#"[{"id": 1, "user": {"login": "eve"}, "state": "APPROVED", "official": false},
            {"id": 2, "user": {"login": "ada"}, "state": "PENDING", "official": true},
            {"id": 3, "user": null, "state": "REQUEST_CHANGES", "official": true}]"#;
        assert_eq!(parse_reviews(none).unwrap(), (Review::Unreviewed, given(0)));
        // A server too old to say which reviews are official.
        let older = r#"[{"id": 1, "user": {"login": "ada"}, "state": "APPROVED"}]"#;
        assert_eq!(parse_reviews(older).unwrap(), (Review::Approved, given(1)));
        assert_eq!(parse_reviews("[]").unwrap(), (Review::Unreviewed, given(0)));
        assert!(parse_reviews(r#"{"message": "not found"}"#).is_err());
    }

    #[test]
    fn reviews_are_asked_for_the_pull_requests_that_changed_and_kept_between_rounds() {
        let second = PULLS.replace("\"updated_at\": \"1970-01-02T00:00:00Z\"", "\"updated_at\": \"1970-01-04T00:00:00Z\"");
        let (mut client, sent) = scripted(
            codeberg(),
            vec![
                reply(200, &[], PULLS),
                reply(200, &[], FORK),
                reply(200, &[], REVIEWS),
                reply(404, &[], r#"{"message": "not found"}"#),
                reply(200, &[], "[]"),
                reply(200, &[], &second),
                reply(200, &[], r#"[{"id": 9, "user": {"login": "grace"}, "state": "REQUEST_CHANGES", "official": true}]"#),
            ],
            with_token(),
        );
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        let details = client.details(NOW).unwrap();
        let parent = details.parent.unwrap();
        assert_eq!((parent.repo.slug().as_str(), parent.default_branch.as_str()), ("team/app", "trunk"));
        // #12 approved, #9 not found (unknown: nothing to say), #8 unreviewed.
        let states: Vec<(u64, Review)> = details.pulls.iter().map(|pull| (pull.number, pull.review)).collect();
        assert_eq!(states, [(12, Review::Approved), (8, Review::Unreviewed)]);
        assert_eq!(details.pulls[0].approvals, Some(Approvals { given: 2, left: 0 }));
        // Nothing changed: nothing asked.
        assert_eq!(client.details(NOW), None);
        // #12 was updated: the list keeps what is known, #12 alone is asked.
        let Outcome::Fresh { items, .. } = client.pulls(NOW) else { panic!("expected a list") };
        assert_eq!(items.iter().map(|pull| pull.review).collect::<Vec<_>>(), [Review::Approved, Review::Unknown, Review::Unreviewed]);
        let details = client.details(NOW).unwrap();
        assert_eq!(details.pulls.iter().map(|pull| (pull.number, pull.review)).collect::<Vec<_>>(), [(12, Review::ChangesRequested)]);

        let asked = urls(&sent);
        assert_eq!(
            asked[1..5],
            [
                "https://codeberg.org/api/v1/repos/team/app",
                "https://codeberg.org/api/v1/repos/team/app/pulls/12/reviews?limit=50",
                "https://codeberg.org/api/v1/repos/team/app/pulls/9/reviews?limit=50",
                "https://codeberg.org/api/v1/repos/team/app/pulls/8/reviews?limit=50",
            ]
        );
        assert_eq!(asked.len(), 7);
        assert!(sent.lock().unwrap().iter().all(|request| request.token.as_deref() == Some(SECRET)));
    }

    #[test]
    fn a_failing_server_or_a_rate_limit_stops_the_reviews_for_the_round() {
        let (mut client, sent) = scripted(
            codeberg(),
            vec![
                reply(200, &[], PULLS),
                reply(200, &[], r#"{"full_name": "team/app", "fork": false, "parent": null}"#),
                reply(502, &[], "<html>"),
                reply(429, &[("retry-after", "600")], ""),
            ],
            Fake::default(),
        );
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        // Not a fork; the first review request failed: nothing learnt, and
        // the others wait for the next round.
        assert_eq!(client.details(NOW), None);
        assert_eq!(urls(&sent).len(), 3);
        // Next round: refused for asking too often, and then nothing at all
        // until the limit lifts, lists included.
        assert_eq!(client.details(NOW), None);
        assert_eq!(client.details(NOW + 60), None);
        assert_eq!(urls(&sent).len(), 4);
        assert_eq!(client.blocked_until(NOW + 60), Some(NOW + 600));
        assert!(sent.lock().unwrap().iter().all(|request| request.token.is_none()));
    }

    #[test]
    fn a_pull_request_into_the_parent_names_the_forks_branch_owner_colon_branch() {
        let fork = RepoId { owner: "ada".to_owned(), ..codeberg() };
        let (mut client, sent) = scripted(fork.clone(), vec![reply(201, &[], CREATED)], with_token());
        let across = Across { from: fork.clone(), into: codeberg() };
        assert_eq!(client.create_pull_across(&new_pull(false), &across, NOW).unwrap().number, 13);
        let sent_now = sent.lock().unwrap();
        assert_eq!(sent_now[0].url, "https://codeberg.org/api/v1/repos/team/app/pulls");
        let body: serde_json::Value = serde_json::from_str(sent_now[0].body.as_deref().unwrap()).unwrap();
        assert_eq!((body["head"].as_str(), body["base"].as_str()), (Some("ada:feature/fast"), Some("main")));
        drop(sent_now);

        // Refusals name the repository it went into.
        let (mut client, _) = scripted(fork.clone(), vec![reply(403, &[], "{}")], with_token());
        let refused = client.create_pull_across(&new_pull(false), &across, NOW).unwrap_err();
        assert_eq!(refused, Problem::Refused("No permission to open pull requests in team/app".to_owned()));
        // Never to another server, and nothing is sent then.
        let elsewhere = RepoId { service: Service::Forgejo, host: "forge.acme.com".to_owned(), ..codeberg() };
        let (mut client, sent) = scripted(fork.clone(), vec![reply(201, &[], CREATED)], with_token());
        assert!(client.create_pull_across(&new_pull(false), &Across { from: fork, into: elsewhere }, NOW).is_err());
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn the_offline_fixture_answers_reviews_and_the_repository() {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-canned-gitea-reviews-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pulls.json"), PULLS).unwrap();
        std::fs::write(dir.join("repo.json"), FORK).unwrap();
        std::fs::write(dir.join("reviews-12.json"), REVIEWS).unwrap();
        let mut client = Client::new(codeberg(), Box::new(Canned { dir: dir.clone(), created: false }), Box::new(NoSecrets));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        let details = client.details(NOW).unwrap();
        assert_eq!(details.parent.unwrap().repo.slug(), "team/app");
        assert_eq!(details.pulls.iter().map(|pull| (pull.number, pull.review)).collect::<Vec<_>>(), [(12, Review::Approved)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_offline_fixture_serves_gitea_endpoints() {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-canned-gitea-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pulls.json"), PULLS).unwrap();
        std::fs::write(dir.join("tasks.json"), TASKS).unwrap();
        let mut client = Client::new(codeberg(), Box::new(Canned { dir: dir.clone(), created: false }), Box::new(NoSecrets));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, .. } if items.len() == 3));
        // No runs.json: the older endpoint answers.
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
