//! GitLab, on gitlab.com and on self-managed servers: the REST API (v4)
//! endpoints and their JSON.
//!
//! A project is addressed by its full path, URL-encoded
//! (`group%2Fsubgroup%2Fapp`), so nested groups need nothing special.
//! Merge requests and issues are numbered per project (`iid`); that is the
//! number the UI shows and the one in `refs/merge-requests/<iid>/head`.

use serde::Deserialize;

use super::*;

/// Sent with every request.
pub const HEADERS: &[(&str, &str)] = &[("Accept", "application/json")];

/// The project's id in API paths: its full path with each `/` encoded.
/// [`RepoId`] paths hold nothing else that would need it.
pub fn project_id(repo: &RepoId) -> String {
    repo.slug().replace('/', "%2F")
}

// ---- JSON ----

#[derive(Deserialize)]
struct RawUser {
    #[serde(default)]
    username: String,
}

/// GitLab credits a deleted account's work to its "ghost" user.
fn username(user: Option<RawUser>) -> String {
    user.map(|user| user.username).filter(|name| !name.is_empty()).unwrap_or_else(|| "ghost".to_owned())
}

#[derive(Deserialize)]
struct RawMergeRequest {
    iid: u64,
    #[serde(default)]
    title: String,
    /// GitLab 14 and later.
    #[serde(default)]
    draft: bool,
    /// What older servers call a draft.
    #[serde(default)]
    work_in_progress: bool,
    author: Option<RawUser>,
    #[serde(default)]
    source_branch: String,
    #[serde(default)]
    target_branch: String,
    /// The head commit.
    sha: Option<String>,
    source_project_id: Option<u64>,
    target_project_id: Option<u64>,
    updated_at: Option<String>,
}

impl RawMergeRequest {
    fn model(self, repo: &RepoId) -> PullRequest {
        // The source project comes as an id only; the same id as the
        // target means the branch is in this project, not a fork.
        let own = self.source_project_id.is_some() && self.source_project_id == self.target_project_id;
        PullRequest {
            number: self.iid,
            title: self.title,
            author: username(self.author),
            draft: self.draft || self.work_in_progress,
            head_ref: self.source_branch,
            head_sha: self.sha.unwrap_or_default(),
            head_repo: own.then(|| repo.slug()),
            base_ref: self.target_branch,
            updated: timestamp(self.updated_at.as_deref()),
        }
    }
}

pub fn parse_merge_requests(json: &str, repo: &RepoId) -> Result<Vec<PullRequest>, String> {
    let raw: Vec<RawMergeRequest> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(raw.into_iter().map(|request| request.model(repo)).collect())
}

/// One merge request, as the API answers its creation.
#[cfg_attr(not(test), expect(dead_code, reason = "reached through Backend::create_pull, which nothing calls until the pull request dialog does"))]
pub fn parse_merge_request(json: &str, repo: &RepoId) -> Result<PullRequest, String> {
    let raw: RawMergeRequest = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(raw.model(repo))
}

/// Labels come as plain names, or as objects when details are asked for.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawLabel {
    Name(String),
    Detailed { name: String },
}

#[derive(Deserialize)]
struct RawIssue {
    iid: u64,
    #[serde(default)]
    title: String,
    author: Option<RawUser>,
    #[serde(default)]
    labels: Vec<RawLabel>,
    #[serde(default)]
    user_notes_count: u64,
    updated_at: Option<String>,
}

/// GitLab's issues endpoint lists issues only; merge requests are separate.
pub fn parse_issues(json: &str) -> Result<Vec<Issue>, String> {
    let raw: Vec<RawIssue> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(raw
        .into_iter()
        .map(|issue| Issue {
            number: issue.iid,
            title: issue.title,
            author: username(issue.author),
            labels: issue
                .labels
                .into_iter()
                .map(|label| match label {
                    RawLabel::Name(name) | RawLabel::Detailed { name } => name,
                })
                .collect(),
            comments: issue.user_notes_count,
            updated: timestamp(issue.updated_at.as_deref()),
        })
        .collect())
}

#[derive(Deserialize)]
struct RawPipeline {
    id: u64,
    /// Numbered per project since GitLab 13.
    iid: Option<u64>,
    /// Set by `workflow:name` (GitLab 16), usually absent.
    name: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    source: String,
    created_at: Option<String>,
}

/// A pipeline's `status` as a [`RunState`].
pub fn pipeline_state(status: &str) -> RunState {
    match status {
        "success" => RunState::Success,
        "failed" => RunState::Failure,
        "canceled" | "canceling" => RunState::Cancelled,
        "skipped" => RunState::Skipped,
        "running" => RunState::Running,
        // A manual job is waiting for someone to start it.
        "manual" => RunState::NeedsAction,
        // created, waiting_for_resource, preparing, pending, scheduled and
        // whatever comes next.
        _ => RunState::Queued,
    }
}

/// Merge request pipelines run on `refs/merge-requests/<iid>/head`; that
/// reads better as `!<iid>`.
fn branch_of(reference: &str) -> String {
    reference
        .strip_prefix("refs/merge-requests/")
        .and_then(|rest| rest.split('/').next())
        .filter(|iid| !iid.is_empty() && iid.bytes().all(|b| b.is_ascii_digit()))
        .map_or_else(|| reference.to_owned(), |iid| format!("!{iid}"))
}

