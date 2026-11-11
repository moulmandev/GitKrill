//! Azure DevOps Services: Azure Repos pull requests, Azure Pipelines builds
//! as the CI runs, and Azure Boards work items in place of issues, through
//! the REST API (7.1) on dev.azure.com.
//!
//! A repository is `organisation/project/repository`. Its remotes come in
//! several forms, all read here: `https://dev.azure.com/{org}/{project}/_git/{repo}`
//! (often with `{org}@` in front), `git@ssh.dev.azure.com:v3/{org}/{project}/{repo}`,
//! and the older `https://{org}.visualstudio.com/[DefaultCollection/]{project}/_git/{repo}`
//! and `{org}@vs-ssh.visualstudio.com:v3/...`. Every one becomes the same
//! [`RepoId`] on dev.azure.com, which serves all organisations. Names may
//! hold spaces and other characters; they are kept `%`-escaped.
//!
//! Sign-ins: a personal access token in `AZURE_DEVOPS_EXT_PAT` (as the Azure
//! CLI's DevOps extension reads it; sent as HTTP Basic), the Azure CLI's own
//! sign-in (`az login`; a Microsoft Entra token, sent as a bearer token),
//! then Git's credential manager, asked about the repository's address
//! because it keeps one sign-in per organisation.
//!
//! There is no ref for a pull request's head on the remote (only for the
//! merge result), so a pull request is checked out from its branch.

use std::process::Command;

use serde::Deserialize;

use super::account::Profile;
use super::signin::{Api, Got, json, plain_message};
use super::*;

/// Where every organisation's API is, below its name. A token is only ever
/// sent below it.
pub const API: &str = "https://dev.azure.com";

/// The host every Azure DevOps repository is known by.
pub const HOST: &str = "dev.azure.com";

/// Where the signed-in user's profile is: outside any organisation.
pub const PROFILE_API: &str = "https://app.vssps.visualstudio.com";

/// Sent with every request. Without the second, a refused sign-in is
/// answered with a redirect to the sign-in page instead of a 401.
pub const HEADERS: &[(&str, &str)] = &[("Accept", "application/json"), ("X-TFS-FedAuthRedirect", "Suppress")];

/// Azure DevOps' id with Microsoft Entra: what the Azure CLI is asked for a
/// token for.
const RESOURCE: &str = "499b84ac-1321-427f-aa17-267ca6975798";

const VERSION: &str = "api-version=7.1";

/// The host names Azure DevOps remotes use.
pub fn is_host(name: &str) -> bool {
    matches!(name, "dev.azure.com" | "ssh.dev.azure.com" | "vs-ssh.visualstudio.com")
        || name.strip_suffix(".visualstudio.com").is_some_and(organisation_name)
}

