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

fn pull_request(pull: RawPull) -> PullRequest {
    PullRequest {
        number: pull.number,
        title: pull.title,
        author: login(pull.user),
        draft: pull.draft,
        head_ref: pull.head.name,
        head_sha: pull.head.sha,
        head_repo: pull.head.repo.and_then(|repo| repo.full_name),
        base_ref: pull.base.name,
        updated: timestamp(pull.updated_at.as_deref()),
        // The list does not say; `Client::details` asks.
        ..Default::default()
    }
}

pub fn parse_pulls(json: &str) -> Result<Vec<PullRequest>, String> {
    let raw: Vec<RawPull> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(raw.into_iter().map(pull_request).collect())
}

/// One pull request, as the API answers a creation.
pub fn parse_pull(json: &str) -> Result<PullRequest, String> {
    serde_json::from_str::<RawPull>(json).map(pull_request).map_err(|e| e.to_string())
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

// ---- opening a pull request ----

/// The JSON sent to open a pull request.
pub fn new_pull_json(new: &NewPull) -> String {
    pull_json(new, None)
}

/// [`new_pull_json`], naming the repository the head branch is in when it
/// is a fork owned by the same account as the base repository: `owner:branch`
/// does not tell them apart then.
fn pull_json(new: &NewPull, head_repo: Option<&str>) -> String {
    let mut json = serde_json::json!({
        "title": new.title,
        "head": new.head,
        "base": new.base,
        "body": new.body,
        "draft": new.draft,
    });
    if let Some(name) = head_repo {
        json["head_repo"] = serde_json::Value::from(name);
    }
    json.to_string()
}

// ---- review states and the parent ----

/// Where GraphQL is asked for an API at `api`: below github.com's (and
/// `*.ghe.com`'s), beside an Enterprise server's `/api/v3` ([`beside`]).
pub fn graphql_url(api: &str) -> String {
    beside(api).unwrap_or_else(|| format!("{api}/graphql"))
}

/// Review states asked at most per request; as many as a list holds.
const REVIEWS_AT_ONCE: usize = 30;

/// Without a sign-in, the pull requests whose reviews are asked at most per
/// round (one request each, see `Client::ask_public_reviews`)...
const PUBLIC_REVIEWS_AT_ONCE: usize = 5;
/// ...and the requests that must be left in the hour to ask any: the lists
/// take three a round, six rounds an hour.
const PUBLIC_RESERVE: u64 = 20;

/// The GraphQL request for how the pull requests `numbers` of `repo` stand
/// with their reviewers: GitHub's review decision (set when the branch's
/// rules require reviews) and the latest approval or request for changes of
/// each reviewer with write access. The numbers are digits and the names go
/// in as variables, so nothing but them comes from outside the program.
pub fn reviews_query(repo: &RepoId, numbers: &[u64]) -> String {
    let fields: String = numbers
        .iter()
        .map(|number| {
            format!(
                "p{number}: pullRequest(number: {number}) {{ number reviewDecision                  latestOpinionatedReviews(first: 30, writersOnly: true) {{ nodes {{ state }} }} }} "
            )
        })
        .collect();
    let query =
        format!("query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {fields}}} }}");
    serde_json::json!({ "query": query, "variables": { "owner": repo.owner, "name": repo.name } }).to_string()
}

/// What a review decision and the latest reviews' states come to. Without a
/// decision (no rule requires reviews) the reviewers' word counts: any
/// request for changes, else any approval.
pub fn review_of(decision: Option<&str>, states: &[String]) -> (Review, Approvals) {
    let given = states.iter().filter(|state| *state == "APPROVED").count() as u32;
    let review = match decision {
        Some("APPROVED") => Review::Approved,
        Some("CHANGES_REQUESTED") => Review::ChangesRequested,
        Some("REVIEW_REQUIRED") => Review::Required,
        _ if states.iter().any(|state| state == "CHANGES_REQUESTED") => Review::ChangesRequested,
        _ if given > 0 => Review::Approved,
        _ => Review::Unreviewed,
    };
    (review, Approvals { given, left: 0 })
}

/// The review states in a GraphQL answer, by pull request number. A pull
/// request the answer has no data for (closed, gone) is left out. `Err`
/// when the answer has no data at all (only errors) or cannot be read.
pub fn parse_reviews(json: &str) -> Result<Vec<(u64, Review, Approvals)>, String> {
    #[derive(Deserialize)]
    struct RawState {
        #[serde(default)]
        state: String,
    }
    #[derive(Deserialize, Default)]
    struct RawNodes {
        #[serde(default)]
        nodes: Vec<Option<RawState>>,
    }
    #[derive(Deserialize)]
    struct RawReviewed {
        number: u64,
        #[serde(rename = "reviewDecision")]
        decision: Option<String>,
        #[serde(rename = "latestOpinionatedReviews")]
        latest: Option<RawNodes>,
    }
    #[derive(Deserialize)]
    struct RawData {
        repository: Option<BTreeMap<String, Option<RawReviewed>>>,
    }
    #[derive(Deserialize)]
    struct RawAnswer {
        data: Option<RawData>,
    }
    let answer: RawAnswer = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let pulls = answer.data.and_then(|data| data.repository).ok_or("no repository in the answer")?;
    Ok(pulls
        .into_values()
        .flatten()
        .map(|pull| {
            let states: Vec<String> =
                pull.latest.unwrap_or_default().nodes.into_iter().flatten().map(|node| node.state).collect();
            let (review, approvals) = review_of(pull.decision.as_deref(), &states);
            (pull.number, review, approvals)
        })
        .collect())
}

/// What one pull request's reviews (`GET /repos/{owner}/{repo}/pulls/
/// {number}/reviews`, oldest first) come to without the review decision,
/// which only GraphQL gives: the latest approval or request for changes of
/// each reviewer with write access (owner, member or collaborator, as
/// `writersOnly` counts them in GraphQL), unless it was dismissed since;
/// comments leave a reviewer's word as it was. Reviews by deleted accounts
/// are left out.
pub fn parse_public_reviews(json: &str) -> Result<(Review, Approvals), String> {
    #[derive(Deserialize)]
    struct RawReview {
        user: Option<RawUser>,
        #[serde(default)]
        state: String,
        #[serde(default)]
        author_association: String,
    }
    let raw: Vec<RawReview> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    // Each reviewer's last word, in the order they first spoke.
    let mut words: Vec<(String, String)> = Vec::new();
    for review in raw {
        let Some(login) = review.user.map(|user| user.login).filter(|login| !login.is_empty()) else { continue };
        let writer = matches!(review.author_association.as_str(), "OWNER" | "MEMBER" | "COLLABORATOR");
        if !writer || !matches!(review.state.as_str(), "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            continue;
        }
        match words.iter_mut().find(|(who, _)| *who == login) {
            Some((_, word)) => *word = review.state,
            None => words.push((login, review.state)),
        }
    }
    let states: Vec<String> = words.into_iter().map(|(_, word)| word).filter(|word| word != "DISMISSED").collect();
    Ok(review_of(None, &states))
}

/// The parent of `repo` in the answer to `GET /repos/{owner}/{repo}`:
/// `None` when it is not a fork, or its parent's name is not one a remote
/// could have.
pub fn parse_parent(json: &str, repo: &RepoId) -> Result<Option<Parent>, String> {
    #[derive(Deserialize)]
    struct RawParent {
        full_name: String,
        #[serde(default)]
        default_branch: String,
    }
    #[derive(Deserialize)]
    struct RawRepository {
        parent: Option<RawParent>,
    }
    let raw: RawRepository = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(raw.parent.and_then(|parent| {
        let default_branch = branch_name(&parent.default_branch);
        Some(Parent { repo: repo.sibling(&parent.full_name)?, default_branch })
    }))
}

/// What a 422 ("Validation Failed") answer to a creation means, in words.
/// GitHub puts the reasons in `errors`, each with a `message` or a
/// `field` and `code`.
pub fn validation_problem(body: &str, new: &NewPull) -> String {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct RawReason {
        message: String,
        field: String,
        code: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct RawFailure {
        message: String,
        errors: Vec<RawReason>,
    }
    let failure: RawFailure = serde_json::from_str(body).unwrap_or_default();
    let (head, base) = (&new.head, &new.base);
    let reasons: Vec<&RawReason> = failure.errors.iter().collect();
    let says = |needle: &str| {
        let needle = needle.to_ascii_lowercase();
        failure.message.to_ascii_lowercase().contains(&needle)
            || reasons.iter().any(|reason| reason.message.to_ascii_lowercase().contains(&needle))
    };
    let field = |name: &str| reasons.iter().any(|reason| reason.field == name);

    if says("already exists") {
        format!("A pull request from {head} into {base} is already open")
    } else if says("no commits between") {
        format!("No commits between {base} and {head}: there is nothing to merge")
    } else if says("draft") {
        "Draft pull requests are not available in this repository: untick Draft".to_owned()
    } else if field("head") {
        format!("GitHub does not have the branch {head}: push it first")
    } else if field("base") {
        format!("GitHub does not have the branch {base} to merge into")
    } else if field("title") {
        "The title is missing".to_owned()
    } else {
        // Whatever it said, as it said it.
        let details: Vec<String> = reasons
            .iter()
            .map(|reason| match (reason.message.is_empty(), reason.field.is_empty()) {
                (false, _) => reason.message.clone(),
                (true, false) => format!("{} is {}", reason.field, reason.code.replace('_', " ")),
                (true, true) => reason.code.replace('_', " "),
            })
            .filter(|text| !text.is_empty())
            .collect();
        match (failure.message.is_empty(), details.is_empty()) {
            (false, false) => format!("{} ({})", failure.message, details.join("; ")),
            (true, false) => details.join("; "),
            (false, true) => failure.message,
            (true, true) => "Validation failed".to_owned(),
        }
    }
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
    /// Review states of the listed pull requests, as last asked.
    reviews: Reviews,
    /// The repository's parent once asked; `Some(None)` when it has none.
    parent: Option<Option<Parent>>,
    /// GraphQL has a budget of its own.
    graphql: Limiter,
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
            reviews: Reviews::default(),
            parent: None,
            graphql: Limiter::default(),
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

    /// POST /repos/{owner}/{repo}/pulls of `into` (this repository, or
    /// another on the same server) with `body`. Always with a token:
    /// opening a pull request anonymously is impossible, so nothing is sent
    /// without one. Never repeated after a request that may have reached
    /// GitHub, except after a 401, which means nothing was done.
    fn open_pull(&mut self, into: &RepoId, new: &NewPull, body: &str, now: u64) -> Result<PullRequest, Problem> {
        let mut url = format!("{}/repos/{}/{}/pulls", self.api, into.owner, into.name);
        let mut hops = 0;
        // Not signed in so far: the user may have signed in since.
        self.look_again();
        loop {
            if !self.looked {
                let found = find_token(self.secrets.as_ref(), &self.repo, &self.rejected);
                self.sign_in(found);
            }
            let Some((token, _)) = &self.token else {
                return Err(if self.rejected.is_empty() { Problem::NotSignedIn } else { Problem::SignInRejected });
            };
            if let Some(until) = self.limiter.blocked(now) {
                return Err(Problem::RateLimited { until });
            }
            let request = HttpRequest { url: &url, etag: None, token: Some(token) };
            let response = self.transport.post(&request, body).map_err(Problem::Unreachable)?;
            let message = api_message(&response.body);
            let rate = Rate::of(&response);
            if let Some(until) = self.limiter.record(response.status, &rate, &message, now) {
                return Err(Problem::RateLimited { until });
            }

            return match response.status {
                200 | 201 => parse_pull(&response.body)
                    .map_err(|error| Problem::Failed(format!("Unreadable answer from GitHub ({error})"))),
                // Only these two keep the method and the body; anything else
                // would turn the request into a GET.
                307 | 308 => match response.header("location") {
                    Some(location) if within(&self.api, location) && hops < 3 => {
                        url = location.to_owned();
                        hops += 1;
                        continue;
                    }
                    _ => Err(Problem::Failed("GitHub sent an unexpected redirect".to_owned())),
                },
                300..=399 => Err(Problem::Failed("GitHub sent an unexpected redirect".to_owned())),
                401 => {
                    if let Some((token, _)) = self.token.take() {
                        self.rejected.push(token);
                    }
                    let found = find_token(self.secrets.as_ref(), &self.repo, &self.rejected);
                    self.sign_in(found);
                    continue;
                }
                404 => Err(Problem::NoAccess),
                403 => Err(Problem::Refused(match message.is_empty() {
                    true => format!("No permission to open pull requests in {}", into.slug()),
                    false => format!("No permission to open pull requests in {} ({message})", into.slug()),
                })),
                422 => Err(Problem::Refused(validation_problem(&response.body, new))),
                status => Err(Problem::Failed(format!("GitHub answered {status}"))),
            };
        }
    }
}

impl Client {
    /// Looks for a token now if there is reason to.
    fn look(&mut self) {
        if !self.looked {
            let found = find_token(self.secrets.as_ref(), &self.repo, &self.rejected);
            self.sign_in(found);
        }
    }

    /// GET /repos/{owner}/{repo}, for the parent of a fork. `None` when
    /// there was no answer worth keeping (no connection, a rate limit, the
    /// server failing): asked again next round.
    fn ask_parent(&mut self, now: u64) -> Option<Option<Parent>> {
        self.look();
        if self.limiter.blocked(now).is_some() {
            return None;
        }
        let url = format!("{}/repos/{}/{}", self.api, self.repo.owner, self.repo.name);
        let request = HttpRequest { url: &url, etag: None, token: self.token.as_ref().map(|(token, _)| token) };
        let response = self.transport.get(&request).ok()?;
        let message = api_message(&response.body);
        if self.limiter.record(response.status, &Rate::of(&response), &message, now).is_some() {
            return None;
        }
        match response.status {
            200 => Some(parse_parent(&response.body, &self.repo).unwrap_or(None)),
            500..=599 => None,
            // Moved, hidden or refused: no parent to offer.
            _ => Some(None),
        }
    }

    /// Asks how the listed pull requests that changed since they were last
    /// asked about stand with their reviewers. Signed in: GraphQL, one
    /// request for all of them, and none at all while nothing changed.
    /// Without a sign-in, GraphQL is closed: each pull request's reviews
    /// instead ([`Client::ask_public_reviews`]). Returns those whose state is
    /// not what it was.
    fn ask_reviews(&mut self, now: u64) -> Vec<PullRequest> {
        let Some((token, _)) = &self.token else { return self.ask_public_reviews(now) };
        if self.graphql.blocked(now).is_some() || self.limiter.blocked(now).is_some() {
            return Vec::new();
        }
        let stale = self.reviews.stale(REVIEWS_AT_ONCE);
        if stale.is_empty() {
            return Vec::new();
        }
        let numbers: Vec<u64> = stale.iter().map(|(number, _)| *number).collect();
        let url = graphql_url(&self.api);
        let request = HttpRequest { url: &url, etag: None, token: Some(token) };
        let Ok(response) = self.transport.post(&request, &reviews_query(&self.repo, &numbers)) else {
            return Vec::new();
        };
        let message = api_message(&response.body);
        if self.graphql.record(response.status, &Rate::of(&response), &message, now).is_some() {
            return Vec::new();
        }
        // Anything but an answer leaves them unknown, and not asked again
        // until they change or the user refreshes.
        let found = match response.status {
            200 => parse_reviews(&response.body).unwrap_or_default(),
            _ => Vec::new(),
        };
        let mut changed = Vec::new();
        for (number, updated) in stale {
            let (review, approvals) = match found.iter().find(|(known, ..)| *known == number) {
                Some((_, review, approvals)) => (*review, Some(*approvals)),
                None => (Review::Unknown, None),
            };
            changed.extend(self.reviews.learnt(number, updated, review, approvals));
        }
        changed
    }

    /// Review states without a sign-in: `GET .../pulls/{number}/reviews`
    /// of each pull request that changed, which public repositories answer
    /// to anyone. Each is one of the 60 requests an hour GitHub allows
    /// without a sign-in, shared with the lists, so at most
    /// [`PUBLIC_REVIEWS_AT_ONCE`] are asked per round and none while fewer
    /// than [`PUBLIC_RESERVE`] are left in the hour. What is learnt is kept
    /// until the pull request changes, like GraphQL's answers. The review
    /// decision (what the branch's rules require) is not in this answer.
    fn ask_public_reviews(&mut self, now: u64) -> Vec<PullRequest> {
        let mut changed = Vec::new();
        for (number, updated) in self.reviews.stale(PUBLIC_REVIEWS_AT_ONCE) {
            let spent = self.limiter.left(now).is_some_and(|left| left < PUBLIC_RESERVE);
            if spent || self.limiter.blocked(now).is_some() {
                break;
            }
            let (owner, name) = (&self.repo.owner, &self.repo.name);
            let url = format!("{}/repos/{owner}/{name}/pulls/{number}/reviews?per_page=100", self.api);
            let request = HttpRequest { url: &url, etag: None, token: None };
            let Ok(response) = self.transport.get(&request) else { break };
            let message = api_message(&response.body);
            if self.limiter.record(response.status, &Rate::of(&response), &message, now).is_some() {
                break;
            }
            let (review, approvals) = match response.status {
                200 => match parse_public_reviews(&response.body) {
                    Ok((review, approvals)) => (review, Some(approvals)),
                    Err(_) => (Review::Unknown, None),
                },
                // Asked again next round.
                500..=599 => break,
                // Hidden or gone: unknown until the pull request changes.
                _ => (Review::Unknown, None),
            };
            changed.extend(self.reviews.learnt(number, updated, review, approvals));
        }
        changed
    }

    /// `problem` with every token this client has held blanked out.
    fn scrub(&self, problem: Problem) -> Problem {
        let mut tokens: Vec<&Token> = self.rejected.iter().collect();
        tokens.extend(self.token.as_ref().map(|(token, _)| token));
        problem.scrubbed(&tokens)
    }
}

impl Backend for Client {
    fn pulls(&mut self, now: u64) -> Outcome<PullRequest> {
        match self.fetch(List::Pulls, now, |json| Ok((parse_pulls(json)?, false))) {
            Outcome::Fresh { mut items, more } => {
                self.reviews.listed(&mut items);
                Outcome::Fresh { items, more }
            }
            other => other,
        }
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
        // Reviews are asked about again, and shown as they were meanwhile.
        self.reviews.ask_again();
    }

    fn blocked_until(&self, now: u64) -> Option<u64> {
        self.limiter.blocked(now)
    }

    fn create_pull(&mut self, new: &NewPull, now: u64) -> Result<PullRequest, Problem> {
        let into = self.repo.clone();
        let opened = self.open_pull(&into, new, &new_pull_json(new), now);
        opened.map_err(|problem| self.scrub(problem))
    }

    fn create_pull_across(&mut self, new: &NewPull, across: &Across, now: u64) -> Result<PullRequest, Problem> {
        let here = |repo: &RepoId| repo.service == self.repo.service && repo.host == self.repo.host;
        if !here(&across.from) || !here(&across.into) {
            return Err(Problem::Failed(format!("Both repositories must be on {}", self.repo.site())));
        }
        // A branch of another repository of the network is `owner:branch`;
        // a fork with the same owner as the base needs its name as well.
        let (head, head_repo) = match across.from.same(&across.into) {
            true => (new.head.clone(), None),
            false => {
                let same_owner = across.from.owner.eq_ignore_ascii_case(&across.into.owner);
                (format!("{}:{}", across.from.owner, new.head), same_owner.then_some(across.from.name.as_str()))
            }
        };
        let new = NewPull { head, ..new.clone() };
        let opened = self.open_pull(&across.into, &new, &pull_json(&new, head_repo), now);
        opened.map_err(|problem| self.scrub(problem))
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
    use super::super::tests::{Fake, Log, NOW, SECRET, Script, reply};
    use super::*;

    fn repo() -> RepoId {
        RepoId { service: Service::GitHub, host: "github.com".to_owned(), owner: "ada".to_owned(), name: "app".to_owned() }
    }

    fn scripted(replies: Vec<Result<HttpResponse, String>>, secrets: Fake) -> (Client, Log) {
        let sent = Log::default();
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        (Client::new(repo(), Box::new(transport), Box::new(secrets)), sent)
    }

    fn with_token() -> Fake {
        Fake { env: vec![("GITHUB_TOKEN", SECRET)], ..Default::default() }
    }

    fn pull(number: u64, updated: &str) -> String {
        format!(
            r#"{{"number": {number}, "title": "Pull {number}", "user": {{"login": "ada"}},
               "head": {{"ref": "feature-{number}", "sha": "1111111111111111111111111111111111111111",
                        "repo": {{"full_name": "grace/app"}}}},
               "base": {{"ref": "main", "sha": "0000000000000000000000000000000000000000"}},
               "updated_at": "{updated}"}}"#
        )
    }

    fn list(pulls: &[String]) -> String {
        format!("[{}]", pulls.join(","))
    }

    const REVIEWS: &str = r#"{"data": {"repository": {
      "p12": {"number": 12, "reviewDecision": "APPROVED",
              "latestOpinionatedReviews": {"nodes": [{"state": "APPROVED"}, {"state": "APPROVED"}]}},
      "p9": {"number": 9, "reviewDecision": null,
             "latestOpinionatedReviews": {"nodes": [{"state": "APPROVED"}, {"state": "CHANGES_REQUESTED"}]}}
    }}}"#;

    const FORK: &str = r#"{"id": 5, "full_name": "ada/app", "fork": true,
      "parent": {"full_name": "octo/app", "default_branch": "trunk"},
      "source": {"full_name": "octo/app", "default_branch": "trunk"}}"#;

    fn fresh(outcome: Outcome<PullRequest>) -> Vec<PullRequest> {
        match outcome {
            Outcome::Fresh { items, .. } => items,
            other => panic!("expected a list, got {other:?}"),
        }
    }

    #[test]
    fn review_decisions_and_latest_reviews_come_to_one_state() {
        let states = |list: &[&str]| list.iter().map(|state| (*state).to_owned()).collect::<Vec<_>>();
        let given = |n| Approvals { given: n, left: 0 };
        for (decision, latest, expected) in [
            (Some("APPROVED"), states(&["APPROVED"]), (Review::Approved, given(1))),
            (Some("CHANGES_REQUESTED"), states(&["CHANGES_REQUESTED", "APPROVED"]), (Review::ChangesRequested, given(1))),
            (Some("REVIEW_REQUIRED"), states(&["APPROVED"]), (Review::Required, given(1))),
            (Some("REVIEW_REQUIRED"), states(&[]), (Review::Required, given(0))),
            // No rule asks for reviews: the reviewers' word counts.
            (None, states(&["APPROVED", "APPROVED"]), (Review::Approved, given(2))),
            (None, states(&["APPROVED", "CHANGES_REQUESTED"]), (Review::ChangesRequested, given(1))),
            (None, states(&["DISMISSED"]), (Review::Unreviewed, given(0))),
            (None, states(&[]), (Review::Unreviewed, given(0))),
            (Some("SOMETHING_NEW"), states(&[]), (Review::Unreviewed, given(0))),
        ] {
            assert_eq!(review_of(decision, &latest), expected, "{decision:?} {latest:?}");
        }

        let found = parse_reviews(REVIEWS).unwrap();
        assert!(found.contains(&(12, Review::Approved, given(2))));
        assert!(found.contains(&(9, Review::ChangesRequested, given(1))));
        // A pull request GitHub has no data for is left out.
        let partial = r#"{"data": {"repository": {"p3": null}}, "errors": [{"type": "NOT_FOUND"}]}"#;
        assert_eq!(parse_reviews(partial).unwrap(), []);
        assert!(parse_reviews(r#"{"errors": [{"message": "Bad credentials"}]}"#).is_err());
        assert!(parse_reviews(r#"{"data": {"repository": null}}"#).is_err());
        assert!(parse_reviews("<html>").is_err());
    }

    #[test]
    fn the_review_query_names_numbers_only_and_passes_names_as_variables() {
        let body: serde_json::Value = serde_json::from_str(&reviews_query(&repo(), &[12, 9])).unwrap();
        let query = body["query"].as_str().unwrap();
        assert!(query.contains("p12: pullRequest(number: 12)") && query.contains("p9: pullRequest(number: 9)"));
        assert!(query.contains("reviewDecision") && query.contains("latestOpinionatedReviews(first: 30, writersOnly: true)"));
        assert!(!query.contains("ada") && !query.contains("app\""));
        assert_eq!(body["variables"], serde_json::json!({"owner": "ada", "name": "app"}));
    }

    #[test]
    fn reviews_are_asked_in_one_request_and_again_only_for_what_changed() {
        let first = list(&[pull(12, "1970-01-02T00:00:00Z"), pull(9, "1970-01-02T00:00:00Z")]);
        let second = list(&[pull(12, "1970-01-03T00:00:00Z"), pull(9, "1970-01-02T00:00:00Z")]);
        let later = r#"{"data": {"repository": {"p12": {"number": 12, "reviewDecision": "CHANGES_REQUESTED",
            "latestOpinionatedReviews": {"nodes": [{"state": "CHANGES_REQUESTED"}]}}}}}"#;
        let (mut client, sent) = scripted(
            vec![
                reply(200, &[("etag", "\"v1\"")], &first),
                reply(200, &[], FORK),
                reply(200, &[], REVIEWS),
                reply(304, &[], ""),
                reply(200, &[("etag", "\"v2\"")], &second),
                reply(200, &[], later),
            ],
            with_token(),
        );
        let pulls = fresh(client.pulls(NOW));
        assert!(pulls.iter().all(|pull| pull.review == Review::Unknown));
        let details = client.details(NOW).unwrap();
        assert_eq!(details.parent.as_ref().map(|parent| parent.repo.slug()).as_deref(), Some("octo/app"));
        assert_eq!(details.parent.as_ref().map(|parent| parent.default_branch.as_str()), Some("trunk"));
        let states: Vec<(u64, Review)> = details.pulls.iter().map(|pull| (pull.number, pull.review)).collect();
        assert_eq!(states, [(12, Review::Approved), (9, Review::ChangesRequested)]);

        // Nothing changed: no request at all, nothing to tell.
        assert_eq!(client.pulls(NOW), Outcome::Unchanged);
        assert_eq!(client.details(NOW), None);
        // #12 was updated: a fresh list keeps the known states, and only
        // #12 is asked about again.
        let pulls = fresh(client.pulls(NOW));
        assert_eq!(pulls.iter().map(|pull| pull.review).collect::<Vec<_>>(), [Review::Approved, Review::ChangesRequested]);
        let details = client.details(NOW).unwrap();
        assert_eq!(details.pulls.iter().map(|pull| (pull.number, pull.review)).collect::<Vec<_>>(), [(12, Review::ChangesRequested)]);
        assert_eq!(details.parent, None);

        let sent = sent.lock().unwrap();
        let urls: Vec<&str> = sent.iter().map(|request| request.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://api.github.com/repos/ada/app/pulls?state=open&per_page=30",
                "https://api.github.com/repos/ada/app",
                "https://api.github.com/graphql",
                "https://api.github.com/repos/ada/app/pulls?state=open&per_page=30",
                "https://api.github.com/repos/ada/app/pulls?state=open&per_page=30",
                "https://api.github.com/graphql",
            ]
        );
        assert!(sent.iter().all(|request| request.token.as_deref() == Some(SECRET)));
        let last: serde_json::Value = serde_json::from_str(sent[5].body.as_deref().unwrap()).unwrap();
        let query = last["query"].as_str().unwrap();
        assert!(query.contains("p12:") && !query.contains("p9:"));
    }

    #[test]
    fn without_a_sign_in_reviews_come_from_each_pull_request_a_few_per_round() {
        let reset = (NOW + 3600).to_string();
        let window = [("x-ratelimit-remaining", "50"), ("x-ratelimit-reset", reset.as_str())];
        let day = "1970-01-02T00:00:00Z";
        let first = list(&[7, 6, 5, 4, 3, 2, 1].map(|number| pull(number, day)));
        let second = list(&[pull(7, "1970-01-03T00:00:00Z"), pull(6, day), pull(5, day), pull(4, day), pull(3, day), pull(2, day), pull(1, day)]);
        let approved = r#"[{"id": 80, "user": {"login": "grace"}, "state": "APPROVED", "author_association": "MEMBER",
            "submitted_at": "1970-01-02T00:00:00Z", "commit_id": "1111111111111111111111111111111111111111"}]"#;
        let mut replies = vec![reply(200, &window, &first), reply(200, &window, r#"{"fork": false}"#)];
        replies.extend((0..6).map(|_| reply(200, &window, approved)));
        replies.extend([
            reply(404, &window, r#"{"message": "Not Found"}"#),
            reply(200, &window, &second),
            reply(200, &window, "[]"),
        ]);
        let (mut client, sent) = scripted(replies, Fake::default());

        fresh(client.pulls(NOW));
        // The parent once, then five pull requests' reviews, newest first.
        let details = client.details(NOW).unwrap();
        assert_eq!(details.parent, None);
        assert_eq!(details.pulls.iter().map(|pull| (pull.number, pull.review)).collect::<Vec<_>>(), [7, 6, 5, 4, 3].map(|n| (n, Review::Approved)));
        assert_eq!(details.pulls[0].approvals, Some(Approvals { given: 1, left: 0 }));
        // The other two next round; one is not found, which is no change
        // from unknown. Then nothing is left to ask.
        let details = client.details(NOW).unwrap();
        assert_eq!(details.pulls.iter().map(|pull| pull.number).collect::<Vec<_>>(), [2]);
        assert_eq!(client.details(NOW), None);
        // #7 changed: a fresh list keeps what is known, and #7 alone is asked.
        let pulls = fresh(client.pulls(NOW));
        assert_eq!(pulls.iter().filter(|pull| pull.review == Review::Approved).count(), 6);
        let details = client.details(NOW).unwrap();
        assert_eq!(details.pulls.iter().map(|pull| (pull.number, pull.review)).collect::<Vec<_>>(), [(7, Review::Unreviewed)]);

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 11);
        assert_eq!(sent[1].url, "https://api.github.com/repos/ada/app");
        assert_eq!(sent[2].url, "https://api.github.com/repos/ada/app/pulls/7/reviews?per_page=100");
        assert_eq!(sent[8].url, "https://api.github.com/repos/ada/app/pulls/1/reviews?per_page=100");
        assert_eq!(sent[10].url, "https://api.github.com/repos/ada/app/pulls/7/reviews?per_page=100");
        assert!(sent.iter().all(|request| request.token.is_none() && request.body.is_none()));
    }

    #[test]
    fn without_a_sign_in_reviews_leave_the_hours_last_requests_to_the_lists() {
        let reset = (NOW + 3600).to_string();
        let left = |n: &'static str| [("x-ratelimit-remaining", n), ("x-ratelimit-reset", reset.as_str())];
        let three = list(&[pull(3, "1970-01-02T00:00:00Z"), pull(2, "1970-01-02T00:00:00Z"), pull(1, "1970-01-02T00:00:00Z")]);
        let (mut client, sent) = scripted(
            vec![
                reply(200, &left("22"), &three),
                reply(200, &left("20"), r#"{"fork": false}"#),
                reply(200, &left("19"), "[]"),
            ],
            Fake::default(),
        );
        fresh(client.pulls(NOW));
        // Twenty left: one more review, which leaves nineteen, and no more.
        assert_eq!(client.details(NOW), Some(Details { pulls: vec![PullRequest { number: 3, review: Review::Unreviewed, approvals: Some(Approvals::default()), ..Default::default() }], parent: None }));
        assert_eq!(client.details(NOW), None);
        assert_eq!(sent.lock().unwrap().len(), 3);
        // A new hour: asked again.
        let (mut client, sent) = scripted(vec![reply(200, &left("5"), &three), reply(200, &[], "{}")], Fake::default());
        fresh(client.pulls(NOW));
        client.details(NOW);
        client.details(NOW + 3601);
        assert_eq!(sent.lock().unwrap().len(), 3);
        assert!(sent.lock().unwrap()[2].url.ends_with("/pulls/3/reviews?per_page=100"));
    }

    #[test]
    fn public_reviews_count_each_writers_last_word() {
        let review = |id: u32, login: &str, state: &str, association: &str| {
            format!(r#"{{"id": {id}, "user": {{"login": "{login}"}}, "state": "{state}", "author_association": "{association}"}}"#)
        };
        let of = |reviews: &[String]| parse_public_reviews(&format!("[{}]", reviews.join(","))).unwrap();
        let given = |n| Approvals { given: n, left: 0 };
        assert_eq!(of(&[]), (Review::Unreviewed, given(0)));
        assert_eq!(
            of(&[review(1, "grace", "APPROVED", "MEMBER"), review(2, "linus", "APPROVED", "COLLABORATOR")]),
            (Review::Approved, given(2))
        );
        // Changes asked for, then a comment: the request stands.
        let asked = [review(1, "grace", "CHANGES_REQUESTED", "OWNER"), review(2, "grace", "COMMENTED", "OWNER")];
        assert_eq!(of(&asked), (Review::ChangesRequested, given(0)));
        // Changes asked for, then approved: approved.
        let turned = [review(1, "grace", "CHANGES_REQUESTED", "OWNER"), review(2, "grace", "APPROVED", "OWNER")];
        assert_eq!(of(&turned), (Review::Approved, given(1)));
        // A dismissed approval counts for nothing; one from someone without
        // write access is left out, as GraphQL's `writersOnly` does.
        let dismissed = [review(1, "grace", "DISMISSED", "MEMBER"), review(2, "ada", "APPROVED", "CONTRIBUTOR")];
        assert_eq!(of(&dismissed), (Review::Unreviewed, given(0)));
        assert_eq!(of(&[review(1, "grace", "PENDING", "MEMBER")]), (Review::Unreviewed, given(0)));
        let ghost = r#"[{"id": 1, "user": null, "state": "CHANGES_REQUESTED", "author_association": "MEMBER"}]"#;
        assert_eq!(parse_public_reviews(ghost).unwrap(), (Review::Unreviewed, given(0)));
        assert!(parse_public_reviews(r#"{"message": "Not Found"}"#).is_err());
    }

    #[test]
    fn a_failed_review_request_leaves_them_unknown_and_does_not_hold_up_the_lists() {
        let one = list(&[pull(12, "1970-01-02T00:00:00Z")]);
        let spent = (NOW + 600).to_string();
        let limited = [("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", spent.as_str()), ("x-ratelimit-resource", "graphql")];
        let (mut client, sent) = scripted(
            vec![
                reply(200, &[], &one),
                reply(500, &[], ""),
                reply(403, &limited, r#"{"message": "API rate limit exceeded"}"#),
                reply(200, &[], &one),
            ],
            with_token(),
        );
        fresh(client.pulls(NOW));
        // The parent lookup failed (asked again later); the GraphQL budget is spent.
        assert_eq!(client.details(NOW), None);
        // The REST list is not held back by GraphQL's limit, and GraphQL
        // is not asked again before it lifts.
        fresh(client.pulls(NOW + 1));
        assert_eq!(sent.lock().unwrap().len(), 4);
        assert_eq!(client.blocked_until(NOW + 1), None);

        // Errors in place of data: unknown, and not asked again until the
        // pull request changes or the user refreshes.
        let (mut client, sent) = scripted(
            vec![
                reply(200, &[], &one),
                reply(200, &[], r#"{"fork": false}"#),
                reply(200, &[], r#"{"errors": [{"message": "Resource not accessible by integration"}]}"#),
                reply(200, &[], REVIEWS),
            ],
            with_token(),
        );
        fresh(client.pulls(NOW));
        assert_eq!(client.details(NOW), None);
        assert_eq!(client.details(NOW), None);
        assert_eq!(sent.lock().unwrap().len(), 3);
        client.look_again();
        let details = client.details(NOW).unwrap();
        assert_eq!(details.pulls[0].review, Review::Approved);
    }

    #[test]
    fn an_enterprise_server_is_asked_through_graphql_beside_its_api() {
        let sent = Log::default();
        let replies = vec![
            reply(200, &[], &list(&[pull(3, "1970-01-02T00:00:00Z")])),
            reply(200, &[], r#"{"fork": false}"#),
            reply(200, &[], r#"{"data": {"repository": {"p3": {"number": 3, "reviewDecision": "REVIEW_REQUIRED"}}}}"#),
        ];
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        let ghe = RepoId { service: Service::GitHubEnterprise, host: "ghe.acme.com".to_owned(), ..repo() };
        let secrets = Fake { env: vec![("GH_ENTERPRISE_TOKEN", SECRET)], ..Default::default() };
        let mut client = Client::new(ghe, Box::new(transport), Box::new(secrets));
        fresh(client.pulls(NOW));
        let details = client.details(NOW).unwrap();
        assert_eq!(details.pulls[0].review, Review::Required);
        let urls: Vec<String> = sent.lock().unwrap().iter().map(|request| request.url.clone()).collect();
        assert_eq!(urls[1], "https://ghe.acme.com/api/v3/repos/ada/app");
        assert_eq!(urls[2], "https://ghe.acme.com/api/graphql");
    }

    #[test]
    fn a_parent_is_taken_only_when_its_name_is_one_a_remote_could_have() {
        assert_eq!(parse_parent(r#"{"fork": false, "parent": null}"#, &repo()).unwrap(), None);
        let odd = r#"{"fork": true, "parent": {"full_name": "../etc", "default_branch": "main"}}"#;
        assert_eq!(parse_parent(odd, &repo()).unwrap(), None);
        let spaced = r#"{"fork": true, "parent": {"full_name": "octo/app", "default_branch": "my branch"}}"#;
        let parent = parse_parent(spaced, &repo()).unwrap().unwrap();
        assert_eq!((parent.repo.slug().as_str(), parent.default_branch.as_str()), ("octo/app", ""));
        assert!(parse_parent("[]", &repo()).is_err());
    }

    const OPENED: &str = r#"{"number": 51, "title": "Into the parent", "draft": false,
       "user": {"login": "ada"},
       "head": {"ref": "feature", "sha": "5555555555555555555555555555555555555555", "repo": {"full_name": "ada/app"}},
       "base": {"ref": "trunk", "sha": "0000000000000000000000000000000000000000", "repo": {"full_name": "octo/app"}}}"#;

    fn new_pull() -> NewPull {
        NewPull {
            head: "feature".to_owned(),
            base: "trunk".to_owned(),
            title: "Into the parent".to_owned(),
            body: String::new(),
            draft: false,
        }
    }

    #[test]
    fn a_pull_request_from_a_fork_names_its_branch_owner_colon_branch() {
        let fork = repo();
        let parent = RepoId { owner: "octo".to_owned(), ..repo() };
        let (mut client, sent) = scripted(vec![reply(201, &[], OPENED)], with_token());
        let across = Across { from: fork.clone(), into: parent.clone() };
        let opened = client.create_pull_across(&new_pull(), &across, NOW).unwrap();
        assert_eq!(opened.number, 51);
        assert!(!opened.is_from(&parent));
        let sent = sent.lock().unwrap();
        assert_eq!(sent[0].url, "https://api.github.com/repos/octo/app/pulls");
        let body: serde_json::Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["head"], "ada:feature");
        assert_eq!(body["base"], "trunk");
        assert!(body.get("head_repo").is_none());
        drop(sent);

        // A fork owned by the same account as its parent (renamed): its
        // name is needed as well.
        let renamed = RepoId { name: "app-fork".to_owned(), ..parent.clone() };
        let (mut client, sent) = scripted(vec![reply(201, &[], OPENED)], with_token());
        client.create_pull_across(&new_pull(), &Across { from: renamed, into: parent.clone() }, NOW).unwrap();
        let body: serde_json::Value = serde_json::from_str(sent.lock().unwrap()[0].body.as_deref().unwrap()).unwrap();
        assert_eq!((body["head"].as_str(), body["head_repo"].as_str()), (Some("octo:feature"), Some("app-fork")));

        // Within one repository (a branch of the parent itself): the plain form.
        let (mut client, sent) = scripted(vec![reply(201, &[], OPENED)], with_token());
        client.create_pull_across(&new_pull(), &Across { from: parent.clone(), into: parent.clone() }, NOW).unwrap();
        let body: serde_json::Value = serde_json::from_str(sent.lock().unwrap()[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["head"], "feature");

        // Never to another server, and nothing is sent then.
        let elsewhere = RepoId { service: Service::GitHubEnterprise, host: "ghe.acme.com".to_owned(), ..parent };
        let (mut client, sent) = scripted(vec![reply(201, &[], OPENED)], with_token());
        assert!(client.create_pull_across(&new_pull(), &Across { from: fork, into: elsewhere }, NOW).is_err());
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn a_refusal_across_forks_says_which_branch_github_lacks() {
        let missing = r#"{"message": "Validation Failed", "errors": [{"resource": "PullRequest", "field": "head", "code": "invalid"}]}"#;
        let (mut client, _) = scripted(vec![reply(422, &[], missing)], with_token());
        let across = Across { from: repo(), into: RepoId { owner: "octo".to_owned(), ..repo() } };
        let problem = client.create_pull_across(&new_pull(), &across, NOW).unwrap_err();
        assert_eq!(problem, Problem::Refused("GitHub does not have the branch ada:feature: push it first".to_owned()));
    }

    #[test]
    fn the_offline_fixture_answers_each_pull_requests_reviews_without_a_sign_in() {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-canned-public-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pulls.json"), list(&[pull(12, "1970-01-02T00:00:00Z"), pull(9, "1970-01-02T00:00:00Z")])).unwrap();
        let approved = r#"[{"id": 1, "user": {"login": "grace"}, "state": "APPROVED", "author_association": "OWNER"}]"#;
        std::fs::write(dir.join("reviews-12.json"), approved).unwrap();
        let canned = Canned { dir: dir.clone(), created: false };
        let mut client = Client::new(repo(), Box::new(canned), Box::new(NoSecrets));
        fresh(client.pulls(NOW));
        // No repo.json: not a fork. No reviews-9.json: unknown.
        let details = client.details(NOW).unwrap();
        assert_eq!(details.parent, None);
        assert_eq!(details.pulls.iter().map(|pull| (pull.number, pull.review)).collect::<Vec<_>>(), [(12, Review::Approved)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_offline_fixture_answers_reviews_and_the_parent() {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-canned-reviews-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pulls.json"), list(&[pull(12, "1970-01-02T00:00:00Z"), pull(9, "1970-01-02T00:00:00Z")])).unwrap();
        std::fs::write(dir.join("graphql.json"), REVIEWS).unwrap();
        std::fs::write(dir.join("repo.json"), FORK).unwrap();
        let canned = Canned { dir: dir.clone(), created: false };
        let mut client = Client::new(repo(), Box::new(canned), Box::new(FixtureSignIn));
        fresh(client.pulls(NOW));
        let details = client.details(NOW).unwrap();
        assert_eq!(details.pulls.len(), 2);
        assert_eq!(details.parent.unwrap().repo.slug(), "octo/app");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
