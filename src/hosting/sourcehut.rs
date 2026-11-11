//! SourceHut (sr.ht): repositories on git.sr.ht, their tickets on
//! todo.sr.ht and their builds on builds.sr.ht, through each service's
//! GraphQL API (`POST https://<service>.sr.ht/query`).
//!
//! SourceHut has no pull requests: patches are mailed to a project's
//! mailing list with `git send-email`, so the sidebar says how instead of
//! listing any ([`Service::has_pulls`]), and there are no forks.
//!
//! * Tickets: those of the tracker named like the repository under the same
//!   user (`todo.sr.ht/~user/name`), the newest first, resolved ones left
//!   out. Without such a tracker the section is not shown.
//! * Builds: the owner's latest build jobs that git.sr.ht submitted for a
//!   push to this repository, tagged `<name>/commits/<branch>/<manifest>`,
//!   their commit read from the job's note. Without any, the section is not
//!   shown.
//!
//! The APIs answer nobody signed out, public repositories included, so
//! nothing is asked without a token ([`Service::needs_sign_in`]). Tokens
//! are OAuth 2.0 personal access tokens (meta.sr.ht > OAuth 2.0), sent as
//! bearer tokens to SourceHut's GraphQL endpoints only ([`ENDPOINTS`]):
//! `SRHT_TOKEN`, then the `access-token` of hut's instance for sr.ht in its
//! configuration file, then Git's credential helper for git.sr.ht. A token
//! refused outright is answered 403; one without the grant a field needs is
//! answered 200 with the error ("Access denied, missing TICKETS:RO grant").
//!
//! Only sr.ht itself is covered: a self-hosted SourceHut keeps each service
//! on a host of its own, which nothing in a repository's remote names.

use serde::Deserialize;
use serde_json::Value;

use super::account::{Profile, PublicKey, SshKey};
use super::signin::{Api, Got, json};
use super::*;

/// The host of SourceHut's git repositories.
pub const HOST: &str = "git.sr.ht";

/// git.sr.ht's GraphQL endpoint: [`RepoId::api_base`] of its repositories.
pub const GIT_API: &str = "https://git.sr.ht/query";

/// todo.sr.ht's, where the tickets are.
pub const TODO_API: &str = "https://todo.sr.ht/query";

/// builds.sr.ht's, where the build jobs are.
pub const BUILDS_API: &str = "https://builds.sr.ht/query";

/// meta.sr.ht's, where the account and its SSH keys are.
pub const META_API: &str = "https://meta.sr.ht/query";

/// Every address a SourceHut token is sent to, exactly.
pub const ENDPOINTS: &[&str] = &[GIT_API, TODO_API, BUILDS_API, META_API];

/// Sent with every request.
pub const HEADERS: &[(&str, &str)] = &[("Accept", "application/json")];

/// How patches reach a SourceHut project, and the guide SourceHut wrote
/// for setting `git send-email` up.
pub const SEND_EMAIL_GUIDE: &str = "https://git-send-email.io";

/// A page of the repository's tracker: `todo.sr.ht/~user/name[/path]`.
pub fn tracker_page(repo: &RepoId, path: &str) -> String {
    let tracker = format!("https://todo.sr.ht/{}/{}", repo.owner, repo.name);
    if path.is_empty() { tracker } else { format!("{tracker}/{path}") }
}

/// A page of the owner's builds: `builds.sr.ht/~user/<path>`, where a
/// path of tags lists the jobs tagged so.
pub fn builds_page(repo: &RepoId, path: &str) -> String {
    format!("https://builds.sr.ht/{}/{path}", repo.owner)
}

/// The fixture file (`todo`, `builds`, `git`, `meta`) answering a GraphQL
/// request to `url`, when it is one of SourceHut's.
pub fn fixture_name(url: &str) -> Option<&'static str> {
    ["git", "todo", "builds", "meta"]
        .into_iter()
        .find(|service| url == format!("https://{service}.sr.ht/query"))
}

// ---- hut's sign-ins ----

/// The token hut (SourceHut's command line tool) keeps for `host`, read
/// from its configuration file (`<config dir>/hut/config`): hut prints the
/// token of its first instance only, which may be another one's.
pub fn hut_token(host: &str) -> Option<String> {
    let path = dirs::config_dir()?.join("hut").join("config");
    // A sign-in file, not something to read megabytes of.
    if std::fs::metadata(&path).ok()?.len() > 1024 * 1024 {
        return None;
    }
    hut_login_token(&std::fs::read_to_string(path).ok()?, host)
}