pub fn parse_pipelines(json: &str) -> Result<Vec<Run>, String> {
    let raw: Vec<RawPipeline> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(raw
        .into_iter()
        .map(|pipeline| Run {
            id: pipeline.id,
            workflow: pipeline.name.filter(|name| !name.is_empty()).unwrap_or_else(|| "Pipeline".to_owned()),
            title: String::new(),
            number: pipeline.iid.unwrap_or(pipeline.id),
            branch: branch_of(pipeline.reference.as_deref().unwrap_or_default()),
            event: pipeline.source.replace('_', " "),
            head_sha: pipeline.sha,
            state: pipeline_state(&pipeline.status),
            outcome: pipeline.status.replace('_', " "),
            when: timestamp(pipeline.created_at.as_deref()),
        })
        .collect())
}

/// What an error body says: its `message`, which GitLab sends as a string,
/// a list, or fields with their complaints (`{"title": ["is too long"]}`),
/// else the `error_description` or `error` of an OAuth-style answer.
pub fn api_message(body: &str) -> String {
    use serde_json::Value;
    fn flatten(value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            Value::Array(items) => items.iter().map(flatten).filter(|text| !text.is_empty()).collect::<Vec<_>>().join("; "),
            Value::Object(fields) => fields
                .iter()
                .map(|(field, complaint)| format!("{field} {}", flatten(complaint)).trim().to_owned())
                .collect::<Vec<_>>()
                .join("; "),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else { return String::new() };
    ["message", "error_description", "error"]
        .iter()
        .filter_map(|key| fields.get(*key))
        .map(flatten)
        .find(|text| !text.is_empty())
        .unwrap_or_default()
}

/// GitLab's word for "you may not": the feature is off for this project,
/// or this account may not use it.
const FORBIDDEN: &str = "403 Forbidden";

/// The body of a new merge request. GitLab has no draft flag on creation:
/// a title starting with "Draft:" is what makes one.
#[cfg_attr(not(test), expect(dead_code, reason = "reached through Backend::create_pull, which nothing calls until the pull request dialog does"))]
pub fn new_merge_request(new: &NewPull) -> String {
    let title = new.title.trim();
    let lower = title.to_lowercase();
    let marked = ["draft:", "[draft]", "(draft)", "draft -"].iter().any(|mark| lower.starts_with(mark));
    let title = if new.draft && !marked { format!("Draft: {title}") } else { title.to_owned() };
    serde_json::json!({
        "source_branch": new.head,
        "target_branch": new.base,
        "title": title,
        "description": new.body,
    })
    .to_string()
}

// ---- the client ----

#[derive(Clone, Copy)]
enum List {
    MergeRequests,
    Issues,
    Pipelines,
}

impl List {
    fn url(self, api: &str, project: &str) -> String {
        let base = format!("{api}/projects/{project}");
        match self {
            List::MergeRequests => format!("{base}/merge_requests?state=opened&per_page=30"),
            List::Issues => format!("{base}/issues?state=opened&per_page=30"),
            List::Pipelines => format!("{base}/pipelines?per_page=20"),
        }
    }
}

enum Got {
    Body {
        text: String,
        /// The server says there is a next page.
        more: bool,
    },
    Unchanged,
    /// The feature is off for this project, or not open to this account.
    Nothing,
}

pub struct Client {
    repo: RepoId,
    /// [`RepoId::api_base`]: every request goes below it.
    api: String,
    /// [`project_id`], computed once.
    project: String,
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
            project: project_id(&repo),
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

    /// Replaces the identity requests are made with. Validators and rate
    /// limits belong to an identity, so both start over.
    fn sign_in(&mut self, token: Option<(Token, TokenSource)>) {
        if self.token.as_ref().map(|(token, _)| token) != token.as_ref().map(|(token, _)| token) {
            self.etags = [None, None, None];
            self.limiter = Limiter::default();
        }
        self.token = token;
        self.looked = true;
    }

    /// Looks for a token now if there is reason to.
    fn look(&mut self) {
        if !self.looked {
            let found = find_token(self.secrets.as_ref(), &self.repo, &self.rejected);
            self.sign_in(found);
        }
    }

    /// The token in use was refused: remember that, and move on to the
    /// next source (ending with none at all).
    fn refuse_token(&mut self) {
        if let Some((token, _)) = self.token.take() {
            self.rejected.push(token);
        }
        let found = find_token(self.secrets.as_ref(), &self.repo, &self.rejected);
        self.sign_in(found);
    }

    /// Every token this client has held, to scrub out of messages.
    fn scrub(&self, problem: Problem) -> Problem {
        let mut tokens: Vec<&Token> = self.rejected.iter().collect();
        tokens.extend(self.token.as_ref().map(|(token, _)| token));
        problem.scrubbed(&tokens)
    }

    fn get(&mut self, list: List, now: u64) -> Result<Got, Problem> {
        let mut url = list.url(&self.api, &self.project);
        let mut hops = 0;
        loop {
            self.look();
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
                    let next_page = response.header("x-next-page").is_some_and(|page| !page.trim().is_empty());
                    let linked = response.header("link").is_some_and(|link| link.contains("rel=\"next\""));
                    Ok(Got::Body { text: response.body, more: next_page || linked })
                }
                304 => Ok(Got::Unchanged),
                // A moved project's old path may answer with its new one.
                301 | 302 | 307 | 308 => match response.header("location") {
                    Some(location) if within(&self.api, location) && hops < 3 => {
                        url = location.to_owned();
                        hops += 1;
                        continue;
                    }
                    _ => Err(Problem::Failed("GitLab sent an unexpected redirect".to_owned())),
                },
                401 if self.token.is_some() => {
                    self.refuse_token();
                    continue;
                }
                // Private projects look like missing ones from outside.
                401 | 404 if self.token.is_none() => Err(if self.rejected.is_empty() {
                    Problem::NotSignedIn
                } else {
                    Problem::SignInRejected
                }),
                404 => Err(Problem::NoAccess),
                403 if message == FORBIDDEN => Ok(Got::Nothing),
                403 if !message.is_empty() => Err(Problem::Refused(message)),
                status => Err(Problem::Failed(format!("GitLab answered {status}"))),
            };
        }
    }

    fn fetch<T>(
        &mut self,
        list: List,
        now: u64,
        parse: impl FnOnce(&str) -> Result<Vec<T>, String>,
    ) -> Outcome<T> {
        let outcome = match self.get(list, now) {
            Ok(Got::Unchanged) => Outcome::Unchanged,
            Ok(Got::Nothing) => Outcome::Fresh { items: Vec::new(), more: false },
            Ok(Got::Body { text, more }) => match parse(&text) {
                Ok(items) => Outcome::Fresh { items, more },
                Err(error) => {
                    // Do not let a later "not modified" vouch for an
                    // answer we could not read.
                    self.etags[list as usize] = None;
                    Outcome::Failed(Problem::Failed(format!("Unreadable answer from GitLab ({error})")))
                }
            },
            Err(problem) => Outcome::Failed(problem),
        };
        match outcome {
            Outcome::Failed(problem) => Outcome::Failed(self.scrub(problem)),
            fine => fine,
        }
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "reached through Backend::create_pull, which nothing calls until the pull request dialog does"))]
    fn create(&mut self, new: &NewPull, now: u64) -> Result<PullRequest, Problem> {
        let url = format!("{}/projects/{}/merge_requests", self.api, self.project);
        let body = new_merge_request(new);
        loop {
            self.look();
            // Nobody can open a merge request anonymously: do not even ask.
            let Some((token, _)) = &self.token else {
                return Err(Problem::Failed(if self.rejected.is_empty() {
                    "Not signed in to GitLab. Set GITLAB_TOKEN or run `glab auth login`, then try again.".to_owned()
                } else {
                    "GitLab refused the saved sign-in (expired or revoked?). Sign in again, then try again."
                        .to_owned()
                }));
            };
            if let Some(until) = self.limiter.blocked(now) {
                return Err(Problem::RateLimited { until });
            }
            let request = HttpRequest { url: &url, etag: None, token: Some(token) };
            let response = self.transport.post(&request, &body).map_err(Problem::Unreachable)?;
            let message = api_message(&response.body);
            let rate = Rate::of(&response);
            if let Some(until) = self.limiter.record(response.status, &rate, &message, now) {
                return Err(Problem::RateLimited { until });
            }
            return match response.status {
                200 | 201 => parse_merge_request(&response.body, &self.repo)
                    .map_err(|error| Problem::Failed(format!("Unreadable answer from GitLab ({error})"))),
                // Refused before anything was created: the next sign-in
                // may do better.
                401 => {
                    self.refuse_token();
                    continue;
                }
                404 => Err(Problem::NoAccess),
                // Already open for this branch (409), a branch that does not
                // exist (400, 422), not allowed (403): GitLab says which.
                400 | 403 | 405 | 409 | 422 if !message.is_empty() => Err(Problem::Refused(message)),
                status => Err(Problem::Failed(format!("GitLab answered {status}"))),
            };
        }
    }
}

