//! Bitbucket Cloud (bitbucket.org): the REST API 2.0 endpoints and their
//! JSON. Bitbucket Data Center has another API and is not covered.
//!
//! Lists come in pages (`{"values": [...], "next": "..."}`). A pull
//! request's head commit is listed by its first 12 digits only; the CI
//! verdict is matched to it with [`same_commit`]. There is no ref for a pull
//! request's head on the remote: one from this repository is checked out
//! from its branch ([`checkout_pull_request`]).
//!
//! There are no issues to list: Bitbucket Cloud removed its issue tracker,
//! API included, on 20 August 2026 (its endpoints answer 410 Gone), so
//! nothing is asked for them ([`Service::has_issues`]). After the lists the
//! repository itself is asked once, for a fork's `parent`; a pull request
//! into the parent names the fork as its source's `repository`.
//!
//! Sign-ins use HTTP Basic: an Atlassian API token with the account's
//! e-mail address (`BITBUCKET_EMAIL` and `BITBUCKET_API_TOKEN`), or an app
//! password with the user name (`BITBUCKET_USERNAME` and
//! `BITBUCKET_APP_PASSWORD`, which Atlassian is retiring). A repository,
//! project or workspace access token (`BITBUCKET_ACCESS_TOKEN`) is sent as
//! a bearer token. Git's credential helper is asked last: its user name and
//! password as Basic, then the password alone as a bearer token, which is
//! what an OAuth sign-in through Git Credential Manager keeps.

use serde::Deserialize;

use super::account::{Profile, PublicKey, SshKey};
use super::signin::{Api, Got, json};
use super::*;

/// The API. A token is only ever sent below it.
pub const API: &str = "https://api.bitbucket.org/2.0";

/// Sent with every request.
pub const HEADERS: &[(&str, &str)] = &[("Accept", "application/json")];

/// Looks for a sign-in for bitbucket.org, most explicit first, passing
/// over the ones already `rejected`.
pub fn find_token(secrets: &dyn Secrets, repo: &RepoId, rejected: &[Token]) -> Option<(Token, TokenSource)> {
    let fresh = |token: &Token| !rejected.contains(token);
    if let Some(token) = secrets.env("BITBUCKET_ACCESS_TOKEN").and_then(|raw| Token::new(&raw)).filter(fresh) {
        return Some((token, TokenSource::Env("BITBUCKET_ACCESS_TOKEN")));
    }
    for (name, users) in [
        // API tokens sign in with the Atlassian account's e-mail address.
        ("BITBUCKET_API_TOKEN", ["BITBUCKET_EMAIL", "BITBUCKET_USERNAME"]),
        ("BITBUCKET_APP_PASSWORD", ["BITBUCKET_USERNAME", "BITBUCKET_EMAIL"]),
    ] {
        let Some(secret) = secrets.env(name) else { continue };
        let user = users.iter().find_map(|user| secrets.env(user).filter(|user| !user.trim().is_empty()));
        if let Some(token) = user.and_then(|user| Token::basic(&user, &secret)).filter(fresh) {
            return Some((token, TokenSource::Env(name)));
        }
    }
    let (user, password) = secrets.credential(&repo.host, "")?;
    // `x-token-auth` is how git sends an OAuth or access token.
    let basic = (!user.is_empty() && user != "x-token-auth").then(|| Token::basic(&user, &password)).flatten();
    [basic, Token::new(&password)]
        .into_iter()
        .flatten()
        .find(|token| fresh(token))
        .map(|token| (token, TokenSource::CredentialHelper))
}

// ---- JSON ----

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawAccount {
    display_name: Option<String>,
    nickname: Option<String>,
    username: Option<String>,
}