/// The `access-token` of the instance in hut's configuration whose git
/// service is at `host`:
///
/// ```text
/// instance "sr.ht" {
///     origin "sr.ht"
///     access-token "<token>"
/// }
/// ```
///
/// An instance's git service is at `git.<origin>` (its name when it has no
/// origin), unless its `git { origin "..." }` block says otherwise. A token
/// that comes from a command (`access-token-cmd`) is not run for.
pub fn hut_login_token(config: &str, host: &str) -> Option<String> {
    #[derive(Default)]
    struct Instance {
        name: String,
        origin: Option<String>,
        git: Option<String>,
        token: Option<String>,
    }
    let mut instances: Vec<Instance> = Vec::new();
    // The directives whose blocks are open.
    let mut open: Vec<String> = Vec::new();
    for line in config.lines() {
        let words = words(line);
        let Some(first) = words.first() else { continue };
        if first == "}" {
            open.pop();
            continue;
        }
        let opens = words.last().is_some_and(|last| last == "{");
        let param = words.get(1).filter(|param| *param != "{").cloned();
        let within = open.iter().map(String::as_str).collect::<Vec<_>>();
        match (within.as_slice(), first.as_str(), instances.last_mut()) {
            ([], "instance", _) => instances.push(Instance { name: param.unwrap_or_default(), ..Default::default() }),
            (["instance"], "origin", Some(instance)) => instance.origin = param,
            (["instance"], "access-token", Some(instance)) => instance.token = param,
            (["instance", "git"], "origin", Some(instance)) => instance.git = param,
            _ => {}
        }
        if opens {
            open.push(first.clone());
        }
    }
    instances
        .into_iter()
        .find(|instance| {
            let domain = instance.origin.as_deref().unwrap_or(&instance.name);
            let git = instance.git.clone().unwrap_or_else(|| format!("https://git.{domain}"));
            same_host(&git, host)
        })
        .and_then(|instance| instance.token)
        .filter(|token| !token.is_empty())
}

/// One line of scfg as words: quoted ones unquoted, a comment left out,
/// `{` and `}` words of their own.
fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut chars = line.trim().chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            _ if c.is_whitespace() => {
                chars.next();
            }
            '#' if words.is_empty() => break,
            // Double quotes take backslash escapes, single quotes none.
            '"' | '\'' => {
                let quote = c;
                chars.next();
                let mut word = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        _ if c == quote => break,
                        '\\' if quote == '"' => word.extend(chars.next()),
                        _ => word.push(c),
                    }
                }
                words.push(word);
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                words.push(word);
            }
        }
    }
    words
}

// ---- GraphQL ----