impl Backend for Client {
    fn pulls(&mut self, now: u64) -> Outcome<PullRequest> {
        let repo = self.repo.clone();
        self.fetch(List::MergeRequests, now, |json| parse_merge_requests(json, &repo))
    }

    fn issues(&mut self, now: u64) -> Outcome<Issue> {
        self.fetch(List::Issues, now, parse_issues)
    }

    fn runs(&mut self, now: u64) -> Outcome<Run> {
        self.fetch(List::Pipelines, now, parse_pipelines)
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
        // The user may have signed in since (`glab auth login`, a push
        // through the credential manager). A working token is kept.
        if self.token.is_none() {
            self.looked = false;
        }
    }

    fn blocked_until(&self, now: u64) -> Option<u64> {
        self.limiter.blocked(now)
    }

    fn create_pull(&mut self, new: &NewPull, now: u64) -> Result<PullRequest, Problem> {
        let created = self.create(new, now);
        created.map_err(|problem| self.scrub(problem))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fake, Log, NOW, OTHER, SECRET, Script, reply};
    use super::*;

    fn project() -> RepoId {
        RepoId {
            service: Service::GitLab,
            host: "gitlab.com".to_owned(),
            owner: "group/sub".to_owned(),
            name: "app".to_owned(),
        }
    }