/// An organisation's name: letters, digits and `-`, not at either end.
fn organisation_name(name: &str) -> bool {
    (1..=50).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// A project or repository name from a URL: `%`-escapes undone, checked,
/// and escaped again the one way [`url_escape`] does, so that the same name
/// always reads the same. `None` for anything a name cannot be.
fn name_part(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let name = String::from_utf8(decoded).ok()?;
    let fits = !name.trim().is_empty()
        && name.len() <= 256
        && name != "."
        && name != ".."
        && !name.starts_with('_')
        && name.chars().all(|c| !c.is_control() && !"/\\?#%:*<>|\"".contains(c));
    fits.then(|| url_escape(&name))
}

/// The Azure DevOps repository a remote URL points at, in any of its forms
/// (see the module documentation); `None` for anything else.
pub fn parse_remote(url: &str) -> Option<RepoId> {
    let url = url.trim();
    let (authority, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = scheme.to_ascii_lowercase();
            if !["https", "http", "ssh", "git+ssh", "ssh+git"].contains(&scheme.as_str()) {
                return None;
            }
            rest.split_once('/')?
        }
        // scp-like: [user@]host:path.
        None => {
            let (left, path) = url.split_once(':')?;
            if left.contains('/') || left.contains('\\') {
                return None;
            }
            (left, path)
        }
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let name = match host.split_once(':') {
        Some((name, port)) if port.is_empty() || port.parse::<u16>().is_ok() => name,
        Some(_) => return None,
        None => host,
    };
    let name = name.to_ascii_lowercase();
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let (organisation, project, repository) = match (name.as_str(), parts.as_slice()) {
        ("dev.azure.com", [org, project, "_git", repo]) => (org.to_string(), *project, *repo),
        // A repository named after its project can leave the project out.
        ("dev.azure.com", [org, "_git", repo]) => (org.to_string(), *repo, *repo),
        ("ssh.dev.azure.com" | "vs-ssh.visualstudio.com", ["v3", org, project, repo]) => (org.to_string(), *project, *repo),
        (host, rest) => {
            let org = host.strip_suffix(".visualstudio.com").filter(|org| *org != "vs-ssh" && organisation_name(org))?;
            let rest = match rest {
                ["DefaultCollection", rest @ ..] | rest => rest,
            };
            match rest {
                [project, "_git", repo] => (org.to_owned(), *project, *repo),
                ["_git", repo] => (org.to_owned(), *repo, *repo),
                _ => return None,
            }
        }
    };
    if !organisation_name(&organisation) {
        return None;
    }
    let (project, repository) = (name_part(project)?, name_part(repository)?);
    Some(RepoId { service: Service::AzureDevOps, host: HOST.to_owned(), owner: format!("{organisation}/{project}"), name: repository })
}

/// `(organisation, project)`.
fn split_owner(repo: &RepoId) -> (&str, &str) {
    repo.owner.split_once('/').unwrap_or((&repo.owner, ""))
}

/// A page of the repository: `.../{org}/{project}/_git/{repo}/{path}`.
pub fn repo_page(repo: &RepoId, path: &str) -> String {
    format!("https://{}/{}/_git/{}/{path}", repo.host, repo.owner, repo.name)
}

/// A page of the project: `.../{org}/{project}/{path}`.
pub fn project_page(repo: &RepoId, path: &str) -> String {
    format!("https://{}/{}/{path}", repo.host, repo.owner)
}

/// The repository's address below the host, as Git's credential manager is
/// asked about it (it keeps a sign-in per organisation); empty for an
/// account that is not about one repository.
fn credential_path(repo: &RepoId) -> String {
    if repo.owner.is_empty() || repo.name.is_empty() {
        return String::new();
    }
    let (organisation, project) = split_owner(repo);
    format!("{organisation}/{project}/_git/{}", repo.name)
}

/// Whether a password is a Microsoft Entra (OAuth) token rather than a
/// personal access token: a JSON web token, three base64 parts.
fn is_jwt(secret: &str) -> bool {
    secret.starts_with("eyJ") && secret.split('.').count() == 3
}

/// Looks for a sign-in for Azure DevOps, most explicit first, passing over
/// the ones already `rejected`.
pub fn find_token(secrets: &dyn Secrets, repo: &RepoId, rejected: &[Token]) -> Option<(Token, TokenSource)> {
    let fresh = |token: &Token| !rejected.contains(token);
    // A personal access token is the password of Basic sign-in; any user
    // name goes.
    if let Some(token) = secrets.env("AZURE_DEVOPS_EXT_PAT").and_then(|raw| Token::basic("", &raw)).filter(fresh) {
        return Some((token, TokenSource::Env("AZURE_DEVOPS_EXT_PAT")));
    }
    if let Some(token) = secrets.cli(repo.service, &repo.host).and_then(|raw| Token::new(&raw)).filter(fresh) {
        return Some((token, TokenSource::Cli));
    }
    let (user, password) = secrets.credential(&repo.host, &credential_path(repo))?;
    let token = if is_jwt(password.trim()) { Token::new(&password) } else { Token::basic(&user, &password) };
    token.filter(fresh).map(|token| (token, TokenSource::CredentialHelper))
}

/// `az account get-access-token` for Azure DevOps, printing the token alone.
/// It never prompts: without `az login` it fails. Its telemetry is off.
pub fn cli_command() -> Command {
    // The Azure CLI is a batch file on Windows.
    let mut cmd = Command::new(if cfg!(windows) { "az.cmd" } else { "az" });
    cmd.args(["account", "get-access-token", "--resource", RESOURCE, "--query", "accessToken", "--output", "tsv"])
        .env("AZURE_CORE_COLLECT_TELEMETRY", "false")
        .env("AZURE_CORE_ONLY_SHOW_ERRORS", "true")
        .env("AZURE_CORE_NO_COLOR", "true");
    cmd
}

// ---- JSON ----

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawIdentity {
    display_name: String,
    unique_name: String,
}

fn identity_name(identity: Option<RawIdentity>) -> String {
    let identity = identity.unwrap_or_default();
    [identity.display_name, identity.unique_name]
        .into_iter()
        .find(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Unknown".to_owned())
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawCommit {
    commit_id: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawProject {
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRepository {
    id: String,
    name: String,
    project: RawProject,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawFork {
    repository: RawRepository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPull {
    pull_request_id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    is_draft: bool,
    created_by: Option<RawIdentity>,
    #[serde(default)]
    source_ref_name: String,
    #[serde(default)]
    target_ref_name: String,
    last_merge_source_commit: Option<RawCommit>,
    /// Present when the source branch is in a fork.
    fork_source: Option<RawFork>,
    /// Pull requests have no "last updated"; this is when it was opened.
    creation_date: Option<String>,
    #[serde(default)]
    reviewers: Vec<RawReviewer>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawReviewer {
    /// 10 approved, 5 approved with suggestions, 0 no vote, -5 waiting for
    /// the author, -10 rejected.
    vote: i32,
    is_required: bool,
}

/// What the reviewers' votes come to: a rejection or "waiting for author"
/// wins, then required reviewers who have not approved, then approvals.
fn review(reviewers: &[RawReviewer]) -> (Review, Option<Approvals>) {
    let given = reviewers.iter().filter(|r| r.vote >= 5).count() as u32;
    let left = reviewers.iter().filter(|r| r.is_required && r.vote < 5).count() as u32;
    let review = if reviewers.iter().any(|r| r.vote < 0) {
        Review::ChangesRequested
    } else if left > 0 {
        Review::Required
    } else if given > 0 {
        Review::Approved
    } else {
        Review::Unreviewed
    };
    (review, Some(Approvals { given, left }))
}

/// A list answer: `{"count": n, "value": [...]}`.
#[derive(Deserialize)]
struct Values<T> {
    value: Vec<T>,
}

/// `refs/heads/feature` as `feature`.
fn branch_name(reference: &str) -> String {
    reference.strip_prefix("refs/heads/").unwrap_or(reference).to_owned()
}

// `..Default::default()` keeps fields added to `PullRequest` later at "unknown".
#[allow(clippy::needless_update, reason = "PullRequest gains fields this backend leaves unknown")]
fn pull_request(pull: RawPull, repo: &RepoId) -> PullRequest {
    let organisation = split_owner(repo).0;
    let head_repo = match pull.fork_source {
        None => Some(repo.slug()),
        Some(fork) => {
            let project = &fork.repository.project.name;
            let named = !fork.repository.name.is_empty() && !project.is_empty();
            named.then(|| format!("{organisation}/{}/{}", url_escape(project), url_escape(&fork.repository.name)))
        }
    };
    let (review, approvals) = review(&pull.reviewers);
    PullRequest {
        number: pull.pull_request_id,
        title: pull.title,
        author: identity_name(pull.created_by),
        draft: pull.is_draft,
        head_ref: branch_name(&pull.source_ref_name),
        head_sha: pull.last_merge_source_commit.map(|commit| commit.commit_id).unwrap_or_default(),
        head_repo,
        base_ref: branch_name(&pull.target_ref_name),
        updated: timestamp(pull.creation_date.as_deref()),
        review,
        approvals,
        ..Default::default()
    }
}

/// How many items a list shows; one more is asked for, to know whether
/// there are more.
const PULLS: usize = 30;
const ITEMS: usize = 30;
const BUILDS: usize = 20;

/// Keeps the first `limit` of `items`, and says whether there were more.
fn first<T>(mut items: Vec<T>, limit: usize) -> (Vec<T>, bool) {
    let more = items.len() > limit;
    items.truncate(limit);
    (items, more)
}

pub fn parse_pulls(text: &str, repo: &RepoId) -> Result<(Vec<PullRequest>, bool), String> {
    let pulls = json::<Values<RawPull>>(text)?.value.into_iter().map(|pull| pull_request(pull, repo)).collect();
    Ok(first(pulls, PULLS))
}

/// One pull request, as the API answers its creation.
pub fn parse_pull(text: &str, repo: &RepoId) -> Result<PullRequest, String> {
    json::<RawPull>(text).map(|pull| pull_request(pull, repo))
}

/// The id of the repository named `name` (escaped, as in [`RepoId::name`])
/// in a list of the project's repositories.
pub fn repository_id(text: &str, name: &str) -> Result<Option<String>, String> {
    let repositories = json::<Values<RawRepository>>(text)?.value;
    let id = repositories
        .into_iter()
        .find(|repository| url_escape(&repository.name).eq_ignore_ascii_case(name))
        .map(|repository| repository.id)
        // A GUID, put in an address as it is.
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'));
    Ok(id)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawDefinition {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBuild {
    id: u64,
    status: Option<String>,
    result: Option<String>,
    #[serde(default)]
    definition: RawDefinition,
    source_branch: Option<String>,
    source_version: Option<String>,
    reason: Option<String>,
    #[serde(default)]
    trigger_info: BTreeMap<String, serde_json::Value>,
    queue_time: Option<String>,
}

/// A build's state from its `status` and, once completed, its `result`.
pub fn build_state(status: &str, result: Option<&str>) -> RunState {
    match (status, result) {
        ("completed", Some("succeeded")) => RunState::Success,
        // Some steps failed and were allowed to.
        ("completed", Some("failed" | "partiallySucceeded")) => RunState::Failure,
        ("completed", Some("canceled")) => RunState::Cancelled,
        ("completed", _) => RunState::Skipped,
        ("inProgress", _) => RunState::Running,
        ("cancelling", _) => RunState::Cancelled,
        // notStarted, postponed, none and whatever comes next.
        _ => RunState::Queued,
    }
}

/// `partiallySucceeded` as "partially succeeded", `individualCI` as
/// "individual CI".
fn words(camel: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = camel.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let upper = c.is_ascii_uppercase();
        let after_lower = i > 0 && chars[i - 1].is_ascii_lowercase();
        if upper && after_lower {
            out.push(' ');
        }
        // Keep acronyms (CI) as they are.
        let acronym = upper && (chars.get(i + 1).is_some_and(char::is_ascii_uppercase) || (i > 0 && chars[i - 1].is_ascii_uppercase()));
        out.push(if acronym { c } else { c.to_ascii_lowercase() });
    }
    out
}

/// `refs/heads/main` as `main`, `refs/pull/12/merge` as `!12`, a tag by
/// its name.
fn build_branch(reference: &str) -> String {
    if let Some(number) = reference.strip_prefix("refs/pull/").and_then(|rest| rest.split('/').next())
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
    {
        return format!("!{number}");
    }
    let name = reference.strip_prefix("refs/heads/").or_else(|| reference.strip_prefix("refs/tags/"));
    name.unwrap_or(reference).to_owned()
}

pub fn parse_builds(text: &str) -> Result<(Vec<Run>, bool), String> {
    let builds = json::<Values<RawBuild>>(text)?.value;
    let runs = builds
        .into_iter()
        .map(|build| {
            let info = |key: &str| build.trigger_info.get(key).and_then(serde_json::Value::as_str).map(str::to_owned);
            let status = build.status.unwrap_or_default();
            let state = build_state(&status, build.result.as_deref());
            Run {
                id: build.id,
                workflow: Some(build.definition.name).filter(|name| !name.is_empty()).unwrap_or_else(|| "Pipeline".to_owned()),
                title: info("ci.message").or_else(|| info("pr.title")).unwrap_or_default(),
                number: build.id,
                branch: build_branch(build.source_branch.as_deref().unwrap_or_default()),
                event: words(build.reason.as_deref().unwrap_or_default()),
                // A pull request build runs on the merge result; the head
                // it was for is in its trigger.
                head_sha: info("pr.sourceSha").or(build.source_version).unwrap_or_default(),
                state,
                outcome: words(build.result.as_deref().unwrap_or(&status)),
                when: timestamp(build.queue_time.as_deref()),
            }
        })
        .collect();
    Ok(first(runs, BUILDS))
}

/// The work item query: the project's items that are not finished, most
/// recently changed first. The finished states of the standard processes
/// (Agile, Scrum, CMMI, Basic) are left out.
const OPEN_ITEMS: &str = "SELECT [System.Id] FROM WorkItems WHERE [System.TeamProject] = @project \
    AND [System.State] NOT IN ('Closed', 'Done', 'Removed', 'Resolved', 'Completed', 'Cut') \
    ORDER BY [System.ChangedDate] DESC";

/// The ids a work item query found, and whether there are more.
pub fn parse_query(text: &str) -> Result<(Vec<u64>, bool), String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawRef {
        id: u64,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawQuery {
        work_items: Vec<RawRef>,
    }
    let ids = json::<RawQuery>(text)?.work_items.into_iter().map(|item| item.id).collect();
    Ok(first(ids, ITEMS))
}

/// The fields asked for of each work item.
const FIELDS: &str =
    "System.Title,System.CreatedBy,System.Tags,System.CommentCount,System.ChangedDate,System.WorkItemType";

/// Work items as issues, in the order of `ids`. Their type (Bug, Task...)
/// and tags stand in for labels.
pub fn parse_work_items(text: &str, ids: &[u64]) -> Result<Vec<Issue>, String> {
    use serde_json::Value;
    #[derive(Deserialize)]
    struct RawItem {
        id: u64,
        #[serde(default)]
        fields: BTreeMap<String, Value>,
    }
    let items = json::<Values<RawItem>>(text)?.value;
    let mut issues: Vec<Issue> = items
        .into_iter()
        .map(|item| {
            let text = |name: &str| item.fields.get(name).and_then(Value::as_str).unwrap_or_default().trim().to_owned();
            // An identity, or on older servers "Name <address>".
            let author = match item.fields.get("System.CreatedBy") {
                Some(Value::Object(identity)) => {
                    identity.get("displayName").and_then(Value::as_str).unwrap_or_default().to_owned()
                }
                Some(Value::String(name)) => name.split(" <").next().unwrap_or_default().to_owned(),
                _ => String::new(),
            };
            let mut labels: Vec<String> = Vec::new();
            labels.extend(Some(text("System.WorkItemType")).filter(|kind| !kind.is_empty()));
            labels.extend(text("System.Tags").split(';').map(str::trim).filter(|tag| !tag.is_empty()).map(str::to_owned));
            Issue {
                number: item.id,
                title: text("System.Title"),
                author: if author.is_empty() { "Unknown".to_owned() } else { author },
                labels,
                comments: item.fields.get("System.CommentCount").and_then(Value::as_u64).unwrap_or(0),
                updated: timestamp(Some(&text("System.ChangedDate"))),
            }
        })
        .collect();
    issues.sort_by_key(|issue| ids.iter().position(|id| *id == issue.number).unwrap_or(usize::MAX));
    Ok(issues)
}

/// The body of a new pull request.
pub fn new_pull_json(new: &NewPull) -> String {
    serde_json::json!({
        "sourceRefName": format!("refs/heads/{}", new.head),
        "targetRefName": format!("refs/heads/{}", new.base),
        "title": new.title.trim(),
        "description": new.body,
        "isDraft": new.draft,
    })
    .to_string()
}

/// `GET {PROFILE_API}/_apis/profile/profiles/me`: the signed-in user.
pub fn profile_url() -> String {
    format!("{PROFILE_API}/_apis/profile/profiles/me?{VERSION}")
}

pub fn parse_profile(text: &str) -> Result<Profile, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Raw {
        display_name: Option<String>,
        email_address: Option<String>,
        public_alias: Option<String>,
    }
    let raw: Raw = json(text)?;
    let present = |text: Option<String>| text.map(|text| tidy(&text)).filter(|text| !text.is_empty());
    let email = present(raw.email_address);
    let login = email.clone().or_else(|| present(raw.public_alias)).ok_or("the answer names no account")?;
    Ok(Profile { id: 0, login, name: present(raw.display_name).unwrap_or_default(), email })
}

// ---- the client ----

#[derive(Clone, Copy)]
enum List {
    Pulls,
    Builds,
    Repositories,
}

pub struct Client {
    api: Api,
    /// The repository's id, which builds are filtered by: not asked for
    /// yet (`None`), or asked and not found (`Some(None)`, all the
    /// project's builds are shown then).
    repo_id: Option<Option<String>>,
}

impl Client {
    pub fn new(repo: RepoId, transport: Box<dyn Transport>, secrets: Box<dyn Secrets>) -> Self {
        // A refused sign-in can also come back as 203, with the sign-in page.
        Client { api: Api::new(repo, transport, secrets, 3, &[401, 203], plain_message), repo_id: None }
    }

    /// `{API}/{org}/{project}/_apis/{path}`.
    fn project_api(&self, path: &str) -> String {
        format!("{}/{}/_apis/{path}", self.api.base, self.api.repo.owner)
    }

    fn pulls_url(&self) -> String {
        self.project_api(&format!("git/repositories/{}/pullrequests", self.api.repo.name))
    }

    /// The repository's id, asked for once.
    fn repository_id(&mut self, now: u64) -> Result<Option<String>, Problem> {
        if let Some(known) = &self.repo_id {
            return Ok(known.clone());
        }
        let url = self.project_api(&format!("git/repositories?{VERSION}"));
        let response = self.api.get(url, Some(List::Repositories as usize), now)?;
        let id = match self.api.classify(response)? {
            Got::Body(response) => repository_id(&response.body, &self.api.repo.name).map_err(|error| self.api.unreadable(error))?,
            // Not asked conditionally before an answer was read.
            Got::Unchanged | Got::Nothing => None,
        };
        self.repo_id = Some(id.clone());
        Ok(id)
    }

    fn work_items(&mut self, now: u64) -> Result<(Vec<Issue>, bool), Problem> {
        let query = serde_json::json!({ "query": OPEN_ITEMS }).to_string();
        let url = self.project_api(&format!("wit/wiql?$top={}&{VERSION}", ITEMS + 1));
        let response = self.api.post(&url, &query, false, now)?;
        // Boards switched off for the project: nothing to show.
        if response.status == 404 && self.api.signed_in() {
            return Ok((Vec::new(), false));
        }
        let Got::Body(response) = self.api.classify(response)? else { return Ok((Vec::new(), false)) };
        let (ids, more) = parse_query(&response.body).map_err(|error| self.api.unreadable(error))?;
        if ids.is_empty() {
            return Ok((Vec::new(), more));
        }
        let list: Vec<String> = ids.iter().map(u64::to_string).collect();
        let url = self.project_api(&format!("wit/workitems?ids={}&fields={FIELDS}&errorPolicy=omit&{VERSION}", list.join(",")));
        let response = self.api.get(url, None, now)?;
        match self.api.classify(response)? {
            Got::Body(response) => {
                let items = parse_work_items(&response.body, &ids).map_err(|error| self.api.unreadable(error))?;
                Ok((items, more))
            }
            Got::Unchanged | Got::Nothing => Ok((Vec::new(), more)),
        }
    }

    fn create(&mut self, new: &NewPull, now: u64) -> Result<PullRequest, Problem> {
        self.api.look_again();
        let url = format!("{}?{VERSION}", self.pulls_url());
        let response = self.api.post(&url, &new_pull_json(new), true, now)?;
        let message = self.api.message(&response.body);
        let lower = message.to_lowercase();
        let (head, base) = (&new.head, &new.base);
        match response.status {
            200 | 201 => parse_pull(&response.body, &self.api.repo).map_err(|error| self.api.unreadable(error)),
            409 => Err(Problem::Refused(format!("A pull request from {head} into {base} is already open"))),
            404 => Err(Problem::NoAccess),
            400 if lower.contains("branch") && (lower.contains("exist") || lower.contains("not found")) => {
                Err(Problem::Refused(format!("Azure DevOps does not have the branch {head} or {base}: push it first")))
            }
            400 | 422 if !message.is_empty() => Err(Problem::Refused(message)),
            403 => Err(Problem::Refused(match message.is_empty() {
                true => format!("No permission to open pull requests in {}", self.api.repo.name),
                false => message,
            })),
            status => Err(Problem::Failed(format!("Azure DevOps answered {status}"))),
        }
    }
}

impl Backend for Client {
    fn pulls(&mut self, now: u64) -> Outcome<PullRequest> {
        self.api.start_round();
        let url = format!("{}?searchCriteria.status=active&$top={}&{VERSION}", self.pulls_url(), PULLS + 1);
        let repo = self.api.repo.clone();
        self.api.fetch(url, List::Pulls as usize, now, |_, _| None, |response| parse_pulls(&response.body, &repo))
    }

    fn issues(&mut self, now: u64) -> Outcome<Issue> {
        match self.work_items(now) {
            Ok((items, more)) => Outcome::Fresh { items, more },
            Err(problem) => Outcome::Failed(self.api.scrub(problem)),
        }
    }

    fn runs(&mut self, now: u64) -> Outcome<Run> {
        let id = match self.repository_id(now) {
            Ok(id) => id,
            Err(problem) => return Outcome::Failed(self.api.scrub(problem)),
        };
        let filter = id.map(|id| format!("repositoryId={id}&repositoryType=TfsGit&")).unwrap_or_default();
        let url = self.project_api(&format!("build/builds?{filter}queryOrder=queueTimeDescending&$top={}&{VERSION}", BUILDS + 1));
        // With a sign-in, "not found" means Pipelines is off for the project.
        let special = |response: &HttpResponse, signed_in: bool| (signed_in && response.status == 404).then_some(Ok(Got::Nothing));
        self.api.fetch(url, List::Builds as usize, now, special, |response| parse_builds(&response.body))
    }

    fn auth(&self) -> Auth {
        self.api.auth()
    }

    fn look_again(&mut self) {
        self.api.look_again();
        if self.repo_id == Some(None) {
            self.repo_id = None;
        }
    }

    fn blocked_until(&self, now: u64) -> Option<u64> {
        self.api.blocked_until(now)
    }

    fn create_pull(&mut self, new: &NewPull, now: u64) -> Result<PullRequest, Problem> {
        let created = self.create(new, now);
        created.map_err(|problem| self.api.scrub(problem))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fake, Log, NOW, OTHER, SECRET, Script, reply};
    use super::*;

    fn repo() -> RepoId {
        parse_remote("https://acme@dev.azure.com/acme/Web/_git/app").unwrap()
    }

    fn scripted(replies: Vec<Result<HttpResponse, String>>, secrets: Fake) -> (Client, Log) {
        let sent = Log::default();
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        (Client::new(repo(), Box::new(transport), Box::new(secrets)), sent)
    }

    fn with_pat() -> Fake {
        Fake { env: vec![("AZURE_DEVOPS_EXT_PAT", SECRET)], ..Default::default() }
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

    // Trimmed-down answers, after the Azure DevOps REST API reference.
    const PULLS_JSON: &str = r#"{"count": 2, "value": [
      {"repository": {"id": "3411ebc1-d5aa-464f-9615-0b527bc66719", "name": "app", "project": {"name": "Web"}},
       "pullRequestId": 12, "codeReviewId": 12, "status": "active",
       "createdBy": {"displayName": "Ada Lovelace", "uniqueName": "ada@acme.example", "id": "u1"},
       "creationDate": "1970-01-02T00:00:00.1234567Z", "title": "Add dark mode", "description": "",
       "sourceRefName": "refs/heads/feature/dark", "targetRefName": "refs/heads/main",
       "mergeStatus": "succeeded", "isDraft": false,
       "lastMergeSourceCommit": {"commitId": "1111111111111111111111111111111111111111"},
       "reviewers": [{"displayName": "Grace", "vote": 10, "isRequired": true}]},
      {"pullRequestId": 9, "title": "Parser", "isDraft": true, "createdBy": null,
       "sourceRefName": "refs/heads/parser", "targetRefName": "refs/heads/main",
       "forkSource": {"repository": {"id": "x", "name": "app fork", "project": {"name": "Grace's"}}}}
    ]}"#;

    const REPOSITORIES: &str = r#"{"count": 2, "value": [
      {"id": "0d1f8c1a-0000-0000-0000-000000000001", "name": "docs", "project": {"name": "Web"}},
      {"id": "3411ebc1-d5aa-464f-9615-0b527bc66719", "name": "App", "project": {"name": "Web"},
       "defaultBranch": "refs/heads/main", "webUrl": "https://dev.azure.com/acme/Web/_git/App"}
    ]}"#;

    const BUILDS_JSON: &str = r#"{"count": 3, "value": [
      {"id": 501, "buildNumber": "19700101.3", "status": "completed", "result": "succeeded",
       "queueTime": "1970-01-01T01:00:00.000Z", "definition": {"id": 3, "name": "app-CI"},
       "sourceBranch": "refs/pull/12/merge", "sourceVersion": "9999999999999999999999999999999999999999",
       "reason": "pullRequest",
       "triggerInfo": {"pr.number": "12", "pr.sourceSha": "1111111111111111111111111111111111111111", "pr.title": "Add dark mode"}},
      {"id": 500, "status": "inProgress", "definition": {"name": "app-CI"}, "sourceBranch": "refs/heads/main",
       "sourceVersion": "4444444444444444444444444444444444444444", "reason": "individualCI",
       "triggerInfo": {"ci.message": "Fix the build"}},
      {"id": 499, "status": "completed", "result": "partiallySucceeded", "definition": {"name": "nightly"},
       "sourceBranch": "refs/tags/v1.2", "reason": "schedule"}
    ]}"#;

    const QUERY: &str = r#"{"queryType": "flat", "queryResultType": "workItem", "asOf": "1970-01-01T00:00:00Z",
      "workItems": [{"id": 31, "url": "https://dev.azure.com/acme/_apis/wit/workItems/31"}, {"id": 30, "url": "x"}]}"#;

    const WORK_ITEMS: &str = r#"{"count": 2, "value": [
      {"id": 30, "rev": 1, "fields": {"System.WorkItemType": "Task", "System.Title": "Docs typo",
       "System.CreatedBy": "Linus <linus@acme.example>"}},
      {"id": 31, "rev": 4, "fields": {"System.WorkItemType": "Bug", "System.Title": "Crash on empty repo",
       "System.CreatedBy": {"displayName": "Grace Hopper", "uniqueName": "grace@acme.example"},
       "System.ChangedDate": "1970-01-01T00:01:00.000Z", "System.CommentCount": 4, "System.Tags": "graph; ui"}}
    ]}"#;

    #[test]
    fn every_form_of_remote_url_names_the_same_repository() {
        let expected = RepoId { service: Service::AzureDevOps, host: HOST.to_owned(), owner: "acme/Web".to_owned(), name: "app".to_owned() };
        for url in [
            "https://dev.azure.com/acme/Web/_git/app",
            "https://acme@dev.azure.com/acme/Web/_git/app",
            "https://acme:pat@dev.azure.com/acme/Web/_git/app/",
            "https://DEV.AZURE.COM/acme/Web/_git/app",
            "git@ssh.dev.azure.com:v3/acme/Web/app",
            "ssh://git@ssh.dev.azure.com/v3/acme/Web/app",
            "ssh://git@ssh.dev.azure.com:22/v3/acme/Web/app",
            "https://acme.visualstudio.com/Web/_git/app",
            "https://acme.visualstudio.com/DefaultCollection/Web/_git/app",
            "https://user@acme.visualstudio.com:443/Web/_git/app",
            "acme@vs-ssh.visualstudio.com:v3/acme/Web/app",
            "ssh://acme@vs-ssh.visualstudio.com:22/v3/acme/Web/app",
            "  https://dev.azure.com/acme/Web/_git/app\n",
        ] {
            assert_eq!(parse_remote(url), Some(expected.clone()), "{url}");
            assert_eq!(super::super::parse_remote(url, &BTreeMap::new()), Some(expected.clone()), "{url}");
        }
        // A repository named after its project.
        for url in ["https://dev.azure.com/acme/_git/Web", "https://acme.visualstudio.com/_git/Web", "https://acme.visualstudio.com/DefaultCollection/_git/Web"] {
            assert_eq!(parse_remote(url).unwrap().slug(), "acme/Web/Web", "{url}");
        }
        // Spaces and other characters stay escaped, the same way whatever
        // the remote wrote.
        let spaced = parse_remote("https://dev.azure.com/acme/My%20Project/_git/My%20Repo").unwrap();
        assert_eq!((spaced.owner.as_str(), spaced.name.as_str()), ("acme/My%20Project", "My%20Repo"));
        assert_eq!(parse_remote("git@ssh.dev.azure.com:v3/acme/My%20Project/My%20Repo"), Some(spaced.clone()));
        assert_eq!(parse_remote("https://dev.azure.com/acme/My%20Project/_git/r%C3%A9po").unwrap().name, "r%C3%A9po");

        for url in [
            "https://dev.azure.com/acme/Web",
            "https://dev.azure.com/acme/Web/_git",
            "https://dev.azure.com/acme/Web/_git/app/pullrequest/12",
            "https://dev.azure.com/acme/Web/_build",
            "https://dev.azure.com/acme/Web/_git/%2E%2E",
            "https://dev.azure.com/acme/Web/_git/a%2Fb",
            "https://dev.azure.com/acme/Web/_git/a%0Ab",
            "https://dev.azure.com/acme/Web/_git/a%zz",
            "https://dev.azure.com/acme/Web/_git/a%",
            "https://dev.azure.com/acme/Web/_git/_private",
            "https://dev.azure.com/ac_me/Web/_git/app",
            "https://dev.azure.com.evil.example/acme/Web/_git/app",
            "https://evil.example/dev.azure.com/acme/Web/_git/app",
            "https://dev.azure.com@evil.example/acme/Web/_git/app",
            "https://evil.visualstudio.com.evil.example/Web/_git/app",
            "https://a.b.visualstudio.com/Web/_git/app",
            "git@ssh.dev.azure.com:v2/acme/Web/app",
            "git@ssh.dev.azure.com:v3/acme/Web",
            "ftp://dev.azure.com/acme/Web/_git/app",
            "file:///dev.azure.com/acme/Web/_git/app",
            "https://dev.azure.com:port/acme/Web/_git/app",
            "https://github.com/acme/app",
        ] {
            assert_eq!(parse_remote(url), None, "{url}");
        }
        // Nothing on these hosts is offered as a server to connect.
        for url in ["https://dev.azure.com/acme/Web", "https://acme.visualstudio.com/Web/_git/app/x"] {
            assert!(offer_for(&RemoteInfo { name: "origin".into(), url: url.into() }).is_none(), "{url}");
        }
    }

    #[test]
    fn links_are_azure_devops_pages_and_stay_safe() {
        let repo = parse_remote("https://dev.azure.com/acme/My%20Project/_git/app").unwrap();
        assert_eq!(repo.pulls_url(), "https://dev.azure.com/acme/My%20Project/_git/app/pullrequests");
        assert_eq!(repo.pull_url(12), "https://dev.azure.com/acme/My%20Project/_git/app/pullrequest/12");
        assert_eq!(repo.issues_url(), "https://dev.azure.com/acme/My%20Project/_workitems");
        assert_eq!(repo.issue_url(31), "https://dev.azure.com/acme/My%20Project/_workitems/edit/31");
        assert_eq!(repo.runs_url(), "https://dev.azure.com/acme/My%20Project/_build");
        assert_eq!(repo.run_url(501), "https://dev.azure.com/acme/My%20Project/_build/results?buildId=501");
        assert_eq!(
            repo.compare_url("main", "feature/dark mode"),
            "https://dev.azure.com/acme/My%20Project/_git/app/pullrequestcreate?sourceRef=feature/dark%20mode&targetRef=main"
        );
        for link in [repo.pulls_url(), repo.pull_url(1), repo.issues_url(), repo.issue_url(1), repo.runs_url(), repo.run_url(1)] {
            assert!(link.starts_with("https://dev.azure.com/"), "{link}");
            assert!(link.bytes().all(|b| b.is_ascii_alphanumeric() || b"/:._-%?=".contains(&b)), "{link}");
        }
        assert!(!repo.self_hosted());
        assert_eq!((repo.site(), repo.api_base()), ("Azure DevOps".to_owned(), API.to_owned()));
        let service = Service::AzureDevOps;
        assert_eq!((service.pull_number(12), service.ci_title(), service.issues_title()), ("!12".to_owned(), "PIPELINES", "WORK ITEMS"));
        assert_eq!(pull_branch(service, 12), "pr/12");
    }

    #[test]
    fn pull_requests_map_to_the_model() {
        let (pulls, more) = parse_pulls(PULLS_JSON, &repo()).unwrap();
        assert!(!more);
        let first = &pulls[0];
        assert_eq!((first.number, first.title.as_str(), first.author.as_str()), (12, "Add dark mode", "Ada Lovelace"));
        assert_eq!((first.head_ref.as_str(), first.base_ref.as_str()), ("feature/dark", "main"));
        assert_eq!((first.head_sha.as_str(), first.updated), ("1111111111111111111111111111111111111111", 86_400));
        assert!(first.is_from(&repo()) && !first.draft);
        let fork = &pulls[1];
        assert!(fork.draft && !fork.is_from(&repo()));
        assert_eq!((fork.author.as_str(), fork.head_repo.as_deref()), ("Unknown", Some("acme/Grace%27s/app%20fork")));
        // One more than shown came back: there are more.
        let many = format!(r#"{{"value": [{}]}}"#, vec![r#"{"pullRequestId": 1}"#; PULLS + 1].join(","));
        let (pulls, more) = parse_pulls(&many, &repo()).unwrap();
        assert!(more && pulls.len() == PULLS);
    }

    #[test]
    fn reviewer_votes_become_the_review_state() {
        let state = |reviewers: &str| {
            let text = format!(r#"{{"value": [{{"pullRequestId": 1, "reviewers": [{reviewers}]}}]}}"#);
            let pull = parse_pulls(&text, &repo()).unwrap().0.remove(0);
            let approvals = pull.approvals.unwrap();
            (pull.review, approvals.given, approvals.left)
        };
        assert_eq!(state(""), (Review::Unreviewed, 0, 0));
        assert_eq!(state(r#"{"vote": 0}"#), (Review::Unreviewed, 0, 0));
        assert_eq!(state(r#"{"vote": 10}, {"vote": 5}"#), (Review::Approved, 2, 0));
        assert_eq!(state(r#"{"vote": 10}, {"vote": 0, "isRequired": true}"#), (Review::Required, 1, 1));
        assert_eq!(state(r#"{"vote": 10}, {"vote": -5}"#), (Review::ChangesRequested, 1, 0));
        assert_eq!(state(r#"{"vote": -10, "isRequired": true}"#), (Review::ChangesRequested, 0, 1));
    }

    #[test]
    fn builds_map_to_runs_with_the_pull_request_head_they_were_for() {
        let (runs, more) = parse_builds(BUILDS_JSON).unwrap();
        assert!(!more);
        let first = &runs[0];
        assert_eq!((first.id, first.workflow.as_str(), first.branch.as_str()), (501, "app-CI", "!12"));
        assert_eq!((first.event.as_str(), first.title.as_str()), ("pull request", "Add dark mode"));
        assert_eq!((first.state, first.outcome.as_str(), first.when), (RunState::Success, "succeeded", 3600));
        assert_eq!(first.head_sha, "1111111111111111111111111111111111111111");
        assert_eq!((runs[1].state, runs[1].event.as_str(), runs[1].title.as_str()), (RunState::Running, "individual CI", "Fix the build"));
        assert_eq!((runs[1].branch.as_str(), runs[1].outcome.as_str()), ("main", "in progress"));
        assert_eq!((runs[2].state, runs[2].outcome.as_str(), runs[2].branch.as_str()), (RunState::Failure, "partially succeeded", "v1.2"));

        let (pulls, _) = parse_pulls(PULLS_JSON, &repo()).unwrap();
        assert_eq!(checks_for(&pulls[0].head_sha, &runs), Some(Checks::Passing));

        for (status, result, expected) in [
            ("completed", Some("succeeded"), RunState::Success),
            ("completed", Some("failed"), RunState::Failure),
            ("completed", Some("canceled"), RunState::Cancelled),
            ("completed", Some("none"), RunState::Skipped),
            ("inProgress", None, RunState::Running),
            ("cancelling", None, RunState::Cancelled),
            ("notStarted", None, RunState::Queued),
            ("postponed", None, RunState::Queued),
        ] {
            assert_eq!(build_state(status, result), expected, "{status}");
        }
        assert_eq!(words("batchedCI"), "batched CI");
        assert_eq!(words("resourceTrigger"), "resource trigger");
    }

    #[test]
    fn work_items_come_in_the_order_the_query_found_them() {
        let (ids, more) = parse_query(QUERY).unwrap();
        assert_eq!((ids.clone(), more), (vec![31, 30], false));
        let items = parse_work_items(WORK_ITEMS, &ids).unwrap();
        assert_eq!(items.iter().map(|item| item.number).collect::<Vec<_>>(), [31, 30]);
        assert_eq!((items[0].title.as_str(), items[0].author.as_str(), items[0].comments), ("Crash on empty repo", "Grace Hopper", 4));
        assert_eq!((items[0].labels.clone(), items[0].updated), (vec!["Bug".to_owned(), "graph".to_owned(), "ui".to_owned()], 60));
        assert_eq!((items[1].author.as_str(), items[1].labels.clone()), ("Linus", vec!["Task".to_owned()]));
    }

    #[test]
    fn the_repository_is_found_by_name_in_its_project() {
        assert_eq!(repository_id(REPOSITORIES, "app").unwrap().as_deref(), Some("3411ebc1-d5aa-464f-9615-0b527bc66719"));
        assert_eq!(repository_id(REPOSITORIES, "other").unwrap(), None);
        let spaced = r#"{"value": [{"id": "0d1f8c1a-0000-0000-0000-000000000002", "name": "My Repo"}]}"#;
        assert!(repository_id(spaced, "My%20Repo").unwrap().is_some());
        // Only a GUID goes into an address.
        assert_eq!(repository_id(r#"{"value": [{"id": "x&y=z", "name": "app"}]}"#, "app").unwrap(), None);
    }

    #[test]
    fn every_list_is_asked_for_below_the_organisation_with_the_token() {
        let (mut client, sent) = scripted(
            vec![
                reply(200, &[], PULLS_JSON),
                reply(200, &[], QUERY),
                reply(200, &[], WORK_ITEMS),
                reply(200, &[], REPOSITORIES),
                reply(200, &[], BUILDS_JSON),
                reply(200, &[], BUILDS_JSON),
            ],
            with_pat(),
        );
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, more: false } if items.len() == 2));
        assert!(matches!(client.issues(NOW), Outcome::Fresh { ref items, more: false } if items.len() == 2));
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, .. } if items.len() == 3));
        // The repository's id is asked for once.
        assert!(matches!(client.runs(NOW), Outcome::Fresh { .. }));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("AZURE_DEVOPS_EXT_PAT")));
        let base = "https://dev.azure.com/acme/Web/_apis";
        let builds = format!(
            "{base}/build/builds?repositoryId=3411ebc1-d5aa-464f-9615-0b527bc66719&repositoryType=TfsGit&queryOrder=queueTimeDescending&$top=21&api-version=7.1"
        );
        assert_eq!(
            urls(&sent),
            [
                format!("{base}/git/repositories/app/pullrequests?searchCriteria.status=active&$top=31&api-version=7.1"),
                format!("{base}/wit/wiql?$top=31&api-version=7.1"),
                format!("{base}/wit/workitems?ids=31,30&fields={FIELDS}&errorPolicy=omit&api-version=7.1"),
                format!("{base}/git/repositories?api-version=7.1"),
                builds.clone(),
                builds,
            ]
        );
        let sent = sent.lock().unwrap();
        assert!(sent.iter().all(|request| within(API, &request.url) && request.token.as_deref() == Some(SECRET)));
        let query: serde_json::Value = serde_json::from_str(sent[1].body.as_deref().unwrap()).unwrap();
        assert!(query["query"].as_str().unwrap().contains("@project"));
    }

    #[test]
    fn statuses_map_to_problems() {
        // Organisations are private: a refusal without a sign-in.
        let (mut client, _) = scripted(vec![reply(401, &[], "")], Fake::default());
        assert_eq!(failure(client.pulls(NOW)), Problem::NotSignedIn);
        let detail = Problem::NotSignedIn.detail(Service::AzureDevOps);
        assert!(detail.contains("AZURE_DEVOPS_EXT_PAT") && detail.contains("az login"), "{detail}");

        // The sign-in page in place of a 401 is a refusal of the token too.
        let (mut client, sent) = scripted(vec![reply(203, &[], "<html>Sign in</html>"), reply(401, &[], "")], with_pat());
        assert_eq!(failure(client.pulls(NOW)), Problem::SignInRejected);
        assert_eq!(sent.lock().unwrap()[1].token, None);

        let missing = r#"{"$id": "1", "message": "TF401019: The Git repository with name or identifier app does not exist or you do not have permissions for the operation you are attempting.", "typeKey": "GitRepositoryNotFoundException"}"#;
        let (mut client, _) = scripted(vec![reply(404, &[], missing), reply(404, &[], "{}")], with_pat());
        assert_eq!(failure(client.pulls(NOW)), Problem::NoAccess);
        // Boards off: no work items, and no error.
        assert_eq!(client.issues(NOW), Outcome::Fresh { items: Vec::new(), more: false });

        // A token the pull requests accepted, refused for builds: a scope.
        let (mut client, _) = scripted(vec![reply(200, &[], PULLS_JSON), reply(401, &[], "")], with_pat());
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert!(matches!(failure(client.runs(NOW)), Problem::Refused(text) if text.contains("scope")));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("AZURE_DEVOPS_EXT_PAT")));

        // Throttled, with Azure's headers.
        let reset = (NOW + 300).to_string();
        let limited = [("retry-after", "30"), ("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", reset.as_str())];
        let (mut client, sent) = scripted(vec![reply(429, &limited, "")], with_pat());
        assert_eq!(failure(client.pulls(NOW)), Problem::RateLimited { until: NOW + 30 });
        assert_eq!(failure(client.runs(NOW + 10)), Problem::RateLimited { until: NOW + 30 });
        assert_eq!(sent.lock().unwrap().len(), 1);

        // A redirect away from the API (to a sign-in page) is not followed.
        let (mut client, sent) = scripted(vec![reply(302, &[("location", "https://login.microsoftonline.com/x")], "")], with_pat());
        assert_eq!(failure(client.pulls(NOW)), Problem::Failed("Azure DevOps sent an unexpected redirect".to_owned()));
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    /// The credential manager as git answers it.
    struct Helper(&'static str, String);

    impl Secrets for Helper {
        fn env(&self, _name: &str) -> Option<String> {
            None
        }
        fn cli(&self, _service: Service, _host: &str) -> Option<String> {
            None
        }
        fn credential_helper(&self, _host: &str) -> Option<String> {
            None
        }
        fn credential(&self, host: &str, path: &str) -> Option<(String, String)> {
            assert_eq!((host, path), ("dev.azure.com", "acme/Web/_git/app"));
            Some((self.0.to_owned(), self.1.clone()))
        }
    }

    #[test]
    fn tokens_are_presented_as_each_kind_wants() {
        let repo = repo();
        // A personal access token: Basic, no user name.
        let (token, source) = find_token(&with_pat(), &repo, &[]).unwrap();
        assert_eq!(source, TokenSource::Env("AZURE_DEVOPS_EXT_PAT"));
        assert_eq!(token.authorization(), format!("Basic {}", base64_padded(format!(":{SECRET}").as_bytes())));
        // The Azure CLI's: a bearer token, however long.
        let long = format!("eyJ0eXAiOiJKV1Qi.{}.sig", "a".repeat(3000));
        let leaked: &'static str = Box::leak(long.clone().into_boxed_str());
        let cli = Fake { cli: Some(leaked), ..Default::default() };
        let (token, source) = find_token(&cli, &repo, &[]).unwrap();
        assert_eq!((source, token.authorization()), (TokenSource::Cli, format!("Bearer {long}")));
        assert_eq!(cli.asked(), ["AZURE_DEVOPS_EXT_PAT", "az dev.azure.com"]);
        // The credential manager is asked about the repository; an OAuth
        // sign-in is a bearer token, a PAT goes with its user name.
        let (token, _) = find_token(&Helper("ada", long.clone()), &repo, &[]).unwrap();
        assert_eq!(token.authorization(), format!("Bearer {long}"));
        let (token, source) = find_token(&Helper("ada", OTHER.to_owned()), &repo, &[]).unwrap();
        assert_eq!((source, token.authorization()), (TokenSource::CredentialHelper, format!("Basic {}", base64_padded(format!("ada:{OTHER}").as_bytes()))));
        assert_eq!(find_token(&Helper("ada", OTHER.to_owned()), &repo, &[token]), None);

        // The CLI command: one fixed request, telemetry off.
        let cmd = cli_command();
        let args: Vec<String> = cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["account", "get-access-token", "--resource", RESOURCE, "--query", "accessToken", "--output", "tsv"]);
        assert!(cmd.get_envs().any(|(name, value)| name == "AZURE_CORE_COLLECT_TELEMETRY" && value == Some(std::ffi::OsStr::new("false"))));
        assert_eq!(credential_query_at(HOST, &credential_path(&repo)), "protocol=https\nhost=dev.azure.com\npath=acme/Web/_git/app\n\n");
        assert_eq!(credential_path(&RepoId { owner: String::new(), name: String::new(), ..repo }), "");
    }

    #[test]
    fn a_pull_request_is_opened_with_one_post_and_checked_out_from_its_branch() {
        let created = r#"{"pullRequestId": 13, "title": "Speed up", "isDraft": true, "createdBy": {"displayName": "Ada"},
            "sourceRefName": "refs/heads/feature/fast", "targetRefName": "refs/heads/main",
            "lastMergeSourceCommit": {"commitId": "5555555555555555555555555555555555555555"}}"#;
        let (mut client, sent) = scripted(vec![reply(201, &[], created)], with_pat());
        let new = NewPull { head: "feature/fast".into(), base: "main".into(), title: " Speed up ".into(), body: "Body".into(), draft: true };
        let pull = client.create_pull(&new, NOW).unwrap();
        assert!(pull.draft && pull.number == 13 && pull.is_from(&repo()));
        {
            let sent = sent.lock().unwrap();
            assert_eq!(sent[0].url, "https://dev.azure.com/acme/Web/_apis/git/repositories/app/pullrequests?api-version=7.1");
            let body: serde_json::Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
            assert_eq!(
                body,
                serde_json::json!({"sourceRefName": "refs/heads/feature/fast", "targetRefName": "refs/heads/main",
                    "title": "Speed up", "description": "Body", "isDraft": true})
            );
        }
        let exists = r#"{"message": "TF401179: An active pull request for the source and target branch already exists.", "typeKey": "GitPullRequestExistsException"}"#;
        let gone = r#"{"message": "TF401398: The pull request cannot be activated because the source and/or the target branch no longer exists, or the requested refs are not branches"}"#;
        for (status, body, expected) in [
            (409, exists, Problem::Refused("A pull request from feature/fast into main is already open".to_owned())),
            (400, gone, Problem::Refused("Azure DevOps does not have the branch feature/fast or main: push it first".to_owned())),
            (404, "{}", Problem::NoAccess),
        ] {
            let (mut client, _) = scripted(vec![reply(status, &[], body)], with_pat());
            assert_eq!(client.create_pull(&new, NOW), Err(expected), "{status}");
        }
        let (mut client, sent) = scripted(vec![], Fake::default());
        assert_eq!(client.create_pull(&new, NOW), Err(Problem::NotSignedIn));
        assert!(sent.lock().unwrap().is_empty());

        // No pull request ref: the branch is fetched.
        let cmd = checkout_pull_request("origin", Service::AzureDevOps, &repo(), &pull, false).unwrap();
        assert_eq!(cmd.label, "Check out pull request !13");
    }

    #[test]
    fn the_profile_is_read_from_its_own_service() {
        assert!(profile_url().starts_with("https://app.vssps.visualstudio.com/_apis/profile/profiles/me?"));
        let profile = parse_profile(
            r#"{"displayName": "Ada Lovelace", "publicAlias": "d6245f20-2af8-44f4-9451-8107cb2767db", "emailAddress": "ada@acme.example", "id": "d6245f20"}"#,
        )
        .unwrap();
        assert_eq!((profile.login.as_str(), profile.display_name(), profile.email.as_deref()), ("ada@acme.example", "Ada Lovelace", Some("ada@acme.example")));
        assert_eq!(parse_profile(r#"{"publicAlias": "d6245f20"}"#).unwrap().login, "d6245f20");
        assert!(parse_profile("{}").is_err());
    }

    #[test]
    fn the_offline_fixture_serves_azure_endpoints() {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-canned-azure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in [("pullrequests", PULLS_JSON), ("wiql", QUERY), ("workitems", WORK_ITEMS), ("repositories", REPOSITORIES), ("builds", BUILDS_JSON)] {
            std::fs::write(dir.join(format!("{name}.json")), body).unwrap();
        }
        let mut client = Client::new(repo(), Box::new(Canned { dir: dir.clone(), created: false }), Box::new(NoSecrets));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
        assert!(matches!(client.issues(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, .. } if items.len() == 3));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
