//! Bitbucket Data Center (formerly Bitbucket Server), on a server the user
//! named: its REST API 1.0, below `/rest` on the server.
//!
//! A repository is a project's (`PRJ/app`) or a user's personal one
//! (`~ada/app`). Its remotes are `https://<server>/scm/<project>/<repo>.git`
//! (after the path the server is installed under, if any) and
//! `ssh://git@<server>:7999/<project>/<repo>.git`; its web pages are below
//! `/projects/<key>/repos/<repo>/` (or `/users/<name>/repos/<repo>/`).
//!
//! Pull requests carry their reviewers' states (approved, needs work).
//! Each one has a ref on the remote, `refs/pull-requests/<id>/from`, also
//! when it comes from a fork, so any of them can be checked out. There are
//! no issues (Bitbucket Data Center leaves them to Jira).
//!
//! CI comes from the build statuses CI servers post about commits: there
//! is no list of a repository's builds, so the round asks about the heads of
//! the open pull requests and of the most recently changed branches. One
//! request (`/build-status/1.0/commits/stats`) says which of them have
//! builds and how they stand; only those whose counts moved are read again
//! (`/build-status/1.0/commits/<id>`). Builds are shown to signed-in users
//! only ([`Problem::SignInNeeded`] otherwise, without asking).
//!
//! The repository itself is asked once, for the repository it was forked
//! from (its `origin`). Pull requests between forks name both repositories.
//!
//! Sign-ins: an HTTP access token in `BITBUCKET_SERVER_TOKEN` while
//! `BITBUCKET_SERVER_HOST` names the server (sent as a bearer token), then
//! Git's credential helper for the server: its user name and password as
//! HTTP Basic (a password, or an HTTP access token in its place), then the
//! password alone as a bearer token. The server answers 401 both for a
//! refused sign-in and for a missing permission; the second is told apart
//! by its `AuthorisationException`.

use std::cmp::Reverse;

use serde::Deserialize;

use super::account::{Profile, PublicKey, SshKey};
use super::signin::{Api, Got, json};
use super::*;

/// Sent with every request.
pub const HEADERS: &[(&str, &str)] = &[("Accept", "application/json")];

/// The core REST API, below [`RepoId::api_base`] (`.../rest`).
const CORE: &str = "api/1.0";

/// Commits whose builds are asked about in a round, at most.
const COMMITS: usize = 20;

/// Builds shown at most.
const SHOWN: usize = 30;

/// A page of the repository's site: `/projects/<key>/repos/<repo>/<path>`,
/// or `/users/<name>/repos/<repo>/<path>` for a personal repository.
pub fn repo_page(repo: &RepoId, path: &str) -> String {
    match repo.owner.strip_prefix('~') {
        Some(user) => format!("https://{}/users/{user}/repos/{}/{path}", repo.host, repo.name),
        None => format!("https://{}/projects/{}/repos/{}/{path}", repo.host, repo.owner, repo.name),
    }
}

/// Looks for a sign-in for the server, most explicit first, passing over
/// the ones already `rejected`.
pub fn find_token(secrets: &dyn Secrets, repo: &RepoId, rejected: &[Token]) -> Option<(Token, TokenSource)> {
    let fresh = |token: &Token| !rejected.contains(token);
    // Only for the server the variable beside it names.
    let named = secrets.env("BITBUCKET_SERVER_HOST").is_some_and(|value| same_host(&value, &repo.host));
    if named
        && let Some(token) = secrets.env("BITBUCKET_SERVER_TOKEN").and_then(|raw| Token::new(&raw)).filter(fresh)
    {
        return Some((token, TokenSource::Env("BITBUCKET_SERVER_TOKEN")));
    }
    // Git's credential helper knows a server by its name, not the path it
    // is installed under.
    let (user, password) = secrets.credential(server_name(&repo.host), "")?;
    let basic = (!user.is_empty()).then(|| Token::basic(&user, &password)).flatten();
    [basic, Token::new(&password)]
        .into_iter()
        .flatten()
        .find(|token| fresh(token))
        .map(|token| (token, TokenSource::CredentialHelper))
}