    fn scripted(replies: Vec<Result<HttpResponse, String>>, secrets: Fake) -> (Client, Log) {
        let sent = Log::default();
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        (Client::new(project(), Box::new(transport), Box::new(secrets)), sent)
    }

    fn with_token() -> Fake {
        Fake { env: vec![("GITLAB_TOKEN", SECRET)], ..Default::default() }
    }

    fn failure<T: std::fmt::Debug>(outcome: Outcome<T>) -> Problem {
        match outcome {
            Outcome::Failed(problem) => problem,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    fn new_pull(draft: bool) -> NewPull {
        NewPull {
            head: "feature/fast".to_owned(),
            base: "main".to_owned(),
            title: "Speed up the graph".to_owned(),
            body: "Caches lane colours.\n\nCloses #31".to_owned(),
            draft,
        }
    }

    // Trimmed-down API answers.
    const MERGE_REQUESTS: &str = r#"[
      {"id": 9001, "iid": 12, "project_id": 7, "title": "Add dark mode", "state": "opened",
       "draft": false, "work_in_progress": false, "author": {"id": 1, "username": "ada", "name": "Ada"},
       "source_branch": "feature/dark", "target_branch": "main",
       "source_project_id": 7, "target_project_id": 7,
       "sha": "1111111111111111111111111111111111111111", "updated_at": "1970-01-02T00:00:00.000Z",
       "web_url": "https://gitlab.com/group/sub/app/-/merge_requests/12"},
      {"id": 9002, "iid": 9, "title": "WIP: parser", "work_in_progress": true, "author": null,
       "source_branch": "main", "target_branch": "main",
       "source_project_id": 99, "target_project_id": 7, "sha": null},
      {"id": 9003, "iid": 8, "title": "Draft: docs", "draft": true,
       "author": {"username": "grace"}, "source_branch": "docs", "target_branch": "main"}
    ]"#;

    const ISSUES: &str = r#"[
      {"id": 1, "iid": 31, "title": "Crash on empty repo", "author": {"username": "grace"},
       "labels": ["bug", "good first issue"], "user_notes_count": 4,
       "updated_at": "1970-01-01T00:01:00Z"},
      {"id": 2, "iid": 30, "title": "Docs typo", "author": {"username": "linus"},
       "labels": [{"id": 5, "name": "docs", "color": "Fixed"}]},
      {"id": 3, "iid": 29, "title": "Sparse"}
    ]"#;