/// Bitbucket shows a deleted account as "Former user".
fn account_name(account: Option<RawAccount>) -> String {
    let account = account.unwrap_or_default();
    [account.nickname, account.username, account.display_name]
        .into_iter()
        .flatten()
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "Former user".to_owned())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawName {
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawCommit {
    hash: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRepository {
    full_name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawEnd {
    branch: RawName,
    commit: Option<RawCommit>,
    repository: Option<RawRepository>,
}

#[derive(Deserialize)]
struct RawPull {
    id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    draft: bool,
    author: Option<RawAccount>,
    #[serde(default)]
    source: RawEnd,
    #[serde(default)]
    destination: RawEnd,
    updated_on: Option<String>,
    /// Asked for with `fields=+values.participants`: reviewers and anyone
    /// who approved or asked for changes.
    #[serde(default)]
    participants: Vec<RawParticipant>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawParticipant {
    approved: bool,
    /// `approved`, `changes_requested`, or none.
    state: Option<String>,
}

/// What the participants have said: changes requested wins, then
/// approvals. Bitbucket's rules (merge checks) are not in the list, so a
/// pull request nobody has approved yet is unreviewed, never "required".
fn review(participants: &[RawParticipant]) -> (Review, Option<Approvals>) {
    let given = participants.iter().filter(|p| p.approved || p.state.as_deref() == Some("approved")).count();
    let approvals = Approvals { given: given as u32, left: 0 };
    let review = if participants.iter().any(|p| p.state.as_deref() == Some("changes_requested")) {
        Review::ChangesRequested
    } else if given > 0 {
        Review::Approved
    } else {
        Review::Unreviewed
    };
    (review, Some(approvals))
}

/// One page of a list.
#[derive(Deserialize)]
struct Page<T> {
    values: Vec<T>,
    /// The next page's address, when there is one.
    next: Option<String>,
}

// `..Default::default()` keeps fields added to `PullRequest` later at "unknown".
#[allow(clippy::needless_update, reason = "PullRequest gains fields this backend leaves unknown")]
fn pull_request(pull: RawPull) -> PullRequest {
    let (review, approvals) = review(&pull.participants);
    PullRequest {
        number: pull.id,
        title: pull.title,
        author: account_name(pull.author),
        draft: pull.draft,
        head_ref: pull.source.branch.name,
        head_sha: pull.source.commit.map(|commit| commit.hash).unwrap_or_default(),
        head_repo: pull.source.repository.and_then(|repo| repo.full_name),
        base_ref: pull.destination.branch.name,
        updated: timestamp(pull.updated_on.as_deref()),
        review,
        approvals,
        ..Default::default()
    }
}

/// The pull requests, and whether there are more.
pub fn parse_pulls(text: &str) -> Result<(Vec<PullRequest>, bool), String> {
    let page: Page<RawPull> = json(text)?;
    Ok((page.values.into_iter().map(pull_request).collect(), page.next.is_some()))
}

/// One pull request, as the API answers its creation.
pub fn parse_pull(text: &str) -> Result<PullRequest, String> {
    json::<RawPull>(text).map(pull_request)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawState {
    /// PENDING, IN_PROGRESS, COMPLETED.
    name: String,
    /// Once completed: SUCCESSFUL, FAILED, ERROR, STOPPED, EXPIRED.
    result: Option<RawName>,
    /// While in progress: RUNNING, PAUSED (a manual step), HALTED.
    stage: Option<RawName>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawSelector {
    #[serde(rename = "type")]
    kind: String,
    pattern: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTarget {
    /// `pipeline_ref_target` or `pipeline_pullrequest_target`.
    #[serde(rename = "type")]
    kind: String,
    ref_name: Option<String>,
    /// A pull request pipeline's source branch.
    source: Option<String>,
    commit: Option<RawCommit>,
    selector: Option<RawSelector>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTrigger {
    /// PUSH, MANUAL, SCHEDULE...
    name: Option<String>,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct RawPipeline {
    build_number: u64,
    #[serde(default)]
    state: RawState,
    #[serde(default)]
    target: RawTarget,
    trigger: Option<RawTrigger>,
    created_on: Option<String>,
}

/// A pipeline's state.
pub fn pipeline_state(state: &str, result: Option<&str>, stage: Option<&str>) -> RunState {
    match (state, result, stage) {
        ("COMPLETED", Some("SUCCESSFUL"), _) => RunState::Success,
        ("COMPLETED", Some("FAILED" | "ERROR"), _) => RunState::Failure,
        ("COMPLETED", Some("STOPPED"), _) => RunState::Cancelled,
        ("COMPLETED", _, _) => RunState::Skipped,
        // Waiting for someone to run a manual step, or stopped half-way.
        ("IN_PROGRESS", _, Some("PAUSED" | "HALTED")) => RunState::NeedsAction,
        ("IN_PROGRESS", _, _) => RunState::Running,
        // PENDING, PARSING and whatever comes next.
        _ => RunState::Queued,
    }
}

fn words(text: &str) -> String {
    text.to_lowercase().replace('_', " ")
}

/// The pipelines, and whether there are more. A pipeline's page is numbered
/// by its build number, which is therefore its [`Run::id`].
pub fn parse_pipelines(text: &str) -> Result<(Vec<Run>, bool), String> {
    let page: Page<RawPipeline> = json(text)?;
    let runs = page
        .values
        .into_iter()
        .map(|pipeline| {
            let RawState { name, result, stage } = pipeline.state;
            let (result, stage) = (result.map(|result| result.name), stage.map(|stage| stage.name));
            let state = pipeline_state(&name, result.as_deref(), stage.as_deref());
            let target = pipeline.target;
            let pull_request = target.kind == "pipeline_pullrequest_target";
            let workflow = match target.selector {
                Some(RawSelector { kind, pattern: Some(pattern) }) if kind == "custom" => pattern,
                _ => "Pipeline".to_owned(),
            };
            let event = match pipeline.trigger {
                _ if pull_request => "pull request".to_owned(),
                Some(RawTrigger { name: Some(name), .. }) => words(&name),
                Some(RawTrigger { kind, .. }) => words(kind.strip_prefix("pipeline_trigger_").unwrap_or(&kind)),
                None => String::new(),
            };
            let outcome = match (result, stage) {
                (Some(result), _) => words(&result),
                (None, Some(stage)) if state == RunState::NeedsAction => words(&stage),
                _ => words(&name),
            };
            Run {
                id: pipeline.build_number,
                workflow,
                title: String::new(),
                number: pipeline.build_number,
                branch: target.ref_name.or(target.source).unwrap_or_default(),
                event,
                head_sha: target.commit.map(|commit| commit.hash).unwrap_or_default(),
                state,
                outcome,
                when: timestamp(pipeline.created_on.as_deref()),
            }
        })
        .collect();
    Ok((runs, page.next.is_some()))
}

/// What an error body says: `{"type": "error", "error": {"message": ...,
/// "detail": ..., "fields": {"source": ["..."]}}}`. The fields' complaints
/// are the specific part when there are any.
pub fn api_message(body: &str) -> String {
    use serde_json::Value;
    let Ok(Value::Object(top)) = serde_json::from_str::<Value>(body) else { return String::new() };
    let Some(Value::Object(error)) = top.get("error") else { return String::new() };
    let text = |value: Option<&Value>| value.and_then(Value::as_str).unwrap_or_default().trim().to_owned();
    let complaints: Vec<String> = match error.get("fields") {
        Some(Value::Object(fields)) => fields
            .values()
            .flat_map(|complaint| match complaint {
                Value::Array(items) => items.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                Value::String(text) => vec![text.clone()],
                _ => Vec::new(),
            })
            .filter(|text| !text.trim().is_empty())
            .collect(),
        _ => Vec::new(),
    };
    if !complaints.is_empty() {
        return complaints.join("; ");
    }
    let (message, detail) = (text(error.get("message")), text(error.get("detail")));
    match (message.is_empty(), detail.is_empty() || detail == message) {
        (_, true) => message,
        (true, false) => detail,
        (false, false) => format!("{message} ({detail})"),
    }
}

/// The body of a new pull request.
pub fn new_pull_json(new: &NewPull) -> String {
    pull_json(new, None)
}

/// [`new_pull_json`], from a branch of `from` (a fork) when given: the
/// source's `repository`, by its `workspace/slug`.
fn pull_json(new: &NewPull, from: Option<&RepoId>) -> String {
    let mut json = serde_json::json!({
        "title": new.title.trim(),
        "description": new.body,
        "source": {"branch": {"name": new.head}},
        "destination": {"branch": {"name": new.base}},
        "draft": new.draft,
    });
    if let Some(from) = from {
        json["source"]["repository"] = serde_json::json!({ "full_name": from.slug() });
    }
    json.to_string()
}

/// The parent of `repo` in the answer to `GET /repositories/{workspace}/
/// {repo_slug}`: `None` when it is not a fork, or its parent's name is not
/// one a remote could have. Its main branch, when the answer has it.
pub fn parse_parent(text: &str, repo: &RepoId) -> Result<Option<Parent>, String> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct RawParent {
        full_name: String,
        mainbranch: Option<RawName>,
    }
    #[derive(Deserialize)]
    struct RawRepo {
        parent: Option<RawParent>,
    }
    let raw: RawRepo = json(text)?;
    Ok(raw.parent.and_then(|parent| {
        let default_branch = branch_name(&parent.mainbranch.map(|branch| branch.name).unwrap_or_default());
        Some(Parent { repo: repo.sibling(&parent.full_name)?, default_branch })
    }))
}

// ---- the account (Preferences > Integrations) ----

/// `GET /user`: the account, and the id its SSH keys are listed under (its
/// `uuid`; Bitbucket no longer takes user names in addresses).
pub fn parse_user(text: &str) -> Result<(Profile, String), String> {
    #[derive(Deserialize)]
    struct Raw {
        username: Option<String>,
        nickname: Option<String>,
        display_name: Option<String>,
        uuid: Option<String>,
        account_id: Option<String>,
    }
    let raw: Raw = json(text)?;
    let present = |text: Option<String>| text.map(|text| tidy(&text)).filter(|text| !text.is_empty());
    let login = present(raw.username).or_else(|| present(raw.nickname)).ok_or("the answer names no account")?;
    let owner = present(raw.uuid).or_else(|| present(raw.account_id)).ok_or("the answer names no account")?;
    let profile = Profile { id: 0, login, name: present(raw.display_name).unwrap_or_default(), email: None };
    Ok((profile, owner))
}

/// Where the SSH keys of the account `owner` (see [`parse_user`]) are.
pub fn keys_url(owner: &str) -> String {
    format!("{API}/users/{}/ssh-keys", url_escape(owner))
}

#[derive(Deserialize)]
struct RawKey {
    #[serde(default)]
    key: String,
    label: Option<String>,
    comment: Option<String>,
    created_on: Option<String>,
    expires_on: Option<String>,
}

fn key_model(raw: RawKey) -> SshKey {
    let parts: Vec<&str> = raw.key.split_whitespace().take(2).collect();
    let title = raw.label.filter(|label| !label.trim().is_empty()).or(raw.comment).unwrap_or_default();
    SshKey {
        // Bitbucket names keys by a uuid, which nothing here needs.
        id: 0,
        title: tidy(&title),
        key: parts.join(" "),
        fingerprint: parts.get(1).and_then(|blob| account::fingerprint(blob)).unwrap_or_default(),
        created: timestamp(raw.created_on.as_deref()),
        expires: timestamp(raw.expires_on.as_deref()),
    }
}

/// A page of `GET /users/{owner}/ssh-keys`.
pub fn parse_keys(text: &str) -> Result<Vec<SshKey>, String> {
    Ok(json::<Page<RawKey>>(text)?.values.into_iter().map(key_model).collect())
}

/// The key a `POST /users/{owner}/ssh-keys` answers with.
pub fn parse_key(text: &str) -> Result<SshKey, String> {
    json::<RawKey>(text).map(key_model)
}

/// The body of `POST /users/{owner}/ssh-keys`.
pub fn new_key_json(title: &str, key: &PublicKey) -> String {
    serde_json::json!({ "key": key.line(), "label": title }).to_string()
}

// ---- the client ----

#[derive(Clone, Copy)]
/// The lists asked for, each with its own validator.
enum List {
    Pulls,
    Pipelines,
}

pub struct Client {
    api: Api,
    /// The repository's parent once asked; `Some(None)` when it has none.
    parent: Option<Option<Parent>>,
}

impl Client {
    pub fn new(repo: RepoId, transport: Box<dyn Transport>, secrets: Box<dyn Secrets>) -> Self {
        Client { api: Api::new(repo, transport, secrets, 2, &[401], api_message), parent: None }
    }

    fn repo_url(&self, path: &str) -> String {
        Self::url_of(&self.api.base, &self.api.repo, path)
    }

    fn url_of(base: &str, repo: &RepoId, path: &str) -> String {
        format!("{base}/repositories/{}/{}/{path}", repo.owner, repo.name)
    }

    /// POST the new pull request `body` to the pull requests of `into`:
    /// this repository, or a fork's parent.
    fn create(&mut self, into: &RepoId, new: &NewPull, body: &str, now: u64) -> Result<PullRequest, Problem> {
        self.api.look_again();
        let url = Self::url_of(&self.api.base, into, "pullrequests");
        let response = self.api.post(&url, body, true, now)?;
        let message = self.api.message(&response.body);
        let lower = message.to_lowercase();
        let (head, base) = (&new.head, &new.base);
        match response.status {
            200 | 201 => parse_pull(&response.body).map_err(|error| self.api.unreadable(error)),
            404 => Err(Problem::NoAccess),
            400 | 409 | 422 if lower.contains("no changes") => {
                Err(Problem::Refused(format!("No commits between {base} and {head}: there is nothing to merge")))
            }
            400 | 409 | 422 if lower.contains("not found") || lower.contains("does not exist") => {
                Err(Problem::Refused(format!("Bitbucket does not have the branch {head} or {base}: push it first")))
            }
            400 | 409 | 422 if !message.is_empty() => Err(Problem::Refused(message)),
            403 => Err(Problem::Refused(match message.is_empty() {
                true => format!("No permission to open pull requests in {}", into.slug()),
                false => format!("No permission to open pull requests in {} ({message})", into.slug()),
            })),
            status => Err(Problem::Failed(format!("Bitbucket answered {status}"))),
        }
    }

    /// GET the repository itself, for the parent of a fork. `None` when
    /// there was no answer worth keeping (no connection, a rate limit, the
    /// server failing): asked again next round.
    fn ask_parent(&mut self, now: u64) -> Option<Option<Parent>> {
        let url = self.repo_url("").trim_end_matches('/').to_owned();
        let response = self.api.get(url, None, now).ok()?;
        match response.status {
            200..=299 => Some(parse_parent(&response.body, &self.api.repo).unwrap_or(None)),
            500..=599 => None,
            // Moved, hidden or refused: no parent to offer.
            _ => Some(None),
        }
    }
}

impl Backend for Client {
    fn pulls(&mut self, now: u64) -> Outcome<PullRequest> {
        self.api.start_round();
        // `state=OPEN` alone leaves drafts out; the query brings them in.
        let query = url_escape("state=\"OPEN\" AND (draft=true OR draft=false)");
        // The participants say who approved; the list leaves them out unless asked.
        let url = self.repo_url(&format!(
            "pullrequests?q={query}&sort=-updated_on&pagelen=30&fields=%2Bvalues.participants"
        ));
        self.api.fetch(url, List::Pulls as usize, now, |_, _| None, |response| parse_pulls(&response.body))
    }

    /// Bitbucket Cloud has no issue tracker any more: nothing to ask.
    fn issues(&mut self, _now: u64) -> Outcome<Issue> {
        Outcome::Fresh { items: Vec::new(), more: false }
    }

    fn runs(&mut self, now: u64) -> Outcome<Run> {
        let url = self.repo_url("pipelines?sort=-created_on&pagelen=20");
        // With a sign-in, "not found" means Pipelines is not set up here;
        // access itself shows in the pull request list.
        let special = |response: &HttpResponse, signed_in: bool| (signed_in && response.status == 404).then_some(Ok(Got::Nothing));
        self.api.fetch(url, List::Pipelines as usize, now, special, |response| parse_pipelines(&response.body))
    }

    fn auth(&self) -> Auth {
        self.api.auth()
    }

    fn look_again(&mut self) {
        self.api.look_again();
    }

    fn blocked_until(&self, now: u64) -> Option<u64> {
        self.api.blocked_until(now)
    }

    fn create_pull(&mut self, new: &NewPull, now: u64) -> Result<PullRequest, Problem> {
        let into = self.api.repo.clone();
        let created = self.create(&into, new, &new_pull_json(new), now);
        created.map_err(|problem| self.api.scrub(problem))
    }

    /// Opened in `across.into`, from a branch of `across.from` (named as the
    /// source's repository when it is another one).
    fn create_pull_across(&mut self, new: &NewPull, across: &Across, now: u64) -> Result<PullRequest, Problem> {
        let here = |repo: &RepoId| repo.service == self.api.repo.service && repo.host == self.api.repo.host;
        if !here(&across.from) || !here(&across.into) {
            return Err(Problem::Failed(format!("Both repositories must be on {}", self.api.repo.site())));
        }
        let from = (!across.from.same(&across.into)).then_some(&across.from);
        let created = self.create(&across.into, new, &pull_json(new, from), now);
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

    fn repo() -> RepoId {
        parse_remote("git@bitbucket.org:team/app.git", &BTreeMap::new()).unwrap()
    }

    fn scripted(replies: Vec<Result<HttpResponse, String>>, secrets: Fake) -> (Client, Log) {
        let sent = Log::default();
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        (Client::new(repo(), Box::new(transport), Box::new(secrets)), sent)
    }

    fn api_token() -> Fake {
        Fake { env: vec![("BITBUCKET_EMAIL", "ada@example.com"), ("BITBUCKET_API_TOKEN", SECRET)], ..Default::default() }
    }

    fn failure<T: std::fmt::Debug>(outcome: Outcome<T>) -> Problem {
        match outcome {
            Outcome::Failed(problem) => problem,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// The `Authorization` header a token would be sent with.
    fn header(token: &Token) -> String {
        token.authorization()
    }

    // Trimmed-down answers, after Bitbucket's API reference.
    const PULLS: &str = r#"{"pagelen": 30, "size": 31, "page": 1,
      "next": "https://api.bitbucket.org/2.0/repositories/team/app/pullrequests?state=OPEN&page=2",
      "values": [
      {"type": "pullrequest", "id": 12, "title": "Add dark mode", "state": "OPEN", "draft": false,
       "author": {"type": "user", "display_name": "Ada Lovelace", "nickname": "ada", "account_id": "557058:1"},
       "source": {"branch": {"name": "feature/dark"}, "commit": {"type": "commit", "hash": "111111111111"},
                  "repository": {"type": "repository", "full_name": "team/app", "name": "app"}},
       "destination": {"branch": {"name": "main"}, "commit": {"hash": "000000000000"},
                       "repository": {"full_name": "team/app"}},
       "comment_count": 2, "task_count": 0, "created_on": "1970-01-01T12:00:00.000000+00:00",
       "updated_on": "1970-01-02T00:00:00.000000+00:00",
       "links": {"html": {"href": "https://bitbucket.org/team/app/pull-requests/12"}}},
      {"type": "pullrequest", "id": 9, "title": "Parser", "draft": true, "author": {"type": "user", "display_name": "Former user"},
       "source": {"branch": {"name": "parser"}, "commit": {"hash": "222222222222"}, "repository": {"full_name": "grace/app-fork"}},
       "destination": {"branch": {"name": "main"}}}
    ]}"#;

    /// `GET /repositories/{workspace}/{repo_slug}` of a fork, trimmed.
    const FORK: &str = r#"{"type": "repository", "full_name": "ada/app", "name": "app", "is_private": false,
      "mainbranch": {"type": "branch", "name": "main"}, "fork_policy": "allow_forks",
      "parent": {"type": "repository", "full_name": "team/app", "name": "app", "uuid": "{7e1c}",
                 "links": {"html": {"href": "https://bitbucket.org/team/app"}}}}"#;

    const PIPELINES: &str = r#"{"page": 1, "pagelen": 20, "size": 3, "values": [
      {"type": "pipeline", "uuid": "{a1}", "build_number": 140,
       "state": {"type": "pipeline_state_completed", "name": "COMPLETED",
                 "result": {"type": "pipeline_state_completed_successful", "name": "SUCCESSFUL"}},
       "target": {"type": "pipeline_pullrequest_target", "source": "feature/dark", "destination": "main",
                  "commit": {"type": "commit", "hash": "1111111111111111111111111111111111111111"},
                  "pullrequest": {"id": 12}, "selector": {"type": "pull-requests", "pattern": "**"}},
       "trigger": {"name": "PUSH", "type": "pipeline_trigger_push"}, "created_on": "1970-01-01T01:00:00.000Z"},
      {"type": "pipeline", "build_number": 139,
       "state": {"name": "IN_PROGRESS", "stage": {"name": "PAUSED"}},
       "target": {"type": "pipeline_ref_target", "ref_type": "branch", "ref_name": "main",
                  "commit": {"hash": "4444444444444444444444444444444444444444"}, "selector": {"type": "custom", "pattern": "deploy"}},
       "trigger": {"type": "pipeline_trigger_manual"}},
      {"type": "pipeline", "build_number": 138, "state": {"name": "PENDING"},
       "target": {"type": "pipeline_ref_target", "ref_name": "v1.2"}}
    ]}"#;

    #[test]
    fn remotes_on_bitbucket_org_are_recognised() {
        for url in [
            "https://bitbucket.org/team/app.git",
            "https://ada@bitbucket.org/team/app.git",
            "https://x-token-auth:tok@bitbucket.org/team/app",
            "https://www.bitbucket.org/team/app/",
            "git@bitbucket.org:team/app.git",
            "bitbucket.org:team/app.git",
            "ssh://git@bitbucket.org/team/app.git",
            "ssh://git@altssh.bitbucket.org:443/team/app.git",
        ] {
            assert_eq!(parse_remote(url, &BTreeMap::new()), Some(repo()), "{url}");
        }
        let repo = repo();
        assert_eq!((repo.service, repo.host.as_str(), repo.slug().as_str()), (Service::Bitbucket, "bitbucket.org", "team/app"));
        assert!(!repo.self_hosted());
        assert_eq!((repo.site(), repo.api_base()), ("Bitbucket".to_owned(), API.to_owned()));
        for url in [
            "https://bitbucket.org/team",
            "https://bitbucket.org/team/app/src/main",
            "https://bitbucket.org.evil.example/team/app.git",
            "https://api.bitbucket.org/2.0/repositories",
            "git@bitbucket.org.evil.example:team/app.git",
        ] {
            assert_eq!(parse_remote(url, &BTreeMap::new()), None, "{url}");
        }
        assert!(offer_for(&RemoteInfo { name: "origin".into(), url: "https://api.bitbucket.org/x/y".into() }).is_none());
    }

    #[test]
    fn links_and_words_follow_bitbuckets() {
        let repo = repo();
        assert_eq!(repo.pulls_url(), "https://bitbucket.org/team/app/pull-requests");
        assert_eq!(repo.pull_url(12), "https://bitbucket.org/team/app/pull-requests/12");
        assert_eq!(repo.issues_url(), "https://bitbucket.org/team/app/issues");
        assert_eq!(repo.issue_url(31), "https://bitbucket.org/team/app/issues/31");
        assert_eq!(repo.runs_url(), "https://bitbucket.org/team/app/pipelines");
        assert_eq!(repo.run_url(140), "https://bitbucket.org/team/app/pipelines/results/140");
        assert_eq!(
            repo.compare_url("main", "feature/dark mode"),
            "https://bitbucket.org/team/app/pull-requests/new?source=feature/dark%20mode&dest=main"
        );
        let service = Service::Bitbucket;
        assert_eq!((service.pulls_title(), service.ci_title(), service.pull_number(12)), ("PULL REQUESTS", "PIPELINES", "#12".to_owned()));
        assert!(!service.has_pull_refs() && service.cli_tool().is_none());
    }

    #[test]
    fn pull_requests_map_to_the_model_and_match_their_pipelines_by_short_hash() {
        let (pulls, more) = parse_pulls(PULLS).unwrap();
        assert!(more);
        let first = &pulls[0];
        assert_eq!((first.number, first.title.as_str(), first.author.as_str()), (12, "Add dark mode", "ada"));
        assert_eq!((first.head_ref.as_str(), first.head_sha.as_str(), first.base_ref.as_str()), ("feature/dark", "111111111111", "main"));
        assert_eq!(first.updated, 86_400);
        assert!(first.is_from(&repo()) && !first.draft);
        assert!(pulls[1].draft && pulls[1].author == "Former user" && !pulls[1].is_from(&repo()));

        let (runs, _) = parse_pipelines(PIPELINES).unwrap();
        assert_eq!(checks_for(&first.head_sha, &runs), Some(Checks::Passing));
        assert_eq!(checks_for("1111111", &runs), None);
        assert!(parse_pulls(r#"[]"#).is_err());
    }

    #[test]
    fn participants_become_the_review_state() {
        let state = |participants: &str| {
            let text = format!(r#"{{"values": [{{"id": 1, "participants": [{participants}]}}]}}"#);
            let pull = parse_pulls(&text).unwrap().0.remove(0);
            (pull.review, pull.approvals.unwrap().given)
        };
        assert_eq!(state(""), (Review::Unreviewed, 0));
        assert_eq!(state(r#"{"role": "REVIEWER", "approved": false}"#), (Review::Unreviewed, 0));
        assert_eq!(state(r#"{"approved": true, "state": "approved"}, {"approved": false}"#), (Review::Approved, 1));
        let both = r#"{"approved": true, "state": "approved"}, {"approved": false, "state": "changes_requested"}"#;
        assert_eq!(state(both), (Review::ChangesRequested, 1));
    }

    #[test]
    fn pipelines_map_to_the_model() {
        let (runs, more) = parse_pipelines(PIPELINES).unwrap();
        assert!(!more);
        let first = &runs[0];
        assert_eq!((first.id, first.number, first.workflow.as_str()), (140, 140, "Pipeline"));
        assert_eq!((first.branch.as_str(), first.event.as_str(), first.when), ("feature/dark", "pull request", 3600));
        assert_eq!((first.state, first.outcome.as_str()), (RunState::Success, "successful"));
        assert_eq!((runs[1].workflow.as_str(), runs[1].state, runs[1].outcome.as_str()), ("deploy", RunState::NeedsAction, "paused"));
        assert_eq!((runs[1].event.as_str(), runs[1].branch.as_str()), ("manual", "main"));
        assert_eq!((runs[2].state, runs[2].outcome.as_str(), runs[2].head_sha.as_str()), (RunState::Queued, "pending", ""));

        for (state, result, stage, expected) in [
            ("COMPLETED", Some("SUCCESSFUL"), None, RunState::Success),
            ("COMPLETED", Some("FAILED"), None, RunState::Failure),
            ("COMPLETED", Some("ERROR"), None, RunState::Failure),
            ("COMPLETED", Some("STOPPED"), None, RunState::Cancelled),
            ("COMPLETED", Some("EXPIRED"), None, RunState::Skipped),
            ("IN_PROGRESS", None, Some("RUNNING"), RunState::Running),
            ("IN_PROGRESS", None, Some("HALTED"), RunState::NeedsAction),
            ("PENDING", None, None, RunState::Queued),
            ("SOMETHING_NEW", None, None, RunState::Queued),
        ] {
            assert_eq!(pipeline_state(state, result, stage), expected, "{state} {result:?} {stage:?}");
        }
    }

    #[test]
    fn error_bodies_are_read_in_bitbuckets_shape() {
        assert_eq!(api_message(r#"{"type": "error", "error": {"message": "Repository not found"}}"#), "Repository not found");
        assert_eq!(
            api_message(r#"{"type": "error", "error": {"message": "Bad request", "fields": {"source": ["There are no changes to be pulled"]}}}"#),
            "There are no changes to be pulled"
        );
        assert_eq!(
            api_message(r#"{"type": "error", "error": {"message": "Your credentials lack one or more required privilege scopes.", "detail": {"granted": [], "required": ["pipeline"]}}}"#),
            "Your credentials lack one or more required privilege scopes."
        );
        assert_eq!(api_message(r#"{"type": "error", "error": {"message": "Forbidden", "detail": "Not your repository"}}"#), "Forbidden (Not your repository)");
        assert_eq!(api_message("<html>"), "");
        assert_eq!(api_message(r#"{"message": "x"}"#), "");
    }

    #[test]
    fn sign_ins_are_basic_with_the_user_or_bearer_for_access_tokens() {
        let repo = repo();
        // An API token goes with the e-mail address, as Basic.
        let (token, source) = find_token(&api_token(), &repo, &[]).unwrap();
        assert_eq!(source, TokenSource::Env("BITBUCKET_API_TOKEN"));
        assert_eq!(header(&token), format!("Basic {}", base64_padded(format!("ada@example.com:{SECRET}").as_bytes())));
        // Without a user to go with it, an API token cannot be used.
        let alone = Fake { env: vec![("BITBUCKET_API_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(find_token(&alone, &repo, &[]), None);
        // An app password goes with the user name.
        let legacy = Fake { env: vec![("BITBUCKET_USERNAME", "ada"), ("BITBUCKET_APP_PASSWORD", SECRET)], ..Default::default() };
        let (token, source) = find_token(&legacy, &repo, &[]).unwrap();
        assert_eq!((source, header(&token)), (TokenSource::Env("BITBUCKET_APP_PASSWORD"), format!("Basic {}", base64_padded(format!("ada:{SECRET}").as_bytes()))));
        // An access token is a bearer token, and comes first.
        let access = Fake { env: vec![("BITBUCKET_ACCESS_TOKEN", OTHER), ("BITBUCKET_EMAIL", "a@b.c"), ("BITBUCKET_API_TOKEN", SECRET)], ..Default::default() };
        let (token, source) = find_token(&access, &repo, &[]).unwrap();
        assert_eq!((source, header(&token)), (TokenSource::Env("BITBUCKET_ACCESS_TOKEN"), format!("Bearer {OTHER}")));
        // Other services' variables are never read.
        let github = Fake { env: vec![("GITHUB_TOKEN", SECRET), ("GITLAB_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(find_token(&github, &repo, &[]), None);
        assert!(github.asked().iter().all(|asked| asked.starts_with("BITBUCKET_") || asked == "git bitbucket.org"));
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
            assert_eq!((host, path), ("bitbucket.org", ""));
            Some((self.0.to_owned(), self.1.to_owned()))
        }
    }

    #[test]
    fn the_credential_helpers_password_is_tried_as_basic_then_as_bearer() {
        let repo = repo();
        let helper = Helper("ada", SECRET);
        let (basic, source) = find_token(&helper, &repo, &[]).unwrap();
        assert_eq!(source, TokenSource::CredentialHelper);
        assert!(header(&basic).starts_with("Basic "));
        // Refused as Basic: the same secret as an OAuth token.
        let (bearer, _) = find_token(&helper, &repo, std::slice::from_ref(&basic)).unwrap();
        assert_eq!(header(&bearer), format!("Bearer {SECRET}"));
        assert_eq!(find_token(&helper, &repo, &[basic, bearer]), None);
        // git's name for a token is no user name.
        let (token, _) = find_token(&Helper("x-token-auth", SECRET), &repo, &[]).unwrap();
        assert_eq!(header(&token), format!("Bearer {SECRET}"));
        // A user name Basic cannot carry is passed over.
        let (token, _) = find_token(&Helper("ada:evil", SECRET), &repo, &[]).unwrap();
        assert_eq!(header(&token), format!("Bearer {SECRET}"));
    }

    #[test]
    fn lists_are_asked_for_below_the_api_and_issues_not_at_all() {
        let (mut client, sent) = scripted(vec![reply(200, &[], PULLS), reply(200, &[], PIPELINES)], api_token());
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, more: true } if items.len() == 2));
        // The issue tracker is gone from Bitbucket Cloud: an empty list,
        // and no request for it.
        assert_eq!(client.issues(NOW), Outcome::Fresh { items: Vec::new(), more: false });
        assert!(!Service::Bitbucket.has_issues() && Service::GitHub.has_issues() && Service::Codeberg.has_issues());
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, .. } if items.len() == 3));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("BITBUCKET_API_TOKEN")));
        let sent = sent.lock().unwrap();
        let urls: Vec<&str> = sent.iter().map(|request| request.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://api.bitbucket.org/2.0/repositories/team/app/pullrequests?q=state%3D%22OPEN%22%20AND%20%28draft%3Dtrue%20OR%20draft%3Dfalse%29&sort=-updated_on&pagelen=30&fields=%2Bvalues.participants",
                "https://api.bitbucket.org/2.0/repositories/team/app/pipelines?sort=-created_on&pagelen=20",
            ]
        );
        assert!(sent.iter().all(|request| within(API, &request.url) && request.token.as_deref() == Some(SECRET)));
    }

    #[test]
    fn a_forks_parent_is_asked_once_and_taken_only_with_a_name_a_remote_could_have() {
        let fork = RepoId { owner: "ada".to_owned(), ..repo() };
        let sent = Log::default();
        let transport = Script { replies: vec![reply(500, &[], ""), reply(200, &[], FORK)].into(), sent: sent.clone() };
        let mut client = Client::new(fork.clone(), Box::new(transport), Box::new(Fake::default()));
        // The server failed: nothing learnt, asked again next round.
        assert_eq!(client.details(NOW), None);
        let parent = client.details(NOW).unwrap().parent.unwrap();
        assert_eq!((parent.repo, parent.default_branch.as_str()), (repo(), ""));
        assert_eq!(client.details(NOW), None);
        let urls: Vec<String> = sent.lock().unwrap().iter().map(|request| request.url.clone()).collect();
        assert_eq!(urls, ["https://api.bitbucket.org/2.0/repositories/ada/app", "https://api.bitbucket.org/2.0/repositories/ada/app"]);

        let named = r#"{"parent": {"full_name": "team/app", "mainbranch": {"name": "trunk"}}}"#;
        assert_eq!(parse_parent(named, &fork).unwrap().unwrap().default_branch, "trunk");
        assert_eq!(parse_parent(r#"{"full_name": "team/app", "parent": null}"#, &fork).unwrap(), None);
        assert_eq!(parse_parent(r#"{"parent": {"full_name": "../x"}}"#, &fork).unwrap(), None);
        assert!(parse_parent("[]", &fork).is_err());
    }

    #[test]
    fn a_pull_request_from_a_fork_names_it_as_its_sources_repository() {
        let fork = RepoId { owner: "ada".to_owned(), ..repo() };
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], api_token());
        let across = Across { from: fork.clone(), into: repo() };
        assert_eq!(client.create_pull_across(&new_pull(false), &across, NOW).unwrap().number, 13);
        let sent = sent.lock().unwrap();
        assert_eq!(sent[0].url, "https://api.bitbucket.org/2.0/repositories/team/app/pullrequests");
        let body: serde_json::Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["source"], serde_json::json!({"branch": {"name": "feature/fast"}, "repository": {"full_name": "ada/app"}}));
        assert_eq!(body["destination"], serde_json::json!({"branch": {"name": "main"}}));
        drop(sent);

        // Within one repository: no repository named. Never elsewhere.
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], api_token());
        client.create_pull_across(&new_pull(false), &Across { from: repo(), into: repo() }, NOW).unwrap();
        let body: serde_json::Value = serde_json::from_str(sent.lock().unwrap()[0].body.as_deref().unwrap()).unwrap();
        assert!(body["source"].get("repository").is_none());
        let github = RepoId { service: Service::GitHub, host: "github.com".to_owned(), ..repo() };
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], api_token());
        assert!(client.create_pull_across(&new_pull(false), &Across { from: fork, into: github }, NOW).is_err());
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn statuses_map_to_problems_in_bitbuckets_words() {
        let missing = r#"{"type": "error", "error": {"message": "Repository team/app not found"}}"#;
        let (mut client, _) = scripted(vec![reply(404, &[], missing), reply(403, &[], "{}")], Fake::default());
        assert_eq!(failure(client.pulls(NOW)), Problem::NotSignedIn);
        assert_eq!(failure(client.runs(NOW)), Problem::NotSignedIn);
        let detail = Problem::NotSignedIn.detail(Service::Bitbucket);
        assert!(detail.contains("BITBUCKET_API_TOKEN") && detail.contains("BITBUCKET_EMAIL"), "{detail}");

        let (mut client, _) = scripted(vec![reply(404, &[], missing), reply(404, &[], "{}")], api_token());
        assert_eq!(failure(client.pulls(NOW)), Problem::NoAccess);
        // Pipelines not set up: nothing to show.
        assert_eq!(client.runs(NOW), Outcome::Fresh { items: Vec::new(), more: false });

        let scope = r#"{"type": "error", "error": {"message": "Your credentials lack one or more required privilege scopes."}}"#;
        let (mut client, _) = scripted(vec![reply(403, &[], scope)], api_token());
        assert_eq!(failure(client.runs(NOW)).line(Service::Bitbucket), "Bitbucket refused: Your credentials lack one or more required privilege scopes.");

        // A refused sign-in falls back to the next, then to none.
        let (mut client, sent) = scripted(vec![reply(401, &[], "{}"), reply(404, &[], missing)], api_token());
        assert_eq!(failure(client.pulls(NOW)), Problem::SignInRejected);
        assert_eq!((client.auth(), sent.lock().unwrap()[1].token.clone()), (Auth::Rejected, None));

        // Bitbucket sends no hint with a 429: back off.
        let (mut client, sent) = scripted(vec![reply(429, &[], "Rate limit for this resource has been exceeded")], api_token());
        assert_eq!(failure(client.pulls(NOW)), Problem::RateLimited { until: NOW + 60 });
        assert_eq!(failure(client.runs(NOW + 30)), Problem::RateLimited { until: NOW + 60 });
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_token_never_reaches_a_displayed_error() {
        let basic = base64_padded(format!("ada@example.com:{SECRET}").as_bytes());
        let echo = format!(r#"{{"type": "error", "error": {{"message": "bad {SECRET} and {basic}"}}}}"#);
        for replies in [
            vec![reply(403, &[], &echo)],
            vec![reply(500, &[], &echo)],
            vec![Err(format!("proxy said Authorization: Basic {basic}"))],
            vec![reply(200, &[], &format!("{{\"values\": \"{SECRET}\"}}"))],
        ] {
            let (mut client, _) = scripted(replies, api_token());
            let problem = failure(client.pulls(NOW));
            for text in [problem.line(Service::Bitbucket), problem.detail(Service::Bitbucket), format!("{problem:?}")] {
                assert!(!text.contains(SECRET) && !text.contains(&basic), "{text}");
            }
        }
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

    const CREATED: &str = r#"{"type": "pullrequest", "id": 13, "title": "Speed up the graph", "draft": true,
       "author": {"nickname": "ada"}, "source": {"branch": {"name": "feature/fast"}, "commit": {"hash": "555555555555"},
       "repository": {"full_name": "team/app"}}, "destination": {"branch": {"name": "main"}},
       "updated_on": "1970-01-03T00:00:00+00:00"}"#;

    #[test]
    fn a_pull_request_is_opened_with_one_post() {
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], api_token());
        let created = client.create_pull(&new_pull(true), NOW).unwrap();
        assert!(created.draft && created.number == 13 && created.is_from(&repo()));
        let sent = sent.lock().unwrap();
        assert_eq!((sent.len(), sent[0].url.as_str()), (1, "https://api.bitbucket.org/2.0/repositories/team/app/pullrequests"));
        let body: serde_json::Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "title": "Speed up the graph",
                "description": "Caches lane colours.",
                "source": {"branch": {"name": "feature/fast"}},
                "destination": {"branch": {"name": "main"}},
                "draft": true,
            })
        );
    }

    #[test]
    fn opening_a_pull_request_says_why_it_failed() {
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], Fake::default());
        assert_eq!(client.create_pull(&new_pull(false), NOW), Err(Problem::NotSignedIn));
        assert!(sent.lock().unwrap().is_empty());

        let no_changes = r#"{"type": "error", "error": {"message": "Bad request", "fields": {"source": ["There are no changes to be pulled"]}}}"#;
        let no_branch = r#"{"type": "error", "error": {"message": "Bad request", "fields": {"source": ["Branch not found: feature/fast"]}}}"#;
        let open = r#"{"type": "error", "error": {"message": "There is already an open pull request from feature/fast to main"}}"#;
        for (status, body, expected) in [
            (400, no_changes, Problem::Refused("No commits between main and feature/fast: there is nothing to merge".to_owned())),
            (400, no_branch, Problem::Refused("Bitbucket does not have the branch feature/fast or main: push it first".to_owned())),
            (400, open, Problem::Refused("There is already an open pull request from feature/fast to main".to_owned())),
            (404, "{}", Problem::NoAccess),
            (500, "", Problem::Failed("Bitbucket answered 500".to_owned())),
        ] {
            let (mut client, _) = scripted(vec![reply(status, &[], body)], api_token());
            assert_eq!(client.create_pull(&new_pull(false), NOW), Err(expected), "{status} {body}");
        }
    }

    #[test]
    fn a_pull_request_from_this_repository_is_checked_out_from_its_branch() {
        let (pulls, _) = parse_pulls(PULLS).unwrap();
        let cmd = checkout_pull_request("origin", Service::Bitbucket, &repo(), &pulls[0], false).unwrap();
        assert_eq!(cmd.label, "Check out pull request #12");
        // From a fork, there is nothing on origin to fetch.
        assert!(checkout_pull_request("origin", Service::Bitbucket, &repo(), &pulls[1], false).is_none());
        // A branch name that could be read as something else is not used.
        for name in ["a:b", "-x", "a..b", "a b", "x*", "refs/heads/../x", ""] {
            let odd = PullRequest { head_ref: name.to_owned(), ..pulls[0].clone() };
            assert!(checkout_pull_request("origin", Service::Bitbucket, &repo(), &odd, false).is_none(), "{name}");
        }
    }

    #[test]
    fn a_pull_request_is_checked_out_from_its_branch_in_a_real_repository() {
        use crate::tests::{Fixture, git};

        let fx = Fixture::new("hosting-bb-pr");
        let origin = fx.dir.with_file_name(format!("hosting-bb-pr-origin-{}.git", std::process::id()));
        let _ = std::fs::remove_dir_all(&origin);
        let origin_arg = origin.to_string_lossy().into_owned();
        fx.write("a.txt", "base\n");
        fx.commit_all("base");
        git(&fx.dir, &["clone", "-q", "--bare", ".", &origin_arg]);
        git(&fx.dir, &["remote", "add", "origin", &origin_arg]);

        // The author's branch, with a name that is not plain ASCII, exists
        // only on the remote.
        let branch = "feature/café";
        let publish = |from: &str, content: &str, force: bool| {
            git(&fx.dir, &["checkout", "-q", "--detach", from]);
            fx.write("pr file.txt", content);
            fx.commit_all("PR work");
            let spec = format!("{}HEAD:refs/heads/{branch}", if force { "+" } else { "" });
            git(&fx.dir, &["push", "-q", "origin", &spec]);
            let head = git(&fx.dir, &["rev-parse", "HEAD"]);
            git(&fx.dir, &["checkout", "-q", "main"]);
            head
        };
        let first = publish("main", "one\n", false);
        let pr = PullRequest { number: 5, head_ref: branch.to_owned(), head_repo: Some("team/app".to_owned()), ..Default::default() };
        // A name with a space is a valid branch, but not one taken from a
        // server's answer: it is refused before git sees it.
        let spaced = PullRequest { head_ref: "feature/café au lait".to_owned(), ..pr.clone() };
        assert!(checkout_pull_request("origin", Service::Bitbucket, &repo(), &spaced, false).is_none());
        let checkout = |on_branch: bool| checkout_pull_request("origin", Service::Bitbucket, &repo(), &pr, on_branch).unwrap();

        let done = fx.run(checkout(false));
        assert_eq!(done.snap.head.branch.as_deref(), Some("pr/5"));
        assert_eq!(done.snap.head.oid.unwrap().to_string(), first);
        assert_eq!(fx.read("pr file.txt"), "one\n");
        assert!(done.undo.is_some());

        // Nothing new: checking out again changes nothing and is no error.
        git(&fx.dir, &["checkout", "-q", "main"]);
        let again = fx.run(checkout(false));
        assert_eq!(again.snap.head.oid.unwrap().to_string(), first);

        // A new commit on the branch, while on pr/5: brought forward.
        let second = publish(&first, "one\ntwo\n", false);
        git(&fx.dir, &["checkout", "-q", "pr/5"]);
        let done = fx.run(checkout(true));
        assert_eq!(done.snap.head.oid.unwrap().to_string(), second);

        // Local work on pr/5 and a rewritten branch: git refuses, nothing is lost.
        fx.write("mine.txt", "local\n");
        fx.commit_all("mine");
        let mine = git(&fx.dir, &["rev-parse", "HEAD"]);
        publish("main", "rewritten\n", true);
        let refused = fx.run_as(checkout(false), crate::worker::Origin::User);
        assert!(refused.result.is_err());
        assert_eq!(git(&fx.dir, &["rev-parse", "pr/5"]), mine);

        // The branch is gone from the remote: an error, not a branch.
        git(&fx.dir, &["push", "-q", "origin", &format!(":refs/heads/{branch}")]);
        let gone = PullRequest { number: 6, ..pr.clone() };
        let missing = fx.run_as(
            checkout_pull_request("origin", Service::Bitbucket, &repo(), &gone, false).unwrap(),
            crate::worker::Origin::User,
        );
        assert!(missing.result.is_err());
        assert!(!missing.snap.branches.iter().any(|branch| branch.name == "pr/6"));

        drop(fx);
        let _ = std::fs::remove_dir_all(&origin);
    }

    #[test]
    fn the_account_and_its_keys_are_read_in_bitbuckets_shape() {
        let (profile, owner) = parse_user(
            r#"{"type": "user", "username": "ada", "display_name": "Ada Lovelace", "uuid": "{d3b07384-d9a0-4c3f-9a2b-1f2e3d4c5b6a}", "account_id": "557058:abc"}"#,
        )
        .unwrap();
        assert_eq!((profile.login.as_str(), profile.display_name(), profile.email.as_deref()), ("ada", "Ada Lovelace", None));
        assert_eq!(owner, "{d3b07384-d9a0-4c3f-9a2b-1f2e3d4c5b6a}");
        assert_eq!(keys_url(&owner), "https://api.bitbucket.org/2.0/users/%7Bd3b07384-d9a0-4c3f-9a2b-1f2e3d4c5b6a%7D/ssh-keys");
        assert!(parse_user(r#"{"display_name": "x"}"#).is_err());

        let keys = parse_keys(
            r#"{"pagelen": 10, "values": [{"type": "ssh_key", "uuid": "{k1}", "label": "Laptop",
                "key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ6/rvs4h30pDKGBvPxCCedyMBv2RjD2wg++vAYe/VwB ada@laptop",
                "comment": "ada@laptop", "created_on": "1970-01-02T00:00:00.000000+00:00"}]}"#,
        )
        .unwrap();
        assert_eq!((keys[0].title.as_str(), keys[0].created), ("Laptop", 86_400));
        assert_eq!(keys[0].fingerprint, "SHA256:rLmucD0ZT+3NWpg/ewTzpahexLO2rz/okUf2br8ey0Y");
        assert!(!keys[0].key.contains("ada@laptop"));
    }
}