/// A GraphQL request's body.
pub fn request(query: &str, variables: Value) -> String {
    serde_json::json!({ "query": query, "variables": variables }).to_string()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawAnswer {
    data: Value,
    errors: Vec<RawError>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawError {
    message: String,
}

/// What an answer's `errors` say, joined; empty when it has none.
pub fn api_message(body: &str) -> String {
    let answer: RawAnswer = serde_json::from_str(body).unwrap_or_default();
    let messages: Vec<&str> = answer.errors.iter().map(|error| error.message.trim()).filter(|text| !text.is_empty()).collect();
    messages.join("; ")
}

/// Why a GraphQL answer has no data to use.
#[derive(Debug, PartialEq, Eq)]
pub enum Unusable {
    /// It came with errors: the service's words.
    Refused(String),
    /// It is not GraphQL's answer.
    Unreadable(String),
}

/// The `data` of a GraphQL answer, unless it came with errors (a missing
/// grant leaves its field null and says so in `errors`).
pub fn data(text: &str) -> Result<Value, Unusable> {
    let answer: RawAnswer = json(text).map_err(Unusable::Unreadable)?;
    let message = api_message(text);
    if !message.is_empty() || !answer.errors.is_empty() {
        return Err(Unusable::Refused(if message.is_empty() { "the request failed".to_owned() } else { message }));
    }
    Ok(answer.data)
}

/// The tickets of a repository's tracker; `name` and `owner` (without its
/// `~`) are its variables.
pub const TICKETS: &str = "query Tickets($owner: String!, $name: String!) { user(username: $owner) { \
    tracker(name: $name) { tickets { cursor results { id subject status created updated \
    submitter { canonicalName } labels { name } } } } } }";

/// The owner's latest build jobs.
pub const JOBS: &str = "query Jobs($owner: String!) { userByName(username: $owner) { \
    jobs { cursor results { id created status note tags } } } }";

/// One page of a cursored list.
#[derive(Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    results: Vec<T>,
    cursor: Option<String>,
}

/// The open tickets in a [`TICKETS`] answer's `data`, and whether the
/// tracker has more; `None` when the user or the tracker does not exist.
pub fn parse_tickets(data: Value) -> Result<Option<(Vec<Issue>, bool)>, String> {
    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct RawEntity {
        canonical_name: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct RawLabel {
        name: String,
    }
    #[derive(Deserialize)]
    struct RawTicket {
        id: u64,
        #[serde(default)]
        subject: String,
        #[serde(default)]
        status: String,
        updated: Option<String>,
        submitter: Option<RawEntity>,
        #[serde(default)]
        labels: Vec<RawLabel>,
    }
    let tickets = &data["user"]["tracker"]["tickets"];
    if tickets.is_null() {
        return Ok(None);
    }
    let page: Page<RawTicket> = serde_json::from_value(tickets.clone()).map_err(|error| error.to_string())?;
    let issues = page
        .results
        .into_iter()
        .filter(|ticket| ticket.status != "RESOLVED")
        .map(|ticket| Issue {
            number: ticket.id,
            title: tidy(&ticket.subject),
            author: ticket.submitter.map(|who| tidy(&who.canonical_name)).filter(|name| !name.is_empty()).unwrap_or_else(|| "Unknown".to_owned()),
            labels: ticket.labels.into_iter().map(|label| tidy(&label.name)).collect(),
            comments: 0,
            updated: timestamp(ticket.updated.as_deref()),
        })
        .collect();
    Ok(Some((issues, page.cursor.is_some())))
}

/// A build job's state.
pub fn job_state(status: &str) -> RunState {
    match status {
        "SUCCESS" => RunState::Success,
        "FAILED" | "TIMEOUT" => RunState::Failure,
        "CANCELLED" => RunState::Cancelled,
        "RUNNING" => RunState::Running,
        // PENDING, QUEUED and whatever comes next.
        _ => RunState::Queued,
    }
}

/// The commit a job was submitted for and its subject, from the note
/// git.sr.ht writes: `[abc1234][0] — [Author][1]`, the subject indented,
/// then `[0]: https://git.sr.ht/~user/name/commit/<id>`.
fn note_commit(note: &str) -> (String, String) {
    let sha = note
        .lines()
        .filter_map(|line| line.trim().strip_prefix("[0]:"))
        .filter_map(|link| link.trim().rsplit_once("/commit/"))
        .map(|(_, id)| id.trim())
        .find(|id| id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_default();
    let subject = note.lines().find(|line| line.starts_with("    ")).map(tidy).unwrap_or_default();
    (sha.to_ascii_lowercase(), subject)
}

/// `.build.yml` or `test.yaml` as "build" or "test".
fn manifest_name(file: &str) -> String {
    let file = file.trim_start_matches('.');
    file.strip_suffix(".yml").or_else(|| file.strip_suffix(".yaml")).unwrap_or(file).to_owned()
}

/// The jobs of `name` in a [`JOBS`] answer's `data`, and whether the owner
/// has more jobs; `None` when the owner does not exist.
pub fn parse_jobs(data: Value, name: &str) -> Result<Option<(Vec<Run>, bool)>, String> {
    #[derive(Deserialize)]
    struct RawJob {
        id: u64,
        created: Option<String>,
        #[serde(default)]
        status: String,
        note: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
    }
    let jobs = &data["userByName"]["jobs"];
    if jobs.is_null() {
        return Ok(None);
    }
    let page: Page<RawJob> = serde_json::from_value(jobs.clone()).map_err(|error| error.to_string())?;
    let runs = page
        .results
        .into_iter()
        // git.sr.ht tags the jobs of a push `<name>/commits[/<branch>]/<manifest>`.
        .filter(|job| job.tags.len() >= 3 && job.tags[0] == name && job.tags[1] == "commits")
        .map(|job| {
            let (head_sha, title) = note_commit(job.note.as_deref().unwrap_or_default());
            let branch = if job.tags.len() >= 4 { branch_name(&job.tags[2]) } else { String::new() };
            Run {
                id: job.id,
                workflow: tidy(&manifest_name(job.tags.last().map(String::as_str).unwrap_or_default())),
                title,
                number: job.id,
                branch,
                event: "push".to_owned(),
                head_sha,
                state: job_state(&job.status),
                outcome: job.status.to_lowercase(),
                when: timestamp(job.created.as_deref()),
            }
        })
        .collect();
    Ok(Some((runs, page.cursor.is_some())))
}

// ---- the account (Preferences > Integrations) ----

/// The signed-in account, from meta.sr.ht.
pub const PROFILE: &str = "query { me { canonicalName username email } }";

/// Its SSH keys.
pub const KEYS: &str = "query { me { sshKeys { cursor results { id created key keyType comment } } } }";

/// Adds one; `key` is its variable, the whole public key line.
pub const ADD_KEY: &str =
    "mutation AddKey($key: String!) { createSSHKey(key: $key) { id created key keyType comment } }";

/// The account in a [`PROFILE`] answer's `data`.
pub fn parse_profile(data: Value) -> Result<Profile, String> {
    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct RawUser {
        canonical_name: String,
        username: String,
        email: String,
    }
    let raw: RawUser = serde_json::from_value(data["me"].clone()).map_err(|error| error.to_string())?;
    let login = tidy(if raw.canonical_name.is_empty() { &raw.username } else { &raw.canonical_name });
    if login.is_empty() {
        return Err("the answer names no account".to_owned());
    }
    let email = Some(tidy(&raw.email)).filter(|email| !email.is_empty());
    Ok(Profile { id: 0, login, name: String::new(), email })
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawKey {
    id: u64,
    created: Option<String>,
    /// The key's base64 body (a whole `type body comment` line on older
    /// servers).
    key: String,
    key_type: Option<String>,
    comment: Option<String>,
}

fn key_model(raw: RawKey) -> SshKey {
    let line = match (&raw.key_type, raw.key.contains(' ')) {
        (Some(kind), false) => format!("{kind} {}", raw.key),
        _ => raw.key.clone(),
    };
    let parts: Vec<&str> = line.split_whitespace().take(2).collect();
    SshKey {
        id: raw.id,
        title: tidy(raw.comment.as_deref().unwrap_or_default()),
        key: parts.join(" "),
        fingerprint: parts.get(1).and_then(|blob| account::fingerprint(blob)).unwrap_or_default(),
        created: timestamp(raw.created.as_deref()),
        expires: 0,
    }
}

/// The keys in a [`KEYS`] answer's `data`.
pub fn parse_keys(data: Value) -> Result<Vec<SshKey>, String> {
    let page: Page<RawKey> = serde_json::from_value(data["me"]["sshKeys"].clone()).map_err(|error| error.to_string())?;
    Ok(page.results.into_iter().map(key_model).collect())
}

/// The key in an [`ADD_KEY`] answer's `data`.
pub fn parse_added(data: Value) -> Result<SshKey, String> {
    serde_json::from_value::<RawKey>(data["createSSHKey"].clone()).map(key_model).map_err(|error| error.to_string())
}

/// The body of an [`ADD_KEY`] request. SourceHut keeps no title beside a
/// key: the title goes in as its comment.
pub fn new_key_json(title: &str, key: &PublicKey) -> String {
    let title: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = if title.is_empty() { key.line() } else { format!("{} {} {title}", key.kind, key.blob) };
    request(ADD_KEY, serde_json::json!({ "key": line }))
}

/// The grant a [`Unusable::Refused`] message says is missing
/// (`SSH_KEYS:RO`), if that is what it says.
pub fn missing_grant(message: &str) -> Option<&'static str> {
    let grant = message.split("missing ").nth(1)?.split_whitespace().next()?;
    ["PROFILE:RO", "SSH_KEYS:RO", "SSH_KEYS:RW", "TICKETS:RO", "TRACKERS:RO", "JOBS:RO"]
        .into_iter()
        .find(|known| *known == grant)
}

// ---- the client ----

pub struct Client {
    api: Api,
}

impl Client {
    pub fn new(repo: RepoId, transport: Box<dyn Transport>, secrets: Box<dyn Secrets>) -> Self {
        // 401: no sign-in; 403: one refused (invalid, expired or revoked).
        Client { api: Api::new(repo, transport, secrets, 0, &[401, 403], api_message) }
    }

    /// The repository's owner without its `~`, as the APIs name users.
    fn owner(&self) -> String {
        self.api.repo.owner.trim_start_matches('~').to_owned()
    }

    /// Asks `query` of the GraphQL API at `url`. Nothing is sent without a
    /// sign-in. Problems leave with tokens scrubbed.
    fn ask(&mut self, url: &str, query: &str, variables: Value, now: u64) -> Result<Value, Problem> {
        let asked = self.api.post(url, &request(query, variables), true, now).and_then(|response| {
            match self.api.classify(response)? {
                Got::Body(response) => data(&response.body).map_err(|unusable| match unusable {
                    Unusable::Refused(message) => Problem::Refused(message),
                    Unusable::Unreadable(error) => self.api.unreadable(error),
                }),
                Got::Unchanged | Got::Nothing => Ok(Value::Null),
            }
        });
        asked.map_err(|problem| self.api.scrub(problem))
    }
}

impl Backend for Client {
    /// Patches go by email: there is nothing to list.
    fn pulls(&mut self, _now: u64) -> Outcome<PullRequest> {
        self.api.start_round();
        Outcome::Fresh { items: Vec::new(), more: false }
    }

    fn issues(&mut self, now: u64) -> Outcome<Issue> {
        let variables = serde_json::json!({ "owner": self.owner(), "name": self.api.repo.name });
        let parsed = self.ask(TODO_API, TICKETS, variables, now).and_then(|data| {
            parse_tickets(data).map_err(|error| self.api.unreadable(error))
        });
        match parsed {
            Ok(Some((items, more))) => Outcome::Fresh { items, more },
            Ok(None) => Outcome::Absent,
            Err(problem) => Outcome::Failed(self.api.scrub(problem)),
        }
    }

    fn runs(&mut self, now: u64) -> Outcome<Run> {
        let variables = serde_json::json!({ "owner": self.owner() });
        let name = self.api.repo.name.clone();
        let parsed =
            self.ask(BUILDS_API, JOBS, variables, now).and_then(|data| parse_jobs(data, &name).map_err(|error| self.api.unreadable(error)));
        match parsed {
            Ok(Some((items, more))) if !items.is_empty() => Outcome::Fresh { items, more },
            Ok(_) => Outcome::Absent,
            Err(problem) => Outcome::Failed(self.api.scrub(problem)),
        }
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

    fn create_pull(&mut self, _new: &NewPull, _now: u64) -> Result<PullRequest, Problem> {
        Err(Problem::Failed("SourceHut has no pull requests: send the branch as patches with git send-email".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fake, Log, NOW, OTHER, SECRET, Script, reply};
    use super::*;

    fn repo() -> RepoId {
        parse_remote("https://git.sr.ht/~ada/app", &BTreeMap::new()).unwrap()
    }

    fn scripted(replies: Vec<Result<HttpResponse, String>>, secrets: Fake) -> (Client, Log) {
        let sent = Log::default();
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        (Client::new(repo(), Box::new(transport), Box::new(secrets)), sent)
    }

    fn with_token() -> Fake {
        Fake { env: vec![("SRHT_TOKEN", SECRET)], ..Default::default() }
    }

    fn failure<T: std::fmt::Debug>(outcome: Outcome<T>) -> Problem {
        match outcome {
            Outcome::Failed(problem) => problem,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    // Answers in the shapes of todo.sr.ht's and builds.sr.ht's GraphQL
    // schemas; the notes and tags as git.sr.ht writes them on a push.
    pub(super) const TICKETS_ANSWER: &str = r#"{"data": {"user": {"tracker": {"tickets": {
      "cursor": "41",
      "results": [
        {"id": 44, "subject": "Crash on an empty repository", "status": "REPORTED",
         "created": "1970-01-01T00:00:30Z", "updated": "1970-01-01T00:01:00Z",
         "submitter": {"canonicalName": "~grace"}, "labels": [{"name": "bug"}]},
        {"id": 43, "subject": "Fixed already", "status": "RESOLVED", "submitter": {"canonicalName": "~ada"}, "labels": []},
        {"id": 42, "subject": "Docs typo", "status": "CONFIRMED", "submitter": {"canonicalName": "linus <linus@example.org>"},
         "labels": []}
      ]}}}}}"#;

    pub(super) const JOBS_ANSWER: &str = r#"{"data": {"userByName": {"jobs": {"cursor": "2001", "results": [
      {"id": 2004, "created": "1970-01-01T02:00:00Z", "status": "RUNNING",
       "note": "[1111111][0] — [Ada][1]\n\n    Add dark mode\n\n[0]: https://git.sr.ht/~ada/app/commit/1111111111111111111111111111111111111111\n[1]: mailto:ada@example.org",
       "tags": ["app", "commits", "feature/dark", ".build.yml"]},
      {"id": 2003, "created": "1970-01-01T01:00:00Z", "status": "FAILED", "note": null,
       "tags": ["other", "commits", "main", ".build.yml"]},
      {"id": 2002, "created": "1970-01-01T00:30:00Z", "status": "SUCCESS",
       "note": "[4444444][0] — [Ada][1]\n\n    Release 1.2\n\n[0]: https://git.sr.ht/~ada/app/commit/4444444444444444444444444444444444444444\n[1]: mailto:ada@example.org",
       "tags": ["app", "commits", "lint.yaml"]},
      {"id": 2001, "created": "1970-01-01T00:10:00Z", "status": "TIMEOUT", "note": "by hand", "tags": ["app"]}
    ]}}}}"#;

    #[test]
    fn remotes_on_git_sr_ht_are_recognised() {
        for url in [
            "https://git.sr.ht/~ada/app",
            "https://git.sr.ht/~ada/app.git",
            "https://git.sr.ht/~ada/app/",
            "git@git.sr.ht:~ada/app",
            "ssh://git@git.sr.ht/~ada/app",
        ] {
            assert_eq!(parse_remote(url, &BTreeMap::new()), Some(repo()), "{url}");
        }
        let repo = repo();
        assert_eq!((repo.service, repo.host.as_str(), repo.slug().as_str()), (Service::SourceHut, "git.sr.ht", "~ada/app"));
        assert!(!repo.self_hosted());
        assert_eq!((repo.site(), repo.api_base()), ("SourceHut".to_owned(), GIT_API.to_owned()));
        for url in [
            "https://git.sr.ht/ada/app",
            "https://git.sr.ht/~ada",
            "https://git.sr.ht/~ada/~app",
            "https://git.sr.ht/~ada/app/tree/main",
            "https://hg.sr.ht/~ada/app",
            "https://git.sr.ht.evil.example/~ada/app",
        ] {
            assert_eq!(parse_remote(url, &BTreeMap::new()), None, "{url}");
        }
        // Its other hosts are not servers to connect.
        assert!(offer_for(&RemoteInfo { name: "origin".into(), url: "https://hg.sr.ht/~ada/app".into() }).is_none());
        assert!(offer_for(&RemoteInfo { name: "origin".into(), url: "https://git.sr.ht/~ada/app".into() }).is_none());
    }

    #[test]
    fn links_and_words_follow_sourcehuts() {
        let repo = repo();
        assert_eq!(repo.home_url(), "https://git.sr.ht/~ada/app");
        assert_eq!(repo.pulls_url(), "https://git.sr.ht/~ada/app");
        assert_eq!(repo.compare_url("main", "feature"), "https://git.sr.ht/~ada/app");
        assert_eq!(repo.issues_url(), "https://todo.sr.ht/~ada/app");
        assert_eq!(repo.issue_url(44), "https://todo.sr.ht/~ada/app/44");
        assert_eq!(repo.runs_url(), "https://builds.sr.ht/~ada/app");
        assert_eq!(repo.run_url(2004), "https://builds.sr.ht/~ada/job/2004");
        assert_eq!(repo.clone_url().as_deref(), Some("https://git.sr.ht/~ada/app"));
        assert_eq!(repo.sibling("~grace/app"), None);
        let service = Service::SourceHut;
        assert_eq!((service.issues_title(), service.ci_title()), ("TICKETS", "BUILDS"));
        assert!(!service.has_pulls() && service.has_issues() && service.needs_sign_in() && !service.has_pull_refs());
        assert_eq!(Problem::NotSignedIn.line(service), "Not signed in");
        assert!(Problem::NotSignedIn.detail(service).contains("SRHT_TOKEN"));
        assert_eq!(Auth::Anonymous.describe(service), "Not signed in: nothing shown");
        assert_eq!(Auth::SignedIn(TokenSource::Cli).describe(service), "Signed in through hut's sign-in");
    }

    #[test]
    fn tickets_and_builds_are_asked_of_their_own_services_with_the_token() {
        let (mut client, sent) = scripted(vec![reply(200, &[], TICKETS_ANSWER), reply(200, &[], JOBS_ANSWER)], with_token());
        assert_eq!(client.pulls(NOW), Outcome::Fresh { items: Vec::new(), more: false });
        let Outcome::Fresh { items: tickets, more: true } = client.issues(NOW) else { panic!("no tickets") };
        assert_eq!(tickets.iter().map(|ticket| ticket.number).collect::<Vec<_>>(), [44, 42]);
        assert_eq!((tickets[0].author.as_str(), tickets[0].labels.clone(), tickets[0].updated), ("~grace", vec!["bug".to_owned()], 60));
        let Outcome::Fresh { items: runs, more: true } = client.runs(NOW) else { panic!("no builds") };
        assert_eq!(runs.iter().map(|run| run.id).collect::<Vec<_>>(), [2004, 2002]);
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("SRHT_TOKEN")));

        let sent = sent.lock().unwrap();
        assert_eq!(sent.iter().map(|request| request.url.as_str()).collect::<Vec<_>>(), [TODO_API, BUILDS_API]);
        assert!(sent.iter().all(|request| request.token.as_deref() == Some(SECRET)));
        let body: Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!((body["query"].as_str(), body["variables"].clone()), (Some(TICKETS), serde_json::json!({"owner": "ada", "name": "app"})));
        let body: Value = serde_json::from_str(sent[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["variables"], serde_json::json!({"owner": "ada"}));
    }

    #[test]
    fn a_push_build_is_read_from_its_tags_and_note() {
        let data = super::data(JOBS_ANSWER).unwrap();
        let (runs, _) = parse_jobs(data, "app").unwrap().unwrap();
        let first = &runs[0];
        assert_eq!((first.workflow.as_str(), first.branch.as_str(), first.title.as_str()), ("build", "feature/dark", "Add dark mode"));
        assert_eq!((first.head_sha.as_str(), first.state, first.outcome.as_str()), ("1111111111111111111111111111111111111111", RunState::Running, "running"));
        assert_eq!((first.when, first.event.as_str()), (7200, "push"));
        // A tag pushed: no branch.
        assert_eq!((runs[1].workflow.as_str(), runs[1].branch.as_str(), runs[1].state), ("lint", "", RunState::Success));
        assert_eq!(checks_for("4444444444444444444444444444444444444444", &runs), Some(Checks::Passing));
        for (status, expected) in [
            ("PENDING", RunState::Queued),
            ("QUEUED", RunState::Queued),
            ("RUNNING", RunState::Running),
            ("SUCCESS", RunState::Success),
            ("FAILED", RunState::Failure),
            ("TIMEOUT", RunState::Failure),
            ("CANCELLED", RunState::Cancelled),
        ] {
            assert_eq!(job_state(status), expected, "{status}");
        }
        assert_eq!(note_commit("[0]: https://git.sr.ht/~ada/app/commit/zz"), (String::new(), String::new()));
    }

    #[test]
    fn a_repository_without_a_tracker_or_builds_shows_neither() {
        let no_tracker = r#"{"data": {"user": {"tracker": null}}}"#;
        let no_builds = r#"{"data": {"userByName": {"jobs": {"cursor": null, "results": [
          {"id": 9, "status": "SUCCESS", "tags": ["other", "commits", "main", ".build.yml"]}]}}}}"#;
        let (mut client, _) = scripted(vec![reply(200, &[], no_tracker), reply(200, &[], no_builds)], with_token());
        assert_eq!(client.issues(NOW), Outcome::Absent);
        assert_eq!(client.runs(NOW), Outcome::Absent);
        let mut feed: Feed<Issue> = Feed::default();
        feed.apply(Outcome::Absent);
        assert!(feed.absent && feed.loaded && feed.items.is_empty());
        let (mut client, _) = scripted(vec![reply(200, &[], r#"{"data": {"user": null}}"#)], with_token());
        assert_eq!(client.issues(NOW), Outcome::Absent);
    }

    #[test]
    fn nothing_is_asked_without_a_sign_in_and_a_refused_one_gives_way() {
        let (mut client, sent) = scripted(Vec::new(), Fake::default());
        assert_eq!(failure(client.issues(NOW)), Problem::NotSignedIn);
        assert_eq!(failure(client.runs(NOW)), Problem::NotSignedIn);
        assert!(sent.lock().unwrap().is_empty());
        assert_eq!(client.auth(), Auth::Anonymous);

        let expired = r#"{"errors": [{"message": "Invalid or expired OAuth 2.0 bearer token", "extensions": {"code": "ERR_UNAUTHORIZED"}}]}"#;
        let secrets = Fake { helper: Some(OTHER), ..with_token() };
        let (mut client, sent) = scripted(vec![reply(403, &[], expired), reply(200, &[], TICKETS_ANSWER)], secrets);
        assert!(matches!(client.issues(NOW), Outcome::Fresh { .. }));
        let tokens: Vec<Option<String>> = sent.lock().unwrap().iter().map(|request| request.token.clone()).collect();
        assert_eq!(tokens, [Some(SECRET.to_owned()), Some(OTHER.to_owned())]);
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::CredentialHelper));

        // Every token refused: none left, and nothing sent without one.
        let (mut client, sent) = scripted(vec![reply(403, &[], expired)], with_token());
        assert_eq!(failure(client.issues(NOW)), Problem::SignInRejected);
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_missing_grant_is_shown_in_sourcehuts_words() {
        let denied = r#"{"data": {"user": {"tracker": null}}, "errors": [{"message": "Access denied, missing TICKETS:RO grant",
          "path": ["user", "tracker"], "extensions": {"code": "ERR_ACCESS_DENIED"}}]}"#;
        let (mut client, _) = scripted(vec![reply(200, &[], denied), reply(200, &[], "<html>")], with_token());
        assert_eq!(failure(client.issues(NOW)).line(Service::SourceHut), "SourceHut refused: Access denied, missing TICKETS:RO grant");
        assert!(matches!(failure(client.runs(NOW)), Problem::Failed(text) if text.starts_with("Unreadable answer from SourceHut")));
        assert_eq!(missing_grant("Access denied, missing SSH_KEYS:RW grant"), Some("SSH_KEYS:RW"));
        assert_eq!(missing_grant("Access denied"), None);
    }

    #[test]
    fn tokens_go_to_sourcehuts_endpoints_and_nowhere_else() {
        let wire = super::super::wire(Service::SourceHut, GIT_API.to_owned());
        let token = Token::new(SECRET).unwrap();
        let header = |url: &str| wire.authorization(&HttpRequest { url, etag: None, token: Some(&token) });
        for url in ENDPOINTS {
            assert!(header(url).is_some(), "{url}");
        }
        for url in ["https://git.sr.ht/~ada/app", "https://lists.sr.ht/query", "https://todo.sr.ht/query/x", "https://evil.example/query"] {
            assert!(header(url).is_none(), "{url}");
        }
    }

    #[test]
    fn sign_ins_come_from_the_variable_then_hut_then_git() {
        let repo = repo();
        let (_, source) = find_token(&with_token(), &repo, &[]).unwrap();
        assert_eq!(source, TokenSource::Env("SRHT_TOKEN"));
        let hut = Fake { cli: Some(OTHER), ..Default::default() };
        let (token, source) = find_token(&hut, &repo, &[]).unwrap();
        assert_eq!((source, token.authorization()), (TokenSource::Cli, format!("Bearer {OTHER}")));
        assert_eq!(hut.asked(), ["SRHT_TOKEN", "hut git.sr.ht"]);
        let github = Fake { env: vec![("GITHUB_TOKEN", SECRET), ("GITLAB_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(find_token(&github, &repo, &[]), None);
    }

    #[test]
    fn huts_configuration_gives_the_token_of_the_instance_for_git_sr_ht() {
        let config = r#"
# Written by hut init.
instance "sr.ht" {
	origin "sr.ht"
	access-token "srht-token-1"
}
instance 'example' {
	origin example.org
	access-token "example-token"
	git {
		origin "https://code.example.org"
	}
}
instance "cmd" {
	origin "cmd.example"
	access-token-cmd pass token
}
"#;
        assert_eq!(hut_login_token(config, "git.sr.ht").as_deref(), Some("srht-token-1"));
        assert_eq!(hut_login_token(config, "code.example.org").as_deref(), Some("example-token"));
        assert_eq!(hut_login_token(config, "git.example.org"), None);
        assert_eq!(hut_login_token(config, "git.cmd.example"), None);
        // An instance named after its origin, without the origin line.
        assert_eq!(hut_login_token("instance \"sr.ht\" {\n  access-token \"t2\"\n}\n", "git.sr.ht").as_deref(), Some("t2"));
        assert_eq!(hut_login_token("", "git.sr.ht"), None);
        assert_eq!(words(r#"access-token "a \"b\" c" # not a comment"#), ["access-token", "a \"b\" c", "#", "not", "a", "comment"]);
    }

    #[test]
    fn the_offline_fixture_answers_each_service_from_its_own_file() {
        assert_eq!(fixture_name(TODO_API), Some("todo"));
        assert_eq!(fixture_name(BUILDS_API), Some("builds"));
        assert_eq!(fixture_name("https://todo.sr.ht.evil.example/query"), None);
        assert_eq!(fixture_name("https://api.github.com/graphql"), None);

        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-srht-canned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("todo.json"), TICKETS_ANSWER).unwrap();
        std::fs::write(dir.join("builds.json"), JOBS_ANSWER).unwrap();
        let canned = Canned { dir: dir.clone(), created: false };
        let mut client = Client::new(repo(), Box::new(canned), Box::new(FixtureSignIn));
        assert!(matches!(client.issues(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Cli));
        // Signed out, nothing is asked at all.
        let mut client = Client::new(repo(), Box::new(Canned { dir: dir.clone(), created: false }), Box::new(NoSecrets));
        assert_eq!(failure(client.issues(NOW)), Problem::NotSignedIn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_a_pull_request_is_not_offered_and_says_why() {
        let (mut client, sent) = scripted(Vec::new(), with_token());
        let new = NewPull { head: "x".into(), base: "main".into(), title: "x".into(), body: String::new(), draft: false };
        assert!(matches!(client.create_pull(&new, NOW), Err(Problem::Failed(text)) if text.contains("git send-email")));
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn the_account_and_its_keys_are_read_in_meta_sr_hts_shape() {
        let profile = parse_profile(super::data(r#"{"data": {"me": {"canonicalName": "~ada", "username": "ada", "email": "ada@example.org"}}}"#).unwrap()).unwrap();
        assert_eq!((profile.login.as_str(), profile.display_name(), profile.email.as_deref()), ("~ada", "~ada", Some("ada@example.org")));
        assert!(parse_profile(Value::Null).is_err());

        let keys = parse_keys(
            super::data(
                r#"{"data": {"me": {"sshKeys": {"cursor": null, "results": [
                  {"id": 5, "created": "1970-01-02T00:00:00Z", "keyType": "ssh-ed25519",
                   "key": "AAAAC3NzaC1lZDI1NTE5AAAAIJ6/rvs4h30pDKGBvPxCCedyMBv2RjD2wg++vAYe/VwB", "comment": "Laptop"},
                  {"id": 6, "key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ6/rvs4h30pDKGBvPxCCedyMBv2RjD2wg++vAYe/VwB old", "comment": null}
                ]}}}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!((keys[0].id, keys[0].title.as_str(), keys[0].created), (5, "Laptop", 86_400));
        assert_eq!(keys[0].fingerprint, "SHA256:rLmucD0ZT+3NWpg/ewTzpahexLO2rz/okUf2br8ey0Y");
        assert_eq!(keys[1].key, keys[0].key);

        let key = account::parse_public_key("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ6/rvs4h30pDKGBvPxCCedyMBv2RjD2wg++vAYe/VwB ada@laptop").unwrap();
        let body: Value = serde_json::from_str(&new_key_json(" My  laptop ", &key)).unwrap();
        assert_eq!(body["query"].as_str(), Some(ADD_KEY));
        assert_eq!(body["variables"]["key"].as_str(), Some("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ6/rvs4h30pDKGBvPxCCedyMBv2RjD2wg++vAYe/VwB My laptop"));
        let added = parse_added(super::data(r#"{"data": {"createSSHKey": {"id": 7, "keyType": "ssh-ed25519", "key": "AAAAC3NzaC1lZDI1NTE5AAAAIJ6/rvs4h30pDKGBvPxCCedyMBv2RjD2wg++vAYe/VwB", "comment": "My laptop"}}}"#).unwrap()).unwrap();
        assert_eq!((added.id, added.title.as_str()), (7, "My laptop"));
    }
}