    const PIPELINES: &str = r#"[
      {"id": 501, "iid": 140, "project_id": 7, "sha": "1111111111111111111111111111111111111111",
       "ref": "refs/merge-requests/12/head", "status": "success", "source": "merge_request_event",
       "created_at": "1970-01-01T01:00:00Z", "web_url": "https://gitlab.com/group/sub/app/-/pipelines/501"},
      {"id": 502, "sha": "3333333333333333333333333333333333333333", "ref": "v1.2",
       "status": "running", "source": "push", "name": "Release"},
      {"id": 503, "iid": 139, "sha": "4444444444444444444444444444444444444444", "ref": "main",
       "status": "waiting_for_resource", "source": "schedule"}
    ]"#;

    // ---- JSON ----

    #[test]
    fn merge_request_json_maps_to_the_model() {
        let requests = parse_merge_requests(MERGE_REQUESTS, &project()).unwrap();
        assert_eq!(requests.len(), 3);

        let first = &requests[0];
        assert_eq!((first.number, first.title.as_str(), first.author.as_str()), (12, "Add dark mode", "ada"));
        assert!(!first.draft);
        assert_eq!((first.head_ref.as_str(), first.base_ref.as_str()), ("feature/dark", "main"));
        assert_eq!(first.head_sha, "1111111111111111111111111111111111111111");
        assert_eq!(first.updated, 86_400);
        // Source and target are the same project: its branch is ours.
        assert!(first.is_from(&project()));

        // A draft in the old spelling, from a fork, by a deleted account.
        let second = &requests[1];
        assert!(second.draft);
        assert_eq!(second.author, "ghost");
        assert_eq!((second.head_repo.as_deref(), second.head_sha.as_str()), (None, ""));
        assert!(!second.is_from(&project()));
        // No project ids at all: not assumed to be ours.
        assert!(requests[2].draft && !requests[2].is_from(&project()));

        assert_eq!(parse_merge_requests("[]", &project()).unwrap(), Vec::new());
        assert!(parse_merge_requests(r#"{"message": "404 Project Not Found"}"#, &project()).is_err());
        assert!(parse_merge_requests("<html>", &project()).is_err());
    }

    #[test]
    fn issue_json_maps_to_the_model_with_either_kind_of_label() {
        let issues = parse_issues(ISSUES).unwrap();
        assert_eq!(issues.iter().map(|issue| issue.number).collect::<Vec<_>>(), [31, 30, 29]);
        assert_eq!((issues[0].author.as_str(), issues[0].comments, issues[0].updated), ("grace", 4, 60));
        assert_eq!(issues[0].labels, ["bug", "good first issue"]);
        assert_eq!(issues[1].labels, ["docs"]);
        assert_eq!((issues[2].author.as_str(), issues[2].comments, issues[2].labels.len()), ("ghost", 0, 0));
        assert!(parse_issues(r#"{"message": "403 Forbidden"}"#).is_err());
    }

    #[test]
    fn pipelines_map_their_status_and_read_merge_request_refs() {
        let runs = parse_pipelines(PIPELINES).unwrap();
        let first = &runs[0];
        assert_eq!((first.id, first.number, first.workflow.as_str()), (501, 140, "Pipeline"));
        assert_eq!((first.branch.as_str(), first.event.as_str()), ("!12", "merge request event"));
        assert_eq!((first.state, first.outcome.as_str(), first.when), (RunState::Success, "success", 3600));
        assert_eq!(first.head_sha, "1111111111111111111111111111111111111111");
        // A named pipeline on a tag, from a server too old for `iid`.
        assert_eq!((runs[1].workflow.as_str(), runs[1].number, runs[1].branch.as_str()), ("Release", 502, "v1.2"));
        assert_eq!(runs[1].state, RunState::Running);
        assert_eq!((runs[2].state, runs[2].outcome.as_str()), (RunState::Queued, "waiting for resource"));

        for (status, expected) in [
            ("success", RunState::Success),
            ("failed", RunState::Failure),
            ("canceled", RunState::Cancelled),
            ("canceling", RunState::Cancelled),
            ("skipped", RunState::Skipped),
            ("running", RunState::Running),
            ("manual", RunState::NeedsAction),
            ("created", RunState::Queued),
            ("pending", RunState::Queued),
            ("preparing", RunState::Queued),
            ("scheduled", RunState::Queued),
            ("something_new", RunState::Queued),
        ] {
            assert_eq!(pipeline_state(status), expected, "{status}");
        }
        assert_eq!(branch_of("refs/merge-requests/12/merge"), "!12");
        assert_eq!(branch_of("refs/merge-requests/x/head"), "refs/merge-requests/x/head");
        assert_eq!(branch_of("feature/x"), "feature/x");

        // A merge request gets its checks from the pipeline on its head.
        let requests = parse_merge_requests(MERGE_REQUESTS, &project()).unwrap();
        assert_eq!(checks_for(&requests[0].head_sha, &runs), Some(Checks::Passing));
        assert!(parse_pipelines(r#"{"pipelines": []}"#).is_err());
    }

    #[test]
    fn error_bodies_are_read_in_every_shape_gitlab_uses() {
        assert_eq!(api_message(r#"{"message": "404 Project Not Found"}"#), "404 Project Not Found");
        assert_eq!(
            api_message(r#"{"message": ["Another open merge request already exists for this source branch: !5"]}"#),
            "Another open merge request already exists for this source branch: !5"
        );
        assert_eq!(
            api_message(r#"{"message": {"source_branch": ["is invalid"], "title": ["is too long", "is odd"]}}"#),
            "source_branch is invalid; title is too long; is odd"
        );
        assert_eq!(
            api_message(r#"{"error": "insufficient_scope", "error_description": "The request requires higher privileges than provided by the access token."}"#),
            "The request requires higher privileges than provided by the access token."
        );
        assert_eq!(api_message(r#"{"error": "title is missing"}"#), "title is missing");
        assert_eq!(api_message("<html>502</html>"), "");
        assert_eq!(api_message("[]"), "");
    }

    // ---- the client ----

    #[test]
    fn requests_go_to_the_project_by_its_encoded_path_with_the_token() {
        let (mut client, sent) = scripted(
            vec![
                reply(200, &[("x-next-page", "2"), ("x-total", "57")], MERGE_REQUESTS),
                reply(200, &[("x-next-page", "")], ISSUES),
                reply(200, &[], PIPELINES),
            ],
            with_token(),
        );
        assert_eq!(client.auth(), Auth::Unknown);
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, more: true } if items.len() == 3));
        assert!(matches!(client.issues(NOW), Outcome::Fresh { ref items, more: false } if items.len() == 3));
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, more: false } if items.len() == 3));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("GITLAB_TOKEN")));

        let sent = sent.lock().unwrap();
        let urls: Vec<&str> = sent.iter().map(|request| request.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://gitlab.com/api/v4/projects/group%2Fsub%2Fapp/merge_requests?state=opened&per_page=30",
                "https://gitlab.com/api/v4/projects/group%2Fsub%2Fapp/issues?state=opened&per_page=30",
                "https://gitlab.com/api/v4/projects/group%2Fsub%2Fapp/pipelines?per_page=20",
            ]
        );
        assert!(sent.iter().all(|request| within("https://gitlab.com/api/v4", &request.url)));
        assert!(sent.iter().all(|request| request.token.as_deref() == Some(SECRET) && request.body.is_none()));
    }

    #[test]
    fn a_self_hosted_server_is_asked_through_its_own_api() {
        let sent = Log::default();
        let transport = Script { replies: vec![reply(200, &[], "[]")].into(), sent: sent.clone() };
        let repo = RepoId { host: "git.acme.com:8443".to_owned(), owner: "team".to_owned(), ..project() };
        // gitlab.com's token is not this server's.
        let secrets = Fake { env: vec![("GITLAB_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() };
        let mut client = Client::new(repo, Box::new(transport), Box::new(secrets.clone()));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        let sent = sent.lock().unwrap();
        assert_eq!(sent[0].url, "https://git.acme.com:8443/api/v4/projects/team%2Fapp/merge_requests?state=opened&per_page=30");
        assert_eq!(sent[0].token.as_deref(), Some(OTHER));
        assert_eq!(secrets.asked(), ["GITLAB_HOST", "glab git.acme.com:8443"]);
    }

    #[test]
    fn unchanged_lists_are_asked_for_conditionally() {
        let (mut client, sent) = scripted(
            vec![reply(200, &[("etag", "W/\"v1\"")], MERGE_REQUESTS), reply(304, &[], ""), reply(200, &[], "[]")],
            Fake::default(),
        );
        let mut feed = Feed::default();
        feed.apply(client.pulls(NOW));
        assert_eq!(feed.items.len(), 3);
        assert_eq!(client.pulls(NOW), Outcome::Unchanged);
        feed.apply(client.pulls(NOW));
        assert!(feed.items.is_empty() && feed.loaded);

        let sent = sent.lock().unwrap();
        let etags: Vec<Option<&str>> = sent.iter().map(|request| request.etag.as_deref()).collect();
        assert_eq!(etags, [None, Some("W/\"v1\""), Some("W/\"v1\"")]);
        assert!(sent.iter().all(|request| request.token.is_none()));
        assert_eq!(client.auth(), Auth::Anonymous);
    }

    #[test]
    fn status_codes_map_to_problems_in_gitlabs_terms() {
        // Private projects look missing from outside.
        let (mut client, _) = scripted(vec![reply(404, &[], r#"{"message": "404 Project Not Found"}"#)], Fake::default());
        let problem = failure(client.pulls(NOW));
        assert_eq!(problem, Problem::NotSignedIn);
        assert_eq!(problem.line(Service::GitLab), "Not signed in: private project");
        let (mut client, _) = scripted(vec![reply(404, &[], r#"{"message": "404 Project Not Found"}"#)], with_token());
        assert_eq!(failure(client.pulls(NOW)), Problem::NoAccess);

        // A feature that is switched off answers a bare 403: nothing to show.
        let (mut client, _) = scripted(vec![reply(403, &[], r#"{"message": "403 Forbidden"}"#)], with_token());
        assert_eq!(client.runs(NOW), Outcome::Fresh { items: Vec::new(), more: false });
        // Any other refusal is shown in GitLab's words.
        let scope = r#"{"error": "insufficient_scope", "error_description": "The request requires higher privileges than provided by the access token."}"#;
        let (mut client, _) = scripted(vec![reply(403, &[], scope)], with_token());
        assert_eq!(
            failure(client.issues(NOW)).line(Service::GitLab),
            "GitLab refused: The request requires higher privileges than provided by the access token."
        );

        let (mut client, _) = scripted(
            vec![reply(502, &[], "<html>"), reply(200, &[], "{ not json"), Err("no connection".to_owned())],
            Fake::default(),
        );
        assert_eq!(failure(client.pulls(NOW)), Problem::Failed("GitLab answered 502".to_owned()));
        assert!(matches!(failure(client.pulls(NOW)), Problem::Failed(text) if text.starts_with("Unreadable answer from GitLab")));
        assert_eq!(failure(client.pulls(NOW)).line(Service::GitLab), "GitLab unreachable: no connection");
    }

    #[test]
    fn a_refused_token_gives_way_to_the_next_source_then_to_none() {
        let unauthorized = r#"{"message": "401 Unauthorized"}"#;
        let secrets = Fake { env: vec![("GITLAB_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() };
        let (mut client, sent) =
            scripted(vec![reply(401, &[], unauthorized), reply(200, &[], ISSUES)], secrets);
        assert!(matches!(client.issues(NOW), Outcome::Fresh { .. }));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Cli));
        let tokens: Vec<Option<String>> = sent.lock().unwrap().iter().map(|request| request.token.clone()).collect();
        assert_eq!(tokens, [Some(SECRET.to_owned()), Some(OTHER.to_owned())]);

        let (mut client, sent) = scripted(
            vec![reply(401, &[], unauthorized), reply(404, &[], r#"{"message": "404 Project Not Found"}"#)],
            with_token(),
        );
        assert_eq!(failure(client.pulls(NOW)), Problem::SignInRejected);
        assert_eq!(client.auth(), Auth::Rejected);
        assert_eq!(sent.lock().unwrap()[1].token, None);
    }

    #[test]
    fn gitlab_rate_limits_stop_requests_until_they_lift() {
        let reset = (NOW + 900).to_string();
        let limited = [("ratelimit-remaining", "0"), ("ratelimit-reset", reset.as_str()), ("retry-after", "60")];
        let (mut client, sent) =
            scripted(vec![reply(429, &limited, "Retry later"), reply(200, &[], ISSUES)], with_token());
        assert_eq!(failure(client.pulls(NOW)), Problem::RateLimited { until: NOW + 60 });
        assert_eq!(failure(client.issues(NOW + 30)), Problem::RateLimited { until: NOW + 60 });
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert!(matches!(client.issues(NOW + 60), Outcome::Fresh { .. }));

        // The last request of a window holds the next one back.
        let last = [("ratelimit-remaining", "0"), ("ratelimit-reset", reset.as_str())];
        let (mut client, sent) = scripted(vec![reply(200, &last, "[]"), reply(200, &[], "[]")], with_token());
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(failure(client.issues(NOW + 1)), Problem::RateLimited { until: NOW + 900 });
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn redirects_are_followed_inside_the_api_and_nowhere_else() {
        let moved = "https://gitlab.com/api/v4/projects/42/merge_requests?state=opened&per_page=30";
        let (mut client, sent) =
            scripted(vec![reply(301, &[("location", moved)], ""), reply(200, &[], "[]")], with_token());
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(sent.lock().unwrap()[1].url, moved);

        for target in [
            "https://gitlab.com/group/sub/app",
            "https://evil.example/api/v4/projects",
            "http://gitlab.com/api/v4/projects/42",
            "https://gitlab.com.evil.example/api/v4/x",
        ] {
            let (mut client, sent) =
                scripted(vec![reply(302, &[("location", target)], ""), reply(200, &[], "[]")], with_token());
            assert_eq!(failure(client.pulls(NOW)), Problem::Failed("GitLab sent an unexpected redirect".to_owned()));
            assert_eq!(sent.lock().unwrap().len(), 1, "{target}");
        }
    }

    #[test]
    fn a_token_never_reaches_a_displayed_error() {
        let echo = format!(r#"{{"message": "Bad token {SECRET}", "error_description": "{SECRET}"}}"#);
        let listed = format!(r#"{{"message": ["first {SECRET}", "then {OTHER}"]}}"#);
        let cases: Vec<(Fake, Vec<Result<HttpResponse, String>>)> = vec![
            (with_token(), vec![Err(format!("tls: failed sending Authorization: Bearer {SECRET}"))]),
            (with_token(), vec![reply(403, &[], &echo)]),
            (with_token(), vec![reply(500, &[], &echo)]),
            (with_token(), vec![reply(200, &[], &echo)]),
            (with_token(), vec![reply(200, &[], &format!("[{{\"iid\": \"{SECRET}\"}}]"))]),
            (with_token(), vec![reply(301, &[("location", format!("https://evil.example/{SECRET}").as_str())], "")]),
            (
                Fake { env: vec![("GITLAB_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() },
                vec![reply(401, &[], &listed), reply(403, &[], &listed)],
            ),
            (with_token(), vec![reply(401, &[], &echo), Err(format!("proxy said: {SECRET}"))]),
        ];
        for (index, (secrets, replies)) in cases.into_iter().enumerate() {
            let (mut client, _) = scripted(replies, secrets);
            let Outcome::Failed(problem) = client.pulls(NOW) else { continue };
            for text in [
                problem.line(Service::GitLab),
                problem.brief(Service::GitLab),
                problem.detail(Service::GitLab),
                format!("{problem:?}"),
                client.auth().describe(Service::GitLab),
            ] {
                assert!(!text.contains(SECRET) && !text.contains(OTHER), "case {index}: {text}");
            }
        }
        let (mut client, _) = scripted(vec![reply(403, &[], &echo)], with_token());
        assert_eq!(failure(client.pulls(NOW)), Problem::Refused("Bad token ***".to_owned()));
    }

    // ---- opening a merge request ----

    const CREATED: &str = r#"{"id": 9100, "iid": 13, "title": "Draft: Speed up the graph", "draft": true,
       "author": {"username": "ada"}, "source_branch": "feature/fast", "target_branch": "main",
       "source_project_id": 7, "target_project_id": 7,
       "sha": "5555555555555555555555555555555555555555", "updated_at": "1970-01-03T00:00:00Z"}"#;

    #[test]
    fn a_merge_request_is_opened_with_one_post_to_the_project() {
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], with_token());
        let created = client.create_pull(&new_pull(true), NOW).unwrap();
        assert_eq!((created.number, created.title.as_str()), (13, "Draft: Speed up the graph"));
        assert!(created.draft && created.is_from(&project()));
        assert_eq!((created.head_ref.as_str(), created.base_ref.as_str()), ("feature/fast", "main"));

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].url, "https://gitlab.com/api/v4/projects/group%2Fsub%2Fapp/merge_requests");
        assert_eq!(sent[0].token.as_deref(), Some(SECRET));
        let body: serde_json::Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "source_branch": "feature/fast",
                "target_branch": "main",
                "title": "Draft: Speed up the graph",
                "description": "Caches lane colours.\n\nCloses #31",
            })
        );
    }

    #[test]
    fn draft_titles_get_the_prefix_once() {
        let title = |title: &str, draft: bool| {
            let body = new_merge_request(&NewPull { title: title.to_owned(), draft, ..new_pull(draft) });
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["title"].as_str().unwrap().to_owned()
        };
        assert_eq!(title("Speed up", true), "Draft: Speed up");
        assert_eq!(title("  Speed up  ", false), "Speed up");
        assert_eq!(title("Draft: Speed up", true), "Draft: Speed up");
        assert_eq!(title("draft: speed up", true), "draft: speed up");
        assert_eq!(title("[Draft] Speed up", true), "[Draft] Speed up");
        // Not a draft, but the user wrote the prefix: GitLab decides.
        assert_eq!(title("Draft: Speed up", false), "Draft: Speed up");
        // Quotes and newlines in what the user typed stay data.
        let body = new_merge_request(&NewPull { title: "Say \"hi\"\n".to_owned(), ..new_pull(false) });
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["title"], "Say \"hi\"");
    }

    #[test]
    fn opening_a_merge_request_needs_a_sign_in_and_says_why_it_failed() {
        // No token: nothing is sent at all.
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], Fake::default());
        let problem = client.create_pull(&new_pull(false), NOW).unwrap_err();
        assert!(matches!(&problem, Problem::Failed(text) if text.starts_with("Not signed in to GitLab")));
        assert!(sent.lock().unwrap().is_empty());

        // GitLab's own reasons come through in its words.
        let exists = r#"{"message": ["Another open merge request already exists for this source branch: !5"]}"#;
        let (mut client, _) = scripted(vec![reply(409, &[], exists)], with_token());
        assert_eq!(
            client.create_pull(&new_pull(false), NOW).unwrap_err().line(Service::GitLab),
            "GitLab refused: Another open merge request already exists for this source branch: !5"
        );
        let invalid = r#"{"message": {"source_branch": ["is invalid"]}}"#;
        let (mut client, _) = scripted(vec![reply(400, &[], invalid)], with_token());
        assert_eq!(client.create_pull(&new_pull(false), NOW), Err(Problem::Refused("source_branch is invalid".to_owned())));
        let (mut client, _) = scripted(vec![reply(404, &[], r#"{"message": "404 Project Not Found"}"#)], with_token());
        assert_eq!(client.create_pull(&new_pull(false), NOW), Err(Problem::NoAccess));
        let (mut client, _) = scripted(vec![reply(500, &[], "")], with_token());
        assert_eq!(client.create_pull(&new_pull(false), NOW), Err(Problem::Failed("GitLab answered 500".to_owned())));
        let (mut client, _) = scripted(vec![reply(201, &[], "{}")], with_token());
        assert!(matches!(client.create_pull(&new_pull(false), NOW), Err(Problem::Failed(text)) if text.starts_with("Unreadable")));
    }

    #[test]
    fn a_refused_token_is_replaced_before_a_merge_request_is_opened_again() {
        let unauthorized = r#"{"message": "401 Unauthorized"}"#;
        let secrets = Fake { env: vec![("GITLAB_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() };
        let (mut client, sent) = scripted(vec![reply(401, &[], unauthorized), reply(201, &[], CREATED)], secrets);
        assert_eq!(client.create_pull(&new_pull(true), NOW).unwrap().number, 13);
        let tokens: Vec<Option<String>> = sent.lock().unwrap().iter().map(|request| request.token.clone()).collect();
        assert_eq!(tokens, [Some(SECRET.to_owned()), Some(OTHER.to_owned())]);

        // Every token refused: stop, without an anonymous attempt.
        let (mut client, sent) = scripted(vec![reply(401, &[], unauthorized), reply(201, &[], CREATED)], with_token());
        let problem = client.create_pull(&new_pull(true), NOW).unwrap_err();
        assert!(matches!(&problem, Problem::Failed(text) if text.starts_with("GitLab refused the saved sign-in")));
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn opening_a_merge_request_respects_rate_limits_and_hides_tokens() {
        let (mut client, sent) = scripted(
            vec![reply(429, &[("retry-after", "120")], "Retry later"), reply(201, &[], CREATED)],
            with_token(),
        );
        assert_eq!(client.create_pull(&new_pull(false), NOW), Err(Problem::RateLimited { until: NOW + 120 }));
        assert_eq!(client.create_pull(&new_pull(false), NOW + 60), Err(Problem::RateLimited { until: NOW + 120 }));
        assert_eq!(sent.lock().unwrap().len(), 1);

        let echo = format!(r#"{{"message": "token {SECRET} may not push here"}}"#);
        let (mut client, _) = scripted(vec![reply(403, &[], &echo)], with_token());
        let problem = client.create_pull(&new_pull(false), NOW).unwrap_err();
        assert_eq!(problem, Problem::Refused("token *** may not push here".to_owned()));
        let (mut client, _) = scripted(vec![Err(format!("reset while sending Bearer {SECRET}"))], with_token());
        assert!(!format!("{:?}", client.create_pull(&new_pull(false), NOW)).contains(SECRET));
    }
}