// ---- JSON ----

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawError {
    message: String,
    #[serde(rename = "exceptionName")]
    exception_name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawErrors {
    errors: Vec<RawError>,
}

/// What an error body says: `{"errors": [{"message": ..., "exceptionName":
/// ...}]}`, the messages joined.
pub fn api_message(body: &str) -> String {
    let raw: RawErrors = serde_json::from_str(body).unwrap_or_default();
    let messages: Vec<&str> = raw.errors.iter().map(|error| error.message.trim()).filter(|text| !text.is_empty()).collect();
    messages.join("; ")
}

/// Whether an error answer is about a missing permission rather than the
/// sign-in: Bitbucket Data Center answers 401 for both, and names the first
/// an `AuthorisationException`.
fn missing_permission(body: &str) -> bool {
    let raw: RawErrors = serde_json::from_str(body).unwrap_or_default();
    raw.errors
        .iter()
        .filter_map(|error| error.exception_name.as_deref())
        .any(|name| name.ends_with("AuthorisationException") || name.ends_with("AuthorizationException"))
}

/// Whether a refusal status turns the sign-in away (see [`missing_permission`]).
pub fn about_sign_in(response: &HttpResponse) -> bool {
    !missing_permission(&response.body)
}

/// Whether the error answer names an exception ending in `name`.
fn exception(body: &str, name: &str) -> bool {
    let raw: RawErrors = serde_json::from_str(body).unwrap_or_default();
    raw.errors.iter().filter_map(|error| error.exception_name.as_deref()).any(|found| found.ends_with(name))
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawUser {
    name: Option<String>,
    slug: Option<String>,
    display_name: Option<String>,
    email_address: Option<String>,
}

/// An account's name as it signs in; a removed one has none left.
fn user_name(user: Option<RawUser>) -> String {
    let user = user.unwrap_or_default();
    [user.name, user.slug, user.display_name]
        .into_iter()
        .flatten()
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "Former user".to_owned())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawParticipant {
    user: Option<RawUser>,
    approved: bool,
    /// `UNAPPROVED`, `NEEDS_WORK` or `APPROVED`.
    status: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawProject {
    key: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRepository {
    slug: String,
    project: Option<RawProject>,
    /// The repository this one was forked from.
    origin: Option<Box<RawRepository>>,
}

impl RawRepository {
    /// `KEY/slug`, when both are there.
    fn path(&self) -> Option<String> {
        let key = &self.project.as_ref()?.key;
        (!key.is_empty() && !self.slug.is_empty()).then(|| format!("{key}/{}", self.slug))
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawRef {
    /// `refs/heads/feature`.
    id: String,
    /// `feature`.
    display_id: String,
    latest_commit: Option<String>,
    repository: Option<RawRepository>,
}

impl RawRef {
    fn branch(&self) -> String {
        let name = if self.display_id.is_empty() {
            self.id.strip_prefix("refs/heads/").unwrap_or(&self.id)
        } else {
            &self.display_id
        };
        branch_name(name)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPull {
    id: u64,
    #[serde(default)]
    title: String,
    /// Bitbucket Data Center 8.18 and later.
    #[serde(default)]
    draft: bool,
    author: Option<RawParticipant>,
    #[serde(default)]
    from_ref: RawRef,
    #[serde(default)]
    to_ref: RawRef,
    /// Milliseconds since the epoch.
    updated_date: Option<i64>,
    #[serde(default)]
    reviewers: Vec<RawParticipant>,
}

/// One page of a list.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page<T> {
    values: Vec<T>,
    #[serde(default = "last_page")]
    is_last_page: bool,
}

fn last_page() -> bool {
    true
}

/// Milliseconds since the epoch, as seconds; 0 when unknown.
fn seconds(millis: Option<i64>) -> i64 {
    millis.filter(|millis| *millis > 0).map_or(0, |millis| millis / 1000)
}

/// What the reviewers have said: a request for more work wins, then
/// approvals. The merge checks (how many approvals are required) are not in
/// the list, so a pull request is never "review required" here.
fn review(reviewers: &[RawParticipant]) -> (Review, Option<Approvals>) {
    let said = |reviewer: &RawParticipant, word: &str| reviewer.status.as_deref() == Some(word);
    let given = reviewers.iter().filter(|reviewer| reviewer.approved || said(reviewer, "APPROVED")).count();
    let review = if reviewers.iter().any(|reviewer| said(reviewer, "NEEDS_WORK")) {
        Review::ChangesRequested
    } else if given > 0 {
        Review::Approved
    } else {
        Review::Unreviewed
    };
    (review, Some(Approvals { given: given as u32, left: 0 }))
}

// `..Default::default()` keeps fields added to `PullRequest` later at "unknown".
#[allow(clippy::needless_update, reason = "PullRequest gains fields this backend leaves unknown")]
fn pull_request(pull: RawPull) -> PullRequest {
    let (review, approvals) = review(&pull.reviewers);
    PullRequest {
        number: pull.id,
        title: pull.title,
        author: user_name(pull.author.and_then(|author| author.user)),
        draft: pull.draft,
        head_ref: pull.from_ref.branch(),
        head_sha: pull.from_ref.latest_commit.clone().unwrap_or_default(),
        head_repo: pull.from_ref.repository.as_ref().and_then(RawRepository::path),
        base_ref: pull.to_ref.branch(),
        updated: seconds(pull.updated_date),
        review,
        approvals,
        ..Default::default()
    }
}

/// The open pull requests, and whether there are more.
pub fn parse_pulls(text: &str) -> Result<(Vec<PullRequest>, bool), String> {
    let page: Page<RawPull> = json(text)?;
    Ok((page.values.into_iter().map(pull_request).collect(), !page.is_last_page))
}

/// One pull request, as the API answers its creation.
pub fn parse_pull(text: &str) -> Result<PullRequest, String> {
    json::<RawPull>(text).map(pull_request)
}

/// The branches of `GET .../branches`: each one's name and the commit at
/// its tip.
pub fn parse_branches(text: &str) -> Result<(Vec<(String, String)>, bool), String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawBranch {
        #[serde(default)]
        display_id: String,
        latest_commit: Option<String>,
    }
    let page: Page<RawBranch> = json(text)?;
    let branches = page
        .values
        .into_iter()
        .filter_map(|branch| Some((branch.latest_commit?, branch_name(&branch.display_id))))
        .collect();
    Ok((branches, !page.is_last_page))
}

/// How many builds of a commit stand where: what
/// `/build-status/1.0/commits/stats` says of each commit that has any.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct Counts {
    successful: u32,
    in_progress: u32,
    failed: u32,
    cancelled: u32,
    unknown: u32,
}

/// The answer to `POST /build-status/1.0/commits/stats`: the counts of each
/// commit that has builds (commits without any are left out).
pub fn parse_stats(text: &str) -> Result<BTreeMap<String, Counts>, String> {
    json(text)
}

/// A build's state.
pub fn build_state(state: &str) -> RunState {
    match state {
        "SUCCESSFUL" => RunState::Success,
        "FAILED" => RunState::Failure,
        "INPROGRESS" => RunState::Running,
        "CANCELLED" => RunState::Cancelled,
        // UNKNOWN, and whatever comes next.
        _ => RunState::Skipped,
    }
}

/// The builds of the commit `sha` (on `branch`, when it is a branch's tip
/// or a pull request's head), from `GET /build-status/1.0/commits/<sha>`.
/// Servers older than 7.14 date a build with `dateAdded`.
pub fn parse_builds(text: &str, sha: &str, branch: &str) -> Result<Vec<Run>, String> {
    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct RawBuild {
        state: String,
        key: String,
        name: Option<String>,
        description: Option<String>,
        build_number: Option<serde_json::Value>,
        created_date: Option<i64>,
        date_added: Option<i64>,
        #[serde(rename = "ref")]
        reference: Option<String>,
    }
    let page: Page<RawBuild> = json(text)?;
    Ok(page
        .values
        .into_iter()
        .map(|build| {
            let number = match &build.build_number {
                Some(serde_json::Value::Number(number)) => number.as_u64().unwrap_or(0),
                Some(serde_json::Value::String(text)) => text.trim().parse().unwrap_or(0),
                _ => 0,
            };
            let workflow = build.name.filter(|name| !name.trim().is_empty()).unwrap_or(build.key);
            let built_on = build.reference.as_deref().map(|reference| {
                branch_name(reference.strip_prefix("refs/heads/").unwrap_or(reference))
            });
            Run {
                id: number,
                workflow: tidy(&workflow),
                title: tidy(&build.description.unwrap_or_default()),
                number,
                branch: built_on.filter(|name| !name.is_empty()).unwrap_or_else(|| branch.to_owned()),
                event: String::new(),
                head_sha: sha.to_owned(),
                state: build_state(&build.state),
                outcome: build.state.to_lowercase().replace("inprogress", "in progress"),
                when: seconds(build.created_date.or(build.date_added)),
            }
        })
        .collect())
}

/// The repository a fork was made from, in the answer to `GET
/// .../projects/<key>/repos/<repo>`: `None` when it is not a fork, or the
/// name is not one a remote could have.
pub fn parse_parent(text: &str, repo: &RepoId) -> Result<Option<Parent>, String> {
    let raw: RawRepository = json(text)?;
    Ok(raw
        .origin
        .and_then(|origin| origin.path())
        .and_then(|path| repo.sibling(&path))
        .map(|repo| Parent { repo, default_branch: String::new() }))
}

/// A ref of `repo` in a new pull request.
fn end(branch: &str, repo: &RepoId) -> serde_json::Value {
    serde_json::json!({
        "id": format!("refs/heads/{branch}"),
        "repository": {"slug": repo.name, "project": {"key": repo.owner}},
    })
}

/// The body of a new pull request from `new.head` in `from` into
/// `new.base` in `into` (the same repository, or a fork and its parent).
/// The draft flag is only sent when set: servers older than 8.18 do not
/// know it.
pub fn new_pull_json(new: &NewPull, from: &RepoId, into: &RepoId) -> String {
    let mut json = serde_json::json!({
        "title": new.title.trim(),
        "description": new.body,
        "fromRef": end(&new.head, from),
        "toRef": end(&new.base, into),
        "reviewers": [],
    });
    if new.draft {
        json["draft"] = serde_json::Value::Bool(true);
    }
    json.to_string()
}

// ---- the account (Preferences > Integrations) ----

/// Where to ask whose sign-in a token is: any answer of the server names
/// the signed-in user in its `X-AUSERNAME` header, and the application's
/// properties are the smallest one. `api` is [`RepoId::api_base`].
pub fn whoami_url(api: &str) -> String {
    format!("{api}/{CORE}/application-properties")
}

/// The user signed in, from an answer's `X-AUSERNAME` header; `None` for
/// an anonymous answer.
pub fn signed_in_user(response: &HttpResponse) -> Option<String> {
    let name = tidy(response.header("x-ausername")?);
    (!name.is_empty() && name != "anonymous").then_some(name)
}

/// Where the account `name` is described.
pub fn user_url(api: &str, name: &str) -> String {
    format!("{api}/{CORE}/users/{}", url_escape(name))
}

/// `GET /api/1.0/users/<name>`: the account, or just its name when the
/// answer has nothing more.
pub fn parse_user(text: &str, name: &str) -> Profile {
    let raw: RawUser = serde_json::from_str(text).unwrap_or_default();
    let present = |text: Option<String>| text.map(|text| tidy(&text)).filter(|text| !text.is_empty());
    Profile {
        id: 0,
        login: present(raw.name).unwrap_or_else(|| name.to_owned()),
        name: present(raw.display_name).unwrap_or_default(),
        email: present(raw.email_address),
    }
}

/// Where the signed-in user's SSH keys are listed and added.
pub fn keys_url(api: &str) -> String {
    format!("{api}/ssh/1.0/keys")
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawKey {
    id: u64,
    text: String,
    label: Option<String>,
    /// Milliseconds since the epoch, or (in newer descriptions) a date.
    created_date: Option<serde_json::Value>,
    /// Days the key works for after it was added; none when it does not
    /// expire.
    expiry_days: Option<i64>,
}

fn key_model(raw: RawKey) -> SshKey {
    let parts: Vec<&str> = raw.text.split_whitespace().take(2).collect();
    let created = match &raw.created_date {
        Some(serde_json::Value::Number(millis)) => seconds(millis.as_i64()),
        Some(serde_json::Value::String(text)) => match text.trim().parse::<i64>() {
            Ok(millis) => seconds(Some(millis)),
            Err(_) => timestamp(Some(text)),
        },
        _ => 0,
    };
    let expires = match raw.expiry_days.filter(|days| *days > 0) {
        Some(days) if created > 0 => created + days * 86_400,
        _ => 0,
    };
    let comment = raw.text.split_whitespace().nth(2).map(str::to_owned);
    let title = raw.label.filter(|label| !label.trim().is_empty()).or(comment).unwrap_or_default();
    SshKey {
        id: raw.id,
        title: tidy(&title),
        key: parts.join(" "),
        fingerprint: parts.get(1).and_then(|blob| account::fingerprint(blob)).unwrap_or_default(),
        created,
        expires,
    }
}

/// A page of `GET /ssh/1.0/keys`.
pub fn parse_keys(text: &str) -> Result<Vec<SshKey>, String> {
    Ok(json::<Page<RawKey>>(text)?.values.into_iter().map(key_model).collect())
}

/// The key a `POST /ssh/1.0/keys` answers with.
pub fn parse_key(text: &str) -> Result<SshKey, String> {
    json::<RawKey>(text).map(key_model)
}

/// The body of `POST /ssh/1.0/keys`: the key and its label.
pub fn new_key_json(title: &str, key: &PublicKey) -> String {
    serde_json::json!({ "text": key.line(), "label": title }).to_string()
}

// ---- the client ----

/// The lists asked for, each with its own validator.
#[derive(Clone, Copy)]
enum List {
    Pulls,
    Branches,
}

pub struct Client {
    api: Api,
    /// The head commits of the last list of open pull requests, with their
    /// branches: their builds are asked about.
    heads: Vec<(String, String)>,
    /// The tips of the most recently changed branches, likewise.
    branches: Vec<(String, String)>,
    /// Each commit's builds as last read, with the counts they were read at.
    builds: BTreeMap<String, (Counts, Vec<Run>)>,
    /// The repository's parent once asked; `Some(None)` when it has none.
    parent: Option<Option<Parent>>,
}

impl Client {
    pub fn new(repo: RepoId, transport: Box<dyn Transport>, secrets: Box<dyn Secrets>) -> Self {
        let api = Api::new(repo, transport, secrets, 2, &[401], api_message).telling_refusals(about_sign_in);
        Client { api, heads: Vec::new(), branches: Vec::new(), builds: BTreeMap::new(), parent: None }
    }

    /// An address below `/rest/api/1.0/projects/<key>/repos/<repo>` of `repo`.
    fn url_of(base: &str, repo: &RepoId, path: &str) -> String {
        let at = format!("{base}/{CORE}/projects/{}/repos/{}", repo.owner, repo.name);
        if path.is_empty() { at } else { format!("{at}/{path}") }
    }

    fn repo_url(&self, path: &str) -> String {
        Self::url_of(&self.api.base, &self.api.repo, path)
    }

    /// A 401 that came back with a sign-in is a missing permission (a
    /// refusal of the sign-in itself is dealt with by [`Api`]).
    fn no_access(response: &HttpResponse, signed_in: bool) -> Option<Result<Got, Problem>> {
        (signed_in && response.status == 401).then_some(Err(Problem::NoAccess))
    }

    /// POST the new pull request `body` to the pull requests of `into`.
    fn create(&mut self, into: &RepoId, new: &NewPull, body: &str, now: u64) -> Result<PullRequest, Problem> {
        self.api.look_again();
        let url = Self::url_of(&self.api.base, into, "pull-requests");
        let response = self.api.post(&url, body, true, now)?;
        let message = self.api.message(&response.body);
        let lower = message.to_lowercase();
        let (head, base) = (&new.head, &new.base);
        let site = self.api.repo.site();
        let empty = exception(&response.body, "EmptyPullRequestException") || lower.contains("up-to-date");
        let duplicate = exception(&response.body, "DuplicatePullRequestException") || lower.contains("only one pull request");
        match response.status {
            200 | 201 => parse_pull(&response.body).map_err(|error| self.api.unreadable(error)),
            409 if empty => {
                Err(Problem::Refused(format!("No commits between {base} and {head}: there is nothing to merge")))
            }
            409 if duplicate => Err(Problem::Refused(format!("A pull request from {head} into {base} is already open"))),
            // "One of the specified repositories or branches does not exist".
            404 => Err(Problem::Refused(format!("{site} does not have the branch {head} or {base}: push it first"))),
            401 => Err(Problem::Refused(match message.is_empty() {
                true => format!("No permission to open pull requests in {}", into.slug()),
                false => format!("No permission to open pull requests in {} ({message})", into.slug()),
            })),
            400 | 409 if !message.is_empty() => Err(Problem::Refused(message)),
            status => Err(Problem::Failed(format!("{} answered {status}", self.api.name()))),
        }
    }

    /// GET the repository itself, for the one it was forked from. `None`
    /// when there was no answer worth keeping: asked again next round.
    fn ask_parent(&mut self, now: u64) -> Option<Option<Parent>> {
        let url = self.repo_url("");
        let response = self.api.get(url, None, now).ok()?;
        match response.status {
            200..=299 => Some(parse_parent(&response.body, &self.api.repo).unwrap_or(None)),
            500..=599 => None,
            _ => Some(None),
        }
    }

    /// The commits whose builds are shown: the pull requests' heads, then
    /// the branches' tips, each once, with the branch it is on.
    fn commits(&self) -> Vec<(String, String)> {
        let mut commits: Vec<(String, String)> = Vec::new();
        for (sha, branch) in self.heads.iter().chain(&self.branches) {
            let full = sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit());
            if full && !commits.iter().any(|(seen, _)| seen.eq_ignore_ascii_case(sha)) {
                commits.push((sha.to_ascii_lowercase(), branch.clone()));
            }
        }
        commits.truncate(COMMITS);
        commits
    }

    /// The builds of the commits of [`Client::commits`]: which have any, and
    /// then each one whose builds changed. A problem stops the round, and
    /// what was shown stays.
    fn builds(&mut self, now: u64) -> Result<(Vec<Run>, bool), Problem> {
        let commits = self.commits();
        if commits.is_empty() {
            self.builds.clear();
            return Ok((Vec::new(), false));
        }
        let shas: Vec<&str> = commits.iter().map(|(sha, _)| sha.as_str()).collect();
        let body = serde_json::to_string(&shas).unwrap_or_default();
        let url = format!("{}/build-status/1.0/commits/stats", self.api.base);
        let response = self.api.post(&url, &body, true, now)?;
        let stats = match self.api.classify(response)? {
            Got::Body(response) => parse_stats(&response.body).map_err(|error| self.api.unreadable(error))?,
            Got::Unchanged | Got::Nothing => BTreeMap::new(),
        };
        self.builds.retain(|sha, _| stats.contains_key(sha));
        for (sha, branch) in &commits {
            let Some(counts) = stats.get(sha) else { continue };
            if self.builds.get(sha).is_some_and(|(seen, _)| seen == counts) {
                continue;
            }
            let url = format!("{}/build-status/1.0/commits/{sha}?orderBy=NEWEST&limit=25", self.api.base);
            let response = self.api.get(url, None, now)?;
            let runs = match self.api.classify(response)? {
                Got::Body(response) => {
                    parse_builds(&response.body, sha, branch).map_err(|error| self.api.unreadable(error))?
                }
                Got::Unchanged | Got::Nothing => Vec::new(),
            };
            self.builds.insert(sha.clone(), (*counts, runs));
        }
        let mut runs: Vec<Run> = commits
            .iter()
            .filter_map(|(sha, _)| self.builds.get(sha))
            .flat_map(|(_, runs)| runs.iter().cloned())
            .collect();
        runs.sort_by_key(|run| Reverse(run.when));
        let more = runs.len() > SHOWN;
        runs.truncate(SHOWN);
        Ok((runs, more))
    }
}

impl Backend for Client {
    fn pulls(&mut self, now: u64) -> Outcome<PullRequest> {
        self.api.start_round();
        let url = self.repo_url("pull-requests?state=OPEN&order=NEWEST&limit=30");
        let outcome = self.api.fetch(url, List::Pulls as usize, now, Self::no_access, |response| parse_pulls(&response.body));
        if let Outcome::Fresh { items, .. } = &outcome {
            self.heads = items.iter().map(|pull| (pull.head_sha.clone(), pull.head_ref.clone())).collect();
        }
        outcome
    }

    /// Issues are Jira's: nothing to ask.
    fn issues(&mut self, _now: u64) -> Outcome<Issue> {
        Outcome::Fresh { items: Vec::new(), more: false }
    }

    fn runs(&mut self, now: u64) -> Outcome<Run> {
        // The build statuses are not shown to anyone signed out, public
        // repositories included: nothing to ask.
        if !self.api.signed_in() {
            return Outcome::Failed(Problem::SignInNeeded { what: "builds" });
        }
        let url = self.repo_url("branches?orderBy=MODIFICATION&details=false&limit=10");
        match self.api.fetch(url, List::Branches as usize, now, Self::no_access, |response| parse_branches(&response.body)) {
            Outcome::Fresh { items, .. } => self.branches = items,
            Outcome::Unchanged | Outcome::Absent => {}
            Outcome::Failed(problem) => return Outcome::Failed(problem),
        }
        match self.builds(now) {
            Ok((items, more)) => Outcome::Fresh { items, more },
            Err(problem) => Outcome::Failed(self.api.scrub(problem)),
        }
    }

    fn auth(&self) -> Auth {
        self.api.auth()
    }

    fn look_again(&mut self) {
        self.api.look_again();
        // Builds are read again.
        self.builds.clear();
    }

    fn blocked_until(&self, now: u64) -> Option<u64> {
        self.api.blocked_until(now)
    }

    fn create_pull(&mut self, new: &NewPull, now: u64) -> Result<PullRequest, Problem> {
        let repo = self.api.repo.clone();
        let created = self.create(&repo, new, &new_pull_json(new, &repo, &repo), now);
        created.map_err(|problem| self.api.scrub(problem))
    }

    /// Opened in `across.into`, from a branch of `across.from`.
    fn create_pull_across(&mut self, new: &NewPull, across: &Across, now: u64) -> Result<PullRequest, Problem> {
        let here = |repo: &RepoId| repo.service == self.api.repo.service && repo.host == self.api.repo.host;
        if !here(&across.from) || !here(&across.into) {
            return Err(Problem::Failed(format!("Both repositories must be on {}", self.api.repo.site())));
        }
        let body = new_pull_json(new, &across.from, &across.into);
        let created = self.create(&across.into, new, &body, now);
        created.map_err(|problem| self.api.scrub(problem))
    }

    fn details(&mut self, now: u64) -> Option<Details> {
        if self.parent.is_some() {
            return None;
        }
        let parent = self.ask_parent(now)?;
        self.parent = Some(parent.clone());
        parent.map(|parent| Details { pulls: Vec::new(), parent: Some(parent) })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fake, Log, NOW, OTHER, SECRET, Script, reply};
    use super::*;

    fn servers() -> Servers {
        [
            ("bitbucket.acme.com".to_owned(), ServerKind::BitbucketDataCenter),
            ("acme.com/bitbucket".to_owned(), ServerKind::BitbucketDataCenter),
        ]
        .into_iter()
        .collect()
    }

    fn repo() -> RepoId {
        parse_remote("https://bitbucket.acme.com/scm/prj/app.git", &servers()).unwrap()
    }

    fn scripted(replies: Vec<Result<HttpResponse, String>>, secrets: Fake) -> (Client, Log) {
        let sent = Log::default();
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        (Client::new(repo(), Box::new(transport), Box::new(secrets)), sent)
    }

    fn with_token() -> Fake {
        Fake {
            env: vec![("BITBUCKET_SERVER_HOST", "https://bitbucket.acme.com/"), ("BITBUCKET_SERVER_TOKEN", SECRET)],
            ..Default::default()
        }
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

    const BASE: &str = "https://bitbucket.acme.com/rest";

    // Trimmed-down answers, after the REST API reference (RestPullRequest,
    // RestBuildStatus, RestSshKey) and its examples.
    pub(super) const PULLS: &str = r#"{"size": 2, "limit": 30, "isLastPage": false, "start": 0, "nextPageStart": 2,
      "values": [
      {"id": 12, "version": 3, "title": "Add dark mode", "description": "Themes.", "state": "OPEN", "open": true,
       "closed": false, "draft": false, "createdDate": 43200000, "updatedDate": 86400000,
       "fromRef": {"id": "refs/heads/feature/dark", "displayId": "feature/dark", "type": "BRANCH",
                   "latestCommit": "1111111111111111111111111111111111111111",
                   "repository": {"slug": "app", "name": "App", "project": {"key": "PRJ", "type": "NORMAL"}}},
       "toRef": {"id": "refs/heads/main", "displayId": "main", "type": "BRANCH",
                 "latestCommit": "0000000000000000000000000000000000000000",
                 "repository": {"slug": "app", "project": {"key": "PRJ"}}},
       "locked": false,
       "author": {"user": {"name": "ada", "emailAddress": "ada@acme.com", "id": 101, "displayName": "Ada Lovelace",
                           "active": true, "slug": "ada", "type": "NORMAL"}, "role": "AUTHOR", "approved": false,
                  "status": "UNAPPROVED"},
       "reviewers": [{"user": {"name": "grace", "displayName": "Grace Hopper", "slug": "grace"},
                      "lastReviewedCommit": "1111111111111111111111111111111111111111", "role": "REVIEWER",
                      "approved": true, "status": "APPROVED"}],
       "participants": [],
       "links": {"self": [{"href": "https://bitbucket.acme.com/projects/PRJ/repos/app/pull-requests/12"}]}},
      {"id": 9, "title": "Parser", "state": "OPEN", "draft": true, "updatedDate": 1000,
       "fromRef": {"id": "refs/heads/parser", "displayId": "parser",
                   "latestCommit": "2222222222222222222222222222222222222222",
                   "repository": {"slug": "app", "project": {"key": "~GRACE", "type": "PERSONAL"}}},
       "toRef": {"id": "refs/heads/main", "displayId": "main", "repository": {"slug": "app", "project": {"key": "PRJ"}}},
       "author": {"user": {"name": "grace"}, "role": "AUTHOR"},
       "reviewers": [{"user": {"name": "ada"}, "approved": true, "status": "APPROVED"},
                     {"user": {"name": "linus"}, "approved": false, "status": "NEEDS_WORK"}]}
    ]}"#;

    pub(super) const BRANCHES: &str = r#"{"size": 2, "limit": 10, "isLastPage": true, "values": [
      {"id": "refs/heads/main", "displayId": "main", "type": "BRANCH",
       "latestCommit": "4444444444444444444444444444444444444444",
       "latestChangeset": "4444444444444444444444444444444444444444", "isDefault": true},
      {"id": "refs/heads/feature/dark", "displayId": "feature/dark", "type": "BRANCH",
       "latestCommit": "1111111111111111111111111111111111111111", "isDefault": false}
    ]}"#;

    pub(super) const STATS: &str = r#"{
      "1111111111111111111111111111111111111111": {"successful": 1, "inProgress": 0, "failed": 1, "cancelled": 0, "unknown": 0},
      "4444444444444444444444444444444444444444": {"successful": 0, "inProgress": 1, "failed": 0, "cancelled": 0, "unknown": 0}
    }"#;

    pub(super) const BUILDS_PR: &str = r#"{"size": 2, "limit": 25, "isLastPage": true, "values": [
      {"buildNumber": "42", "createdDate": 7200000, "updatedDate": 7300000, "description": "Unit and lint",
       "duration": 100000, "key": "APP-CI", "name": "CI", "parent": "APP", "projectKey": "PRJ",
       "ref": "refs/heads/feature/dark", "repositorySlug": "app", "state": "SUCCESSFUL",
       "testResults": {"failed": 0, "skipped": 0, "successful": 134}, "url": "https://ci.acme.com/browse/APP-CI-42"},
      {"key": "APP-E2E", "name": "Browser tests", "state": "FAILED", "dateAdded": 3600000,
       "url": "https://ci.acme.com/browse/APP-E2E-7"}
    ]}"#;

    pub(super) const BUILDS_MAIN: &str = r#"{"size": 1, "limit": 25, "isLastPage": true, "values": [
      {"buildNumber": 8, "createdDate": 1000, "key": "APP-DEPLOY", "name": "", "state": "INPROGRESS"}
    ]}"#;

    /// `GET .../projects/~ADA/repos/app` of a fork, trimmed.
    pub(super) const FORK: &str = r#"{"slug": "app", "id": 7, "name": "app", "hierarchyId": "e3c939f9ef4a7fae272e",
      "scmId": "git", "state": "AVAILABLE", "forkable": true, "public": false,
      "project": {"key": "~ADA", "id": 3, "type": "PERSONAL", "owner": {"name": "ada"}},
      "origin": {"slug": "app", "id": 2, "name": "app", "scmId": "git", "project": {"key": "PRJ", "id": 1, "name": "Project", "type": "NORMAL"}}}"#;

    pub(super) const CREATED: &str = r#"{"id": 13, "title": "Speed up the graph", "state": "OPEN", "draft": true,
      "updatedDate": 172800000,
      "fromRef": {"id": "refs/heads/feature/fast", "displayId": "feature/fast",
                  "latestCommit": "5555555555555555555555555555555555555555",
                  "repository": {"slug": "app", "project": {"key": "PRJ"}}},
      "toRef": {"id": "refs/heads/main", "displayId": "main", "repository": {"slug": "app", "project": {"key": "PRJ"}}},
      "author": {"user": {"name": "ada"}}, "reviewers": []}"#;

    fn denied(exception: &str) -> String {
        format!(r#"{{"errors": [{{"context": null, "message": "You are not permitted to access this resource", "exceptionName": "{exception}"}}]}}"#)
    }

    const BAD_TOKEN: &str = r#"{"errors": [{"context": null, "message": "Authentication failed. Please check your credentials and try again.", "exceptionName": "com.atlassian.bitbucket.auth.IncorrectPasswordAuthenticationException"}]}"#;

    #[test]
    fn remotes_are_recognised_once_the_server_is_named() {
        for url in [
            "https://bitbucket.acme.com/scm/prj/app.git",
            "https://ada@bitbucket.acme.com/scm/prj/app.git",
            "https://bitbucket.acme.com/scm/prj/app",
            "ssh://git@bitbucket.acme.com:7999/prj/app.git",
            "git@bitbucket.acme.com:prj/app.git",
        ] {
            assert_eq!(parse_remote(url, &servers()), Some(repo()), "{url}");
            assert_eq!(parse_remote(url, &BTreeMap::new()), None, "{url}");
        }
        let repo = repo();
        assert_eq!((repo.service, repo.host.as_str(), repo.slug().as_str()), (Service::BitbucketDataCenter, "bitbucket.acme.com", "prj/app"));
        assert!(repo.self_hosted());
        assert_eq!(repo.api_base(), BASE);

        // A personal repository, and a server installed under a path.
        let personal = parse_remote("https://bitbucket.acme.com/scm/~ada/app.git", &servers()).unwrap();
        assert_eq!(personal.owner, "~ada");
        assert_eq!(parse_remote("ssh://git@bitbucket.acme.com:7999/~ada/app.git", &servers()), Some(personal));
        let under = parse_remote("https://acme.com/bitbucket/scm/prj/app.git", &servers()).unwrap();
        assert_eq!((under.host.as_str(), under.api_base().as_str()), ("acme.com/bitbucket", "https://acme.com/bitbucket/rest"));
        assert_eq!(parse_remote("ssh://git@acme.com:7999/prj/app.git", &servers()).unwrap().host, "acme.com/bitbucket");

        for url in [
            // Web pages and addresses without the `/scm/`.
            "https://bitbucket.acme.com/projects/PRJ/repos/app/browse",
            "https://bitbucket.acme.com/prj/app.git",
            "https://bitbucket.acme.com/scm/prj/sub/app.git",
            "https://bitbucket.acme.com/scm/prj/~app.git",
            "https://bitbucket.acme.com/scm/~/app.git",
            "https://bitbucket.acme.com/scm/~~ada/app.git",
        ] {
            assert_eq!(parse_remote(url, &servers()), None, "{url}");
        }
        // Nobody else's paths have a `~`.
        let gitea: Servers = [("bitbucket.acme.com".to_owned(), ServerKind::Gitea)].into_iter().collect();
        assert_eq!(parse_remote("https://bitbucket.acme.com/~ada/app.git", &gitea), None);
    }

    #[test]
    fn a_server_is_offered_as_bitbucket_by_its_name_or_its_scm_path() {
        let offer = |url: &str| offer_for(&RemoteInfo { name: "origin".into(), url: url.into() }).unwrap();
        assert_eq!(offer("https://git.acme.com/scm/prj/app.git").guess, Some(ServerKind::BitbucketDataCenter));
        assert_eq!(offer("ssh://git@bitbucket.acme.com:7999/prj/app.git").guess, Some(ServerKind::BitbucketDataCenter));
        assert_eq!(offer("ssh://git@stash.acme.com:7999/prj/app.git").guess, Some(ServerKind::BitbucketDataCenter));
        assert_eq!(offer("https://git.acme.com/prj/app.git").guess, None);
        // Connected with the path it is installed under, when its HTTPS
        // remote has one before the `/scm/`.
        let under = offer("https://acme.com/bitbucket/scm/prj/app.git");
        assert_eq!(server_key(ServerKind::BitbucketDataCenter, &under.host, &under.path), "acme.com/bitbucket");
        assert_eq!(server_key(ServerKind::BitbucketDataCenter, "git.acme.com", "scm/prj/app"), "git.acme.com");
        assert_eq!(server_key(ServerKind::BitbucketDataCenter, "git.acme.com", "prj/app"), "git.acme.com");
        assert_eq!(server_key(ServerKind::GitLab, "acme.com", "bitbucket/scm/prj/app"), "acme.com");
        let kept: Servers = [("acme.com/bitbucket".to_owned(), ServerKind::BitbucketDataCenter)].into_iter().collect();
        let remotes = [RemoteInfo { name: "origin".into(), url: "https://acme.com/bitbucket/scm/prj/app.git".into() }];
        assert!(offers(&remotes, &kept).is_empty());
        assert_eq!(ServerKind::from("bitbucket-data-center".to_owned()), ServerKind::BitbucketDataCenter);
        assert_eq!(<&str>::from(ServerKind::BitbucketDataCenter), "bitbucket-data-center");
    }

    #[test]
    fn links_and_words_follow_bitbucket_data_centers() {
        let repo = repo();
        let site = "https://bitbucket.acme.com/projects/prj/repos/app";
        assert_eq!(repo.pulls_url(), format!("{site}/pull-requests"));
        assert_eq!(repo.pull_url(12), format!("{site}/pull-requests/12/overview"));
        assert_eq!(repo.runs_url(), format!("{site}/commits"));
        assert_eq!(repo.home_url(), site);
        assert_eq!(
            repo.compare_url("main", "feature/dark mode"),
            format!("{site}/pull-requests?create&sourceBranch=refs/heads/feature/dark%20mode&targetBranch=refs/heads/main")
        );
        let run = Run { head_sha: "1111111111111111111111111111111111111111".to_owned(), ..build() };
        assert_eq!(repo.run_page(&run), format!("{site}/commits/1111111111111111111111111111111111111111"));
        let odd = Run { head_sha: "11111111/../../x".to_owned(), ..build() };
        assert_eq!(repo.run_page(&odd), format!("{site}/commits"));
        assert_eq!(repo.clone_url().as_deref(), Some("https://bitbucket.acme.com/scm/prj/app.git"));
        let personal = RepoId { owner: "~ada".to_owned(), ..repo.clone() };
        assert_eq!(personal.pull_url(3), "https://bitbucket.acme.com/users/ada/repos/app/pull-requests/3/overview");

        let service = Service::BitbucketDataCenter;
        assert_eq!((service.pulls_title(), service.ci_title(), service.pull_number(12)), ("PULL REQUESTS", "BUILDS", "#12".to_owned()));
        assert!(service.has_pull_refs() && !service.has_issues() && service.has_pulls() && service.cli_tool().is_none());
        assert_eq!(checkout_pull("origin", service, 12, false).label, "Check out pull request #12");
    }

    fn build() -> Run {
        Run {
            id: 0,
            workflow: String::new(),
            title: String::new(),
            number: 0,
            branch: String::new(),
            event: String::new(),
            head_sha: String::new(),
            state: RunState::Queued,
            outcome: String::new(),
            when: 0,
        }
    }

    #[test]
    fn pull_requests_map_to_the_model_with_their_reviewers() {
        let (pulls, more) = parse_pulls(PULLS).unwrap();
        assert!(more);
        let first = &pulls[0];
        assert_eq!((first.number, first.title.as_str(), first.author.as_str(), first.draft), (12, "Add dark mode", "ada", false));
        assert_eq!((first.head_ref.as_str(), first.base_ref.as_str(), first.updated), ("feature/dark", "main", 86_400));
        assert_eq!(first.head_sha, "1111111111111111111111111111111111111111");
        assert!(first.is_from(&repo()));
        assert_eq!((first.review, first.approvals.unwrap().given), (Review::Approved, 1));
        // From a personal fork, a draft, and someone asked for more work.
        let second = &pulls[1];
        assert_eq!((second.head_repo.as_deref(), second.draft, second.review), (Some("~GRACE/app"), true, Review::ChangesRequested));
        assert!(!second.is_from(&repo()));
        assert!(parse_pulls("[]").is_err());
        let lonely = r#"{"values": [{"id": 1, "fromRef": {}, "toRef": {}, "author": {}}]}"#;
        let (pulls, more) = parse_pulls(lonely).unwrap();
        assert_eq!((pulls[0].author.as_str(), pulls[0].review, more), ("Former user", Review::Unreviewed, false));
    }

    #[test]
    fn builds_map_to_runs_and_their_pull_requests_checks() {
        let runs = parse_builds(BUILDS_PR, "1111111111111111111111111111111111111111", "feature/dark").unwrap();
        assert_eq!((runs[0].workflow.as_str(), runs[0].number, runs[0].state), ("CI", 42, RunState::Success));
        assert_eq!((runs[0].title.as_str(), runs[0].branch.as_str(), runs[0].when), ("Unit and lint", "feature/dark", 7200));
        assert_eq!((runs[1].state, runs[1].outcome.as_str(), runs[1].when, runs[1].number), (RunState::Failure, "failed", 3600, 0));
        let deploy = parse_builds(BUILDS_MAIN, "4444444444444444444444444444444444444444", "main").unwrap();
        assert_eq!((deploy[0].workflow.as_str(), deploy[0].number, deploy[0].outcome.as_str()), ("APP-DEPLOY", 8, "in progress"));
        let (pulls, _) = parse_pulls(PULLS).unwrap();
        assert_eq!(checks_for(&pulls[0].head_sha, &runs), Some(Checks::Failing));
        for (state, expected) in [
            ("SUCCESSFUL", RunState::Success),
            ("FAILED", RunState::Failure),
            ("INPROGRESS", RunState::Running),
            ("CANCELLED", RunState::Cancelled),
            ("UNKNOWN", RunState::Skipped),
        ] {
            assert_eq!(build_state(state), expected, "{state}");
        }
        let stats = parse_stats(STATS).unwrap();
        assert_eq!(stats.len(), 2);
        assert!(parse_stats("[]").is_err());
    }

    #[test]
    fn a_round_asks_for_the_builds_of_pull_requests_and_branches_and_only_what_changed() {
        let (mut client, sent) = scripted(
            vec![
                reply(200, &[], PULLS),
                reply(200, &[], BRANCHES),
                reply(200, &[], STATS),
                reply(200, &[], BUILDS_PR),
                reply(200, &[], BUILDS_MAIN),
                // The next round: nothing moved.
                reply(200, &[], PULLS),
                reply(200, &[], BRANCHES),
                reply(200, &[], STATS),
            ],
            with_token(),
        );
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, more: true } if items.len() == 2));
        assert_eq!(client.issues(NOW), Outcome::Fresh { items: Vec::new(), more: false });
        let Outcome::Fresh { items: runs, more: false } = client.runs(NOW) else { panic!("no builds") };
        // Newest first, whichever commit they are about.
        let names: Vec<&str> = runs.iter().map(|run| run.workflow.as_str()).collect();
        assert_eq!(names, ["CI", "Browser tests", "APP-DEPLOY"]);
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("BITBUCKET_SERVER_TOKEN")));
        let repo = format!("{BASE}/api/1.0/projects/prj/repos/app");
        assert_eq!(
            urls(&sent),
            [
                format!("{repo}/pull-requests?state=OPEN&order=NEWEST&limit=30"),
                format!("{repo}/branches?orderBy=MODIFICATION&details=false&limit=10"),
                format!("{BASE}/build-status/1.0/commits/stats"),
                format!("{BASE}/build-status/1.0/commits/1111111111111111111111111111111111111111?orderBy=NEWEST&limit=25"),
                format!("{BASE}/build-status/1.0/commits/4444444444444444444444444444444444444444?orderBy=NEWEST&limit=25"),
            ]
        );
        // The pull requests' heads (one is also a branch's tip, and is asked
        // about once), then the branches' tips.
        let asked: Vec<String> = serde_json::from_str(sent.lock().unwrap()[2].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            asked,
            [
                "1111111111111111111111111111111111111111",
                "2222222222222222222222222222222222222222",
                "4444444444444444444444444444444444444444"
            ]
        );
        assert!(sent.lock().unwrap().iter().all(|request| within(BASE, &request.url) && request.token.as_deref() == Some(SECRET)));

        client.pulls(NOW);
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, .. } if items.len() == 3));
        assert_eq!(sent.lock().unwrap().len(), 8);
    }

    #[test]
    fn builds_are_not_asked_for_without_a_sign_in() {
        let (mut client, sent) = scripted(vec![reply(200, &[], PULLS)], Fake::default());
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(client.auth(), Auth::Anonymous);
        let problem = failure(client.runs(NOW));
        assert_eq!(problem, Problem::SignInNeeded { what: "builds" });
        assert_eq!(problem.line(Service::BitbucketDataCenter), "Sign in to see builds");
        let detail = problem.detail(Service::BitbucketDataCenter);
        assert!(detail.contains("shows builds only to someone signed in") && detail.contains("BITBUCKET_SERVER_TOKEN"), "{detail}");
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert!(sent.lock().unwrap()[0].token.is_none());
    }

    #[test]
    fn a_missing_permission_is_told_from_a_refused_sign_in() {
        // The token is fine, the repository is not for this account.
        let (mut client, sent) =
            scripted(vec![reply(401, &[], &denied("com.atlassian.bitbucket.AuthorisationException"))], with_token());
        assert_eq!(failure(client.pulls(NOW)), Problem::NoAccess);
        assert_eq!((client.auth(), sent.lock().unwrap().len()), (Auth::SignedIn(TokenSource::Env("BITBUCKET_SERVER_TOKEN")), 1));

        // A refused token gives way to the credential helper's, then to none.
        let secrets = Fake { helper: Some(OTHER), ..with_token() };
        let (mut client, sent) = scripted(
            vec![reply(401, &[], BAD_TOKEN), reply(401, &[], BAD_TOKEN), reply(401, &[], &denied("com.atlassian.bitbucket.AuthorisationException"))],
            secrets,
        );
        assert_eq!(failure(client.pulls(NOW)), Problem::SignInRejected);
        let tokens: Vec<Option<String>> = sent.lock().unwrap().iter().map(|request| request.token.clone()).collect();
        assert_eq!(tokens, [Some(SECRET.to_owned()), Some(OTHER.to_owned()), None]);
        assert_eq!(client.auth(), Auth::Rejected);

        // Without a sign-in, a private repository.
        let (mut client, _) = scripted(vec![reply(401, &[], &denied("com.atlassian.bitbucket.AuthorisationException"))], Fake::default());
        assert_eq!(failure(client.pulls(NOW)), Problem::NotSignedIn);
        let detail = Problem::NotSignedIn.detail(Service::BitbucketDataCenter);
        assert!(detail.contains("BITBUCKET_SERVER_TOKEN") && detail.contains("BITBUCKET_SERVER_HOST"), "{detail}");

        assert_eq!(api_message(BAD_TOKEN), "Authentication failed. Please check your credentials and try again.");
        assert_eq!(api_message("<html>"), "");
    }

    #[test]
    fn sign_ins_are_the_named_servers_token_then_the_credential_helpers() {
        let repo = repo();
        let (token, source) = find_token(&with_token(), &repo, &[]).unwrap();
        assert_eq!((source, token.authorization()), (TokenSource::Env("BITBUCKET_SERVER_TOKEN"), format!("Bearer {SECRET}")));
        // A token for another server is not this one's.
        let elsewhere = Fake {
            env: vec![("BITBUCKET_SERVER_HOST", "bitbucket.other.com"), ("BITBUCKET_SERVER_TOKEN", SECRET)],
            ..Default::default()
        };
        assert_eq!(find_token(&elsewhere, &repo, &[]), None);
        let unnamed = Fake { env: vec![("BITBUCKET_SERVER_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(find_token(&unnamed, &repo, &[]), None);
        // The server under a path is named by its name alone too.
        let under = parse_remote("https://acme.com/bitbucket/scm/prj/app.git", &servers()).unwrap();
        let named = Fake { env: vec![("BITBUCKET_SERVER_HOST", "acme.com"), ("BITBUCKET_SERVER_TOKEN", SECRET)], ..Default::default() };
        assert!(find_token(&named, &under, &[]).is_some());
        // The credential helper is asked about the server's name.
        let helper = Fake { helper: Some(OTHER), ..Default::default() };
        let (token, source) = find_token(&helper, &under, &[]).unwrap();
        assert_eq!((source, token.authorization()), (TokenSource::CredentialHelper, format!("Bearer {OTHER}")));
        assert!(helper.asked().contains(&"git acme.com".to_owned()));
        // Other services' variables are never read.
        let github = Fake { env: vec![("GITHUB_TOKEN", SECRET), ("BITBUCKET_ACCESS_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(find_token(&github, &repo, &[]), None);
    }

    /// The credential helper as git answers it: a user name and a password.
    struct Helper(&'static str, &'static str);

    impl Secrets for Helper {
        fn env(&self, _name: &str) -> Option<String> {
            None
        }
        fn cli(&self, _service: Service, _host: &str) -> Option<String> {
            None
        }
        fn credential_helper(&self, _host: &str) -> Option<String> {
            Some(self.1.to_owned())
        }
        fn credential(&self, host: &str, path: &str) -> Option<(String, String)> {
            assert_eq!((host, path), ("bitbucket.acme.com", ""));
            Some((self.0.to_owned(), self.1.to_owned()))
        }
    }

    #[test]
    fn the_credential_helpers_password_goes_as_basic_then_as_bearer() {
        let repo = repo();
        let helper = Helper("ada", SECRET);
        let (basic, _) = find_token(&helper, &repo, &[]).unwrap();
        assert_eq!(basic.authorization(), format!("Basic {}", base64_padded(format!("ada:{SECRET}").as_bytes())));
        let (bearer, _) = find_token(&helper, &repo, std::slice::from_ref(&basic)).unwrap();
        assert_eq!(bearer.authorization(), format!("Bearer {SECRET}"));
        assert_eq!(find_token(&helper, &repo, &[basic, bearer]), None);
    }

    #[test]
    fn a_pull_request_is_opened_with_one_post_naming_both_repositories() {
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], with_token());
        let new = NewPull {
            head: "feature/fast".to_owned(),
            base: "main".to_owned(),
            title: " Speed up the graph ".to_owned(),
            body: "Caches lane colours.".to_owned(),
            draft: true,
        };
        let created = client.create_pull(&new, NOW).unwrap();
        assert!(created.draft && created.number == 13 && created.is_from(&repo()));
        let body: serde_json::Value = serde_json::from_str(sent.lock().unwrap()[0].body.as_deref().unwrap()).unwrap();
        let end = |branch: &str, key: &str| serde_json::json!({"id": format!("refs/heads/{branch}"), "repository": {"slug": "app", "project": {"key": key}}});
        assert_eq!(
            body,
            serde_json::json!({
                "title": "Speed up the graph",
                "description": "Caches lane colours.",
                "fromRef": end("feature/fast", "prj"),
                "toRef": end("main", "prj"),
                "reviewers": [],
                "draft": true,
            })
        );
        assert_eq!(sent.lock().unwrap()[0].url, format!("{BASE}/api/1.0/projects/prj/repos/app/pull-requests"));

        // From a personal fork into its parent: opened in the parent.
        let fork = RepoId { owner: "~ada".to_owned(), ..repo() };
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], with_token());
        let not_draft = NewPull { draft: false, ..new.clone() };
        client.create_pull_across(&not_draft, &Across { from: fork.clone(), into: repo() }, NOW).unwrap();
        let body: serde_json::Value = serde_json::from_str(sent.lock().unwrap()[0].body.as_deref().unwrap()).unwrap();
        assert_eq!((body["fromRef"].clone(), body.get("draft")), (end("feature/fast", "~ada"), None));
        assert_eq!(sent.lock().unwrap()[0].url, format!("{BASE}/api/1.0/projects/prj/repos/app/pull-requests"));
        // Never to another server.
        let github = RepoId { service: Service::GitHub, host: "github.com".to_owned(), ..repo() };
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], with_token());
        assert!(client.create_pull_across(&new, &Across { from: fork, into: github }, NOW).is_err());
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn opening_a_pull_request_says_why_it_failed() {
        let new = NewPull {
            head: "feature/fast".to_owned(),
            base: "main".to_owned(),
            title: "Speed up".to_owned(),
            body: String::new(),
            draft: false,
        };
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], Fake::default());
        assert_eq!(client.create_pull(&new, NOW), Err(Problem::NotSignedIn));
        assert!(sent.lock().unwrap().is_empty());

        let error = |exception: &str, message: &str| {
            format!(r#"{{"errors": [{{"context": null, "message": "{message}", "exceptionName": "{exception}"}}]}}"#)
        };
        let empty = error(
            "com.atlassian.bitbucket.pull.EmptyPullRequestException",
            "Repository app of project with key PRJ is already up-to-date with branch feature/fast.",
        );
        let duplicate = error(
            "com.atlassian.bitbucket.pull.DuplicatePullRequestException",
            "Only one pull request may be open for a given source and target branch",
        );
        let missing = error("com.atlassian.bitbucket.repository.NoSuchBranchException", "Branch feature/fast does not exist.");
        for (status, body, expected) in [
            (409, empty.as_str(), Problem::Refused("No commits between main and feature/fast: there is nothing to merge".to_owned())),
            (409, duplicate.as_str(), Problem::Refused("A pull request from feature/fast into main is already open".to_owned())),
            (404, missing.as_str(), Problem::Refused("bitbucket.acme.com does not have the branch feature/fast or main: push it first".to_owned())),
            (
                401,
                &denied("com.atlassian.bitbucket.AuthorisationException"),
                Problem::Refused("No permission to open pull requests in prj/app (You are not permitted to access this resource)".to_owned()),
            ),
            (400, &error("x", "The title is required"), Problem::Refused("The title is required".to_owned())),
            (500, "", Problem::Failed("Bitbucket Data Center answered 500".to_owned())),
        ] {
            let (mut client, _) = scripted(vec![reply(status, &[], body)], with_token());
            assert_eq!(client.create_pull(&new, NOW), Err(expected), "{status} {body}");
        }
    }

    #[test]
    fn a_forks_origin_is_asked_once_and_taken_only_with_a_name_a_remote_could_have() {
        let fork = RepoId { owner: "~ada".to_owned(), ..repo() };
        let sent = Log::default();
        let transport = Script { replies: vec![reply(503, &[], ""), reply(200, &[], FORK)].into(), sent: sent.clone() };
        let mut client = Client::new(fork.clone(), Box::new(transport), Box::new(with_token()));
        assert_eq!(client.details(NOW), None);
        let parent = client.details(NOW).unwrap().parent.unwrap();
        assert!(parent.repo.same(&repo()));
        assert_eq!(parent.repo.owner, "PRJ");
        assert_eq!(client.details(NOW), None);
        assert_eq!(urls(&sent), [format!("{BASE}/api/1.0/projects/~ada/repos/app"), format!("{BASE}/api/1.0/projects/~ada/repos/app")]);
        assert_eq!(parse_parent(r#"{"slug": "app", "project": {"key": "PRJ"}}"#, &fork).unwrap(), None);
        assert_eq!(parse_parent(r#"{"origin": {"slug": "../x", "project": {"key": "PRJ"}}}"#, &fork).unwrap(), None);
        assert!(parse_parent("42", &fork).is_err());
    }

    #[test]
    fn any_pull_request_is_checked_out_from_its_ref_in_the_repository() {
        let (pulls, _) = parse_pulls(PULLS).unwrap();
        // From the personal fork too: the ref is in this repository.
        let cmd = checkout_pull_request("origin", Service::BitbucketDataCenter, &repo(), &pulls[1], false).unwrap();
        assert_eq!(cmd.label, "Check out pull request #9");
        assert_eq!(cmd.steps[0].args, ["fetch", "--no-tags", "origin", "refs/pull-requests/9/from:pr/9"]);
    }

    #[test]
    fn a_token_never_reaches_a_displayed_error() {
        let echo = format!(r#"{{"errors": [{{"message": "bad {SECRET}"}}]}}"#);
        for replies in [
            vec![reply(403, &[], &echo)],
            vec![reply(500, &[], &echo)],
            vec![Err(format!("proxy said Authorization: Bearer {SECRET}"))],
            vec![reply(200, &[], &format!("{{\"values\": \"{SECRET}\"}}"))],
        ] {
            let (mut client, _) = scripted(replies, with_token());
            let problem = failure(client.pulls(NOW));
            for text in [problem.line(Service::BitbucketDataCenter), problem.detail(Service::BitbucketDataCenter), format!("{problem:?}")] {
                assert!(!text.contains(SECRET), "{text}");
            }
        }
    }

    #[test]
    fn the_offline_fixture_serves_every_endpoint_from_its_own_file() {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-bbdc-canned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in [
            ("pull-requests", PULLS),
            ("branches", BRANCHES),
            ("stats", STATS),
            ("builds-1111111111111111111111111111111111111111", BUILDS_PR),
            ("builds-4444444444444444444444444444444444444444", BUILDS_MAIN),
            ("repo", FORK),
            ("create", CREATED),
        ] {
            std::fs::write(dir.join(format!("{name}.json")), body).unwrap();
        }
        let canned = Canned { dir: dir.clone(), created: false };
        let mut client = Client::new(repo(), Box::new(canned), Box::new(FixtureSignIn));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, .. } if items.len() == 3));
        assert!(client.details(NOW).unwrap().parent.is_some());
        let new = NewPull { head: "feature/fast".into(), base: "main".into(), title: "x".into(), body: String::new(), draft: false };
        assert_eq!(client.create_pull(&new, NOW).unwrap().number, 13);
        // The body sent is kept; asking for the stats is no creation.
        let sent: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("create.sent.json")).unwrap()).unwrap();
        assert_eq!(sent["fromRef"]["id"], "refs/heads/feature/fast");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_account_and_its_keys_are_read_in_bitbucket_data_centers_shape() {
        let api = repo().api_base();
        assert_eq!(whoami_url(&api), format!("{BASE}/api/1.0/application-properties"));
        assert_eq!(user_url(&api, "ada lovelace"), format!("{BASE}/api/1.0/users/ada%20lovelace"));
        assert_eq!(keys_url(&api), format!("{BASE}/ssh/1.0/keys"));
        let answer = |name: Option<&str>| HttpResponse {
            status: 200,
            headers: name.map(|name| vec![("x-ausername".to_owned(), name.to_owned())]).unwrap_or_default(),
            body: r#"{"version": "9.6.0", "buildNumber": "9006000", "displayName": "Bitbucket"}"#.to_owned(),
        };
        assert_eq!(signed_in_user(&answer(Some("ada"))).as_deref(), Some("ada"));
        assert_eq!(signed_in_user(&answer(Some("anonymous"))), None);
        assert_eq!(signed_in_user(&answer(None)), None);

        let profile = parse_user(
            r#"{"name": "ada", "emailAddress": "ada@acme.com", "active": true, "displayName": "Ada Lovelace", "id": 101, "slug": "ada", "type": "NORMAL"}"#,
            "ada",
        );
        assert_eq!((profile.login.as_str(), profile.display_name(), profile.email.as_deref()), ("ada", "Ada Lovelace", Some("ada@acme.com")));
        assert_eq!(parse_user("<html>", "ada").login, "ada");

        let keys = parse_keys(
            r#"{"size": 2, "limit": 25, "isLastPage": true, "values": [
              {"id": 1, "text": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ6/rvs4h30pDKGBvPxCCedyMBv2RjD2wg++vAYe/VwB ada@laptop",
               "label": "Laptop", "algorithmType": "ED25519", "bitLength": 256, "createdDate": 86400000, "expiryDays": 30},
              {"id": 2, "text": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ6/rvs4h30pDKGBvPxCCedyMBv2RjD2wg++vAYe/VwB ada@desk",
               "createdDate": "1970-01-03T00:00:00Z"}
            ]}"#,
        )
        .unwrap();
        assert_eq!((keys[0].id, keys[0].title.as_str(), keys[0].created, keys[0].expires), (1, "Laptop", 86_400, 86_400 * 31));
        assert_eq!(keys[0].fingerprint, "SHA256:rLmucD0ZT+3NWpg/ewTzpahexLO2rz/okUf2br8ey0Y");
        assert!(!keys[0].key.contains("ada@laptop"));
        assert_eq!((keys[1].title.as_str(), keys[1].created, keys[1].expires), ("ada@desk", 172_800, 0));
        assert!(parse_keys("{}").is_err());
    }
}
