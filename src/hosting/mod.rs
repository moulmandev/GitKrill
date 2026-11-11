//! Hosting-service integration: pull requests (merge requests on GitLab),
//! issues and CI runs for the repository a remote points at.
//!
//! * The model ([`PullRequest`], [`Issue`], [`Run`], [`Feed`], [`Problem`]),
//!   the rate-limit bookkeeping ([`Limiter`]), finding a token
//!   ([`find_token`]), the network ([`Wire`]) and the per-tab
//!   [`HostingState`] are shared by every service.
//! * [`github`] (github.com and GitHub Enterprise), [`gitlab`] (gitlab.com
//!   and self-managed GitLab), [`bitbucket`] (bitbucket.org), [`gitea`]
//!   (Gitea, Forgejo and codeberg.org) and [`azure`] (Azure DevOps) hold the
//!   REST endpoints and their JSON, each behind the [`Backend`] trait. The
//!   last three share their sign-in and request handling (`signin`).
//!
//! Which service a remote is on comes from its host. github.com,
//! gitlab.com, bitbucket.org, codeberg.org and Azure DevOps are known; any
//! other host is contacted only once the user has said what runs there
//! ([`Servers`], kept in the settings). Until then the sidebar merely offers
//! to connect it ([`offers`]).
//!
//! All network and process work happens on one background thread per tab
//! (`worker`); the UI thread only sends it orders and drains its events.
//! Nothing in this module logs or writes to disk. A token is only ever
//! attached to a request by [`Wire`], and only to one below the API address
//! of the host the token was looked up for.
//!
//! Development aid: with `GITKRILL_HOSTING_FIXTURE=<dir>` set, answers come
//! from files in `<dir>` named after the endpoint (`pulls.json`,
//! `issues.json` and `runs.json` for GitHub; `merge_requests.json`,
//! `issues.json` and `pipelines.json` for GitLab; the other services' after
//! their own endpoints) instead of the network
//! (see `Canned`) and no token is looked for, so screenshots of every state
//! can be taken offline. Opening a pull request is answered from
//! `create.json`; an empty file named `signed-in` stands in for a sign-in.
//! The accounts in Preferences > Integrations ([`account`]) are answered
//! from `user.json` and `keys.json` the same way.
//!
//! An account the user disconnected there ([`Disconnected`]) is not
//! signed in to: its repositories are asked about without a token.
//!
//! After the three lists, each round asks the backend for [`Details`]:
//! review states, the forks pull requests come from and the repository's
//! parent, only for what changed since it last asked. A self-managed
//! server may be installed under a path (`example.com/gitlab`); that is
//! then its name in [`Servers`] and [`RepoId::host`].

use std::collections::{BTreeMap, BTreeSet};
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
use serde::{Deserialize, Serialize};

use crate::git::RemoteInfo;
use crate::git::ops::{GitCmd, UndoKind};

// ---- what is hosted where -------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    /// github.com.
    GitHub,
    /// GitHub Enterprise Server, or Enterprise Cloud on `*.ghe.com`, on a
    /// host the user named.
    GitHubEnterprise,
    /// gitlab.com, or a self-managed GitLab on a host the user named.
    GitLab,
    /// bitbucket.org (Bitbucket Cloud).
    Bitbucket,
    /// Gitea on a host the user named.
    Gitea,
    /// Forgejo on a host the user named.
    Forgejo,
    /// codeberg.org, which runs Forgejo.
    Codeberg,
    /// Azure DevOps Services: dev.azure.com, and the older
    /// `<organisation>.visualstudio.com` addresses.
    AzureDevOps,
}

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Service::GitHub => "GitHub",
            Service::GitHubEnterprise => "GitHub Enterprise",
            Service::GitLab => "GitLab",
            Service::Bitbucket => "Bitbucket",
            Service::Gitea => "Gitea",
            Service::Forgejo => "Forgejo",
            Service::Codeberg => "Codeberg",
            Service::AzureDevOps => "Azure DevOps",
        }
    }

    /// GitLab has its own words: merge requests, `!12`, pipelines, projects.
    pub fn is_gitlab(self) -> bool {
        matches!(self, Service::GitLab)
    }

    /// Speaks Gitea's API: Gitea, Forgejo (its fork) and Codeberg.
    pub fn is_gitea(self) -> bool {
        matches!(self, Service::Gitea | Service::Forgejo | Service::Codeberg)
    }

    /// The service's own command line tool, which GitKrill asks for the
    /// sign-in it keeps: `gh`, `glab`, `tea` (Gitea's, for Forgejo too) or
    /// `az`. Bitbucket has none.
    pub fn cli_tool(self) -> Option<&'static str> {
        match self {
            Service::GitHub | Service::GitHubEnterprise => Some("gh"),
            Service::GitLab => Some("glab"),
            Service::Gitea | Service::Forgejo | Service::Codeberg => Some("tea"),
            Service::AzureDevOps => Some("az"),
            Service::Bitbucket => None,
        }
    }

    /// Its CI runs are called pipelines (GitLab, Bitbucket, Azure DevOps)
    /// rather than workflow runs and checks.
    pub fn has_pipelines(self) -> bool {
        matches!(self, Service::GitLab | Service::Bitbucket | Service::AzureDevOps)
    }

    /// Whether the remote keeps a ref following each pull request's head
    /// ([`Service::pull_ref`]). Bitbucket Cloud has none, and Azure DevOps
    /// only the merge result: there a pull request is checked out from its
    /// branch ([`checkout_pull_request`]).
    pub fn has_pull_refs(self) -> bool {
        !matches!(self, Service::Bitbucket | Service::AzureDevOps)
    }

    /// Sidebar title of the issue section: Azure DevOps has work items.
    pub fn issues_title(self) -> &'static str {
        if self == Service::AzureDevOps { "WORK ITEMS" } else { "ISSUES" }
    }

    /// "pull request", or "merge request" on GitLab.
    pub fn pull_word(self) -> &'static str {
        if self.is_gitlab() { "merge request" } else { "pull request" }
    }

    /// [`Service::pull_word`] at the start of a sentence.
    pub fn pull_word_title(self) -> &'static str {
        if self.is_gitlab() { "Merge request" } else { "Pull request" }
    }

    /// Sidebar title of the pull / merge request section.
    pub fn pulls_title(self) -> &'static str {
        if self.is_gitlab() { "MERGE REQUESTS" } else { "PULL REQUESTS" }
    }

    /// Sidebar title of the CI section.
    pub fn ci_title(self) -> &'static str {
        if self.has_pipelines() {
            "PIPELINES"
        } else if self.is_gitea() {
            "ACTIONS"
        } else {
            "GITHUB ACTIONS"
        }
    }

    /// A pull request's number as the service writes it: `#12`, or `!12`
    /// for a GitLab merge request and an Azure DevOps pull request.
    pub fn pull_number(self, number: u64) -> String {
        if self.is_gitlab() || self == Service::AzureDevOps { format!("!{number}") } else { format!("#{number}") }
    }

    /// "repository", or "project" on GitLab.
    pub fn repo_word(self) -> &'static str {
        if self.is_gitlab() { "project" } else { "repository" }
    }

    /// "repositories", or "projects" on GitLab.
    pub fn repos_word(self) -> &'static str {
        if self.is_gitlab() { "projects" } else { "repositories" }
    }

    /// The ref on the remote that follows a pull request's head commit.
    /// Gitea and Forgejo keep GitHub's. Bitbucket and Azure DevOps keep
    /// none (see [`Service::has_pull_refs`]); GitHub's name stands in, and
    /// fetching it fails cleanly.
    fn pull_ref(self, number: u64) -> String {
        match self {
            Service::GitLab => format!("merge-requests/{number}/head"),
            _ => format!("pull/{number}/head"),
        }
    }
}

/// What the user said a self-hosted server is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "&'static str")]
pub enum ServerKind {
    GitHubEnterprise,
    GitLab,
    Gitea,
    Forgejo,
    /// Neither: never contacted, and not offered again. An entry this
    /// version does not understand reads as this too.
    Ignored,
}

impl ServerKind {
    pub fn service(self) -> Option<Service> {
        match self {
            ServerKind::GitHubEnterprise => Some(Service::GitHubEnterprise),
            ServerKind::GitLab => Some(Service::GitLab),
            ServerKind::Gitea => Some(Service::Gitea),
            ServerKind::Forgejo => Some(Service::Forgejo),
            ServerKind::Ignored => None,
        }
    }

    /// The kind of server a self-hosted service runs on.
    pub fn of(service: Service) -> Option<ServerKind> {
        match service {
            Service::GitHubEnterprise => Some(ServerKind::GitHubEnterprise),
            Service::GitLab => Some(ServerKind::GitLab),
            Service::Gitea => Some(ServerKind::Gitea),
            Service::Forgejo => Some(ServerKind::Forgejo),
            _ => None,
        }
    }
}

impl From<String> for ServerKind {
    fn from(text: String) -> Self {
        match text.as_str() {
            "github-enterprise" => ServerKind::GitHubEnterprise,
            "gitlab" => ServerKind::GitLab,
            "gitea" => ServerKind::Gitea,
            "forgejo" => ServerKind::Forgejo,
            _ => ServerKind::Ignored,
        }
    }
}

impl From<ServerKind> for &'static str {
    fn from(kind: ServerKind) -> Self {
        match kind {
            ServerKind::GitHubEnterprise => "github-enterprise",
            ServerKind::GitLab => "gitlab",
            ServerKind::Gitea => "gitea",
            ServerKind::Forgejo => "forgejo",
            ServerKind::Ignored => "ignored",
        }
    }
}

/// Self-hosted servers by host (written as [`RepoId::host`] is) and what
/// the user said each one is. Kept in the settings; a host that is not in
/// here, and is not github.com or gitlab.com, is never sent anything.
pub type Servers = BTreeMap<String, ServerKind>;

/// Hosts (written as [`RepoId::host`] is) whose sign-in GitKrill does not
/// use, because the user disconnected the account in Preferences >
/// Integrations. Kept in the settings. The sign-in itself is left alone:
/// git and other tools keep using it.
pub type Disconnected = BTreeSet<String>;

/// A repository on a hosting service, as named by a remote URL.
///
/// `host` only ever contains lower-case letters, digits, `.` and `-`, and a
/// `:port`, then for a server installed under a path (`example.com/gitlab`)
/// that path, made like `owner`; `owner` and `name` only letters, digits,
/// `.`, `_` and `-`, with `/` between the groups of a GitLab path (and
/// between an Azure DevOps organisation and project, whose names keep
/// anything else `%`-escaped). The parsers reject everything else, so the
/// links built here are safe to hand to the operating system as they are,
/// and [`server_name`] of `host` is safe in a credential-helper query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoId {
    pub service: Service,
    /// `github.com`, `gitlab.com`, or the self-hosted server's name, with
    /// `:port` when an HTTPS remote names a port other than 443, and the
    /// path the server is installed under when it is not at the root
    /// (`example.com/gitlab`): the key of [`Servers`].
    pub host: String,
    /// The account or organisation; on GitLab the whole group path, which
    /// may have subgroups (`group/subgroup`); on Bitbucket the workspace; on
    /// Azure DevOps `organisation/project`.
    pub owner: String,
    pub name: String,
}

impl RepoId {
    /// `owner/name`: the full path on GitLab.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// On a server the user named, rather than a public service
    /// (github.com, gitlab.com, bitbucket.org, codeberg.org, dev.azure.com).
    pub fn self_hosted(&self) -> bool {
        public(&self.host).is_none()
    }

    /// What to call where the repository is: "GitHub", "GitLab", or the
    /// server's name when it is self-hosted.
    pub fn site(&self) -> String {
        if self.self_hosted() { self.host.clone() } else { self.service.name().to_owned() }
    }

    /// Where the REST API lives. A token is only ever sent below this.
    pub fn api_base(&self) -> String {
        api_base(self.service, &self.host)
    }

    /// A page of the repository's web site. GitLab keeps them under `/-/`,
    /// Azure DevOps under `/_git/`.
    fn page(&self, path: &str) -> String {
        if self.service == Service::AzureDevOps {
            return azure::repo_page(self, path);
        }
        let separator = if self.service.is_gitlab() { "/-/" } else { "/" };
        format!("https://{}/{}{separator}{path}", self.host, self.slug())
    }

    pub fn pulls_url(&self) -> String {
        match self.service {
            Service::Bitbucket => self.page("pull-requests"),
            Service::AzureDevOps => self.page("pullrequests"),
            _ => self.page(if self.service.is_gitlab() { "merge_requests" } else { "pulls" }),
        }
    }

    pub fn pull_url(&self, number: u64) -> String {
        match self.service {
            Service::Bitbucket => self.page(&format!("pull-requests/{number}")),
            Service::AzureDevOps => self.page(&format!("pullrequest/{number}")),
            service if service.is_gitea() => self.page(&format!("pulls/{number}")),
            service if service.is_gitlab() => self.page(&format!("merge_requests/{number}")),
            _ => self.page(&format!("pull/{number}")),
        }
    }

    /// Azure DevOps has work items, kept by the project.
    pub fn issues_url(&self) -> String {
        match self.service {
            Service::AzureDevOps => azure::project_page(self, "_workitems"),
            _ => self.page("issues"),
        }
    }

    pub fn issue_url(&self, number: u64) -> String {
        match self.service {
            Service::AzureDevOps => azure::project_page(self, &format!("_workitems/edit/{number}")),
            _ => self.page(&format!("issues/{number}")),
        }
    }

    pub fn runs_url(&self) -> String {
        match self.service {
            Service::AzureDevOps => azure::project_page(self, "_build"),
            service if service.has_pipelines() => self.page("pipelines"),
            _ => self.page("actions"),
        }
    }

    /// Bitbucket numbers a pipeline's page by its build number, Gitea and
    /// Forgejo a run's by its number in the repository: what their
    /// backends put in [`Run::id`].
    pub fn run_url(&self, id: u64) -> String {
        match self.service {
            Service::Bitbucket => self.page(&format!("pipelines/results/{id}")),
            Service::AzureDevOps => azure::project_page(self, &format!("_build/results?buildId={id}")),
            service if service.is_gitlab() => self.page(&format!("pipelines/{id}")),
            _ => self.page(&format!("actions/runs/{id}")),
        }
    }

    /// The service's own page for opening a pull request from `head` into
    /// `base`, prefilled. Works without any sign-in in GitKrill.
    ///
    /// Branch names can hold almost anything; every byte outside letters,
    /// digits, `.`, `_`, `-` and `/` is percent-encoded, so the link stays
    /// within the same safe characters as the others plus `%`, `?` and `=`.
    pub fn compare_url(&self, base: &str, head: &str) -> String {
        match self.service {
            Service::GitHub | Service::GitHubEnterprise => {
                self.page(&format!("compare/{}...{}?expand=1", url_escape(base), url_escape(head)))
            }
            // `merge_request[source_branch]`, with its brackets encoded.
            Service::GitLab => self.page(&format!(
                "merge_requests/new?merge_request%5Bsource_branch%5D={}&merge_request%5Btarget_branch%5D={}",
                url_escape(head),
                url_escape(base)
            )),
            Service::Gitea | Service::Forgejo | Service::Codeberg => {
                self.page(&format!("compare/{}...{}", url_escape(base), url_escape(head)))
            }
            Service::Bitbucket => {
                self.page(&format!("pull-requests/new?source={}&dest={}", url_escape(head), url_escape(base)))
            }
            Service::AzureDevOps => self.page(&format!(
                "pullrequestcreate?sourceRef={}&targetRef={}",
                url_escape(head),
                url_escape(base)
            )),
        }
    }

    /// [`RepoId::compare_url`] for a pull request into this repository from
    /// `head` in `from`, which may be a fork of it (or its parent). GitHub
    /// compares across forks as `base...owner:head` on the base repository;
    /// GitLab starts a merge request on the project the branch is in, and
    /// its form offers the project it was forked from as the target.
    pub fn compare_url_from(&self, base: &str, from: &RepoId, head: &str) -> String {
        if from == self {
            return self.compare_url(base, head);
        }
        match self.service {
            Service::GitHub | Service::GitHubEnterprise => self.page(&format!(
                "compare/{}...{}:{}?expand=1",
                url_escape(base),
                url_escape(&from.owner),
                url_escape(head)
            )),
            // Gitea and Forgejo compare across forks the same way.
            Service::Gitea | Service::Forgejo | Service::Codeberg => self.page(&format!(
                "compare/{}...{}:{}",
                url_escape(base),
                url_escape(&from.owner),
                url_escape(head)
            )),
            // These start a pull request from the repository the branch is
            // in, whose form offers the one it was forked from.
            Service::GitLab | Service::Bitbucket | Service::AzureDevOps => from.compare_url(base, head),
        }
    }

    /// Another repository on the same service and server, from the
    /// `owner/name` (the whole path on GitLab) a service answered with.
    /// `None` unless the path is one a remote URL could have named, so
    /// links built from it are as safe as this repository's.
    pub fn sibling(&self, path: &str) -> Option<RepoId> {
        let parts: Vec<&str> = path.split('/').collect();
        let fits = match self.service {
            Service::GitHub | Service::GitHubEnterprise => parts.len() == 2,
            Service::GitLab => (2..=32).contains(&parts.len()) && parts.iter().all(|part| !part.starts_with('-')),
            Service::Bitbucket | Service::Gitea | Service::Forgejo | Service::Codeberg => parts.len() == 2,
            // An organisation and project whose names may need escapes:
            // never taken from an answer.
            Service::AzureDevOps => false,
        };
        if !fits || !parts.iter().all(|part| path_part(part)) {
            return None;
        }
        let (name, owner) = parts.split_last()?;
        Some(RepoId { service: self.service, host: self.host.clone(), owner: owner.join("/"), name: (*name).to_owned() })
    }

    /// The same repository, whatever the capitalisation of its path.
    pub fn same(&self, other: &RepoId) -> bool {
        self.service == other.service && self.host == other.host && self.slug().eq_ignore_ascii_case(&other.slug())
    }
}

/// One part of a repository path: letters, digits, `-`, `_` and `.`, and
/// not `.` or `..` alone.
fn path_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The server's own name in a [`RepoId::host`], with its port but without
/// the path it may be installed under: `example.com` of
/// `example.com/gitlab`. What git's credential helper and the services'
/// command line tools know the server by.
pub fn server_name(host: &str) -> &str {
    host.split('/').next().unwrap_or(host)
}

/// Percent-encodes everything but `A-Z a-z 0-9 . _ - /`.
pub fn url_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"._-/".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Where `service`'s REST API lives for a repository on `host`.
pub fn api_base(service: Service, host: &str) -> String {
    match service {
        Service::GitHub => github::API.to_owned(),
        // Enterprise Cloud with data residency has an API host of its own.
        Service::GitHubEnterprise if host.ends_with(".ghe.com") => format!("https://api.{host}"),
        Service::GitHubEnterprise => format!("https://{host}/api/v3"),
        Service::GitLab => format!("https://{host}/api/v4"),
        Service::Bitbucket => bitbucket::API.to_owned(),
        Service::Gitea | Service::Forgejo | Service::Codeberg => format!("https://{host}/api/v1"),
        // Every organisation's API is below its name on dev.azure.com.
        Service::AzureDevOps => azure::API.to_owned(),
    }
}

/// A remote URL taken apart, when it names a repository on a network host.
#[derive(Debug, PartialEq, Eq)]
struct Address {
    /// Lower-case host name, without a port.
    name: String,
    /// The port of an `https://` URL, unless it is 443. Other schemes'
    /// ports (SSH's) say nothing about where the web server listens.
    port: Option<u16>,
    /// The path's parts with `.git` taken off the last: at least two, each
    /// made only of letters, digits, `.`, `_` and `-`.
    path: Vec<String>,
    /// An `http(s)://` URL: its path starts with the one the server is
    /// installed under, if any. SSH paths never have it.
    web: bool,
}

impl Address {
    /// The host as [`RepoId::host`] and the settings write it.
    fn host(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.name),
            None => self.name.clone(),
        }
    }
}

/// Understands `https://host/a/b(.git)`, `git@host:a/b.git`,
/// `ssh://git@host:22/a/b` and their variations (user names, ports,
/// trailing slashes). Local paths, `file://` and other schemes, and paths
/// with characters a hosted repository's cannot have are `None`.
fn address(url: &str) -> Option<Address> {
    let url = url.trim();
    let mut web = false;
    let (https, host, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = scheme.to_ascii_lowercase();
            let known = ["https", "http", "ssh", "git", "git+ssh", "ssh+git"];
            if !known.contains(&scheme.as_str()) {
                return None;
            }
            web = scheme.starts_with("http");
            let (authority, path) = rest.split_once('/')?;
            (scheme == "https", authority.rsplit_once('@').map_or(authority, |(_, host)| host), path)
        }
        // scp-like: [user@]host:path. A slash before the colon means a
        // local path, and a one-letter "host" is a drive (`C:/x`), which
        // `valid_host` turns down.
        None => {
            let (left, path) = url.split_once(':')?;
            if left.contains('/') || left.contains('\\') {
                return None;
            }
            (false, left.rsplit_once('@').map_or(left, |(_, host)| host), path)
        }
    };
    let (name, port) = match host.split_once(':') {
        Some((name, "")) => (name, None),
        Some((name, port)) => (name, Some(port.parse::<u16>().ok()?)),
        None => (host, None),
    };
    let name = name.to_ascii_lowercase();
    if !valid_host(&name) {
        return None;
    }

    let mut path: Vec<String> = path.trim_matches('/').split('/').map(str::to_owned).collect();
    if let Some(last) = path.last_mut()
        && let Some(stripped) = last.strip_suffix(".git")
    {
        *last = stripped.to_owned();
    }
    let valid = |part: &String| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if !(2..=32).contains(&path.len()) || !path.iter().all(valid) {
        return None;
    }
    let port = port.filter(|port| https && *port != 443);
    Some(Address { name, port, path, web })
}

/// A plausible DNS name, already lower-case.
fn valid_host(name: &str) -> bool {
    (2..=253).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
        && !name.starts_with(['.', '-'])
        && !name.ends_with(['.', '-'])
        && !name.contains("..")
}

/// The public services (github.com, gitlab.com, bitbucket.org,
/// codeberg.org, Azure DevOps), under every name their remotes use.
fn public(name: &str) -> Option<(Service, &'static str)> {
    match name {
        "github.com" | "www.github.com" | "ssh.github.com" => Some((Service::GitHub, "github.com")),
        "gitlab.com" | "www.gitlab.com" | "altssh.gitlab.com" => Some((Service::GitLab, "gitlab.com")),
        "bitbucket.org" | "www.bitbucket.org" | "altssh.bitbucket.org" => Some((Service::Bitbucket, "bitbucket.org")),
        "codeberg.org" | "www.codeberg.org" => Some((Service::Codeberg, "codeberg.org")),
        // Its remotes are read by `azure::parse_remote`; this is the host
        // its repositories are known by.
        name if azure::is_host(name) => Some((Service::AzureDevOps, azure::HOST)),
        _ => None,
    }
}

/// Well-known hosts GitKrill cannot show, and subdomains of the public
/// services that are not servers of their own (gists): not worth offering
/// to connect.
fn unsupported(name: &str) -> bool {
    let others = ["git.sr.ht", "gitee.com", "huggingface.co", "source.developers.google.com"];
    others.contains(&name)
        || [".github.com", ".gitlab.com", ".bitbucket.org", ".codeberg.org"]
            .iter()
            .any(|end| name.ends_with(end))
}

/// What a server's name suggests it runs. Never more than a pre-selection.
fn guess(name: &str) -> Option<ServerKind> {
    let first = name.split('.').next().unwrap_or_default();
    if first.starts_with("gitlab") || name.contains(".gitlab.") {
        Some(ServerKind::GitLab)
    } else if first.starts_with("github") || first == "ghe" || name.ends_with(".ghe.com") {
        Some(ServerKind::GitHubEnterprise)
    } else if first.starts_with("gitea") || name.contains(".gitea.") {
        Some(ServerKind::Gitea)
    } else if first.starts_with("forgejo") || name.contains(".forgejo.") {
        Some(ServerKind::Forgejo)
    } else {
        None
    }
}

/// The hosted repository a remote URL points at: on a public service
/// (github.com, gitlab.com, bitbucket.org, codeberg.org, Azure DevOps), or
/// on a server listed in `servers` as GitHub Enterprise, GitLab, Gitea or
/// Forgejo. Anything else, including hosts whose name merely looks like one
/// of those, is `None`.
pub fn parse_remote(url: &str, servers: &Servers) -> Option<RepoId> {
    // Azure DevOps paths have a shape (and escapes) of their own.
    if let Some(repo) = azure::parse_remote(url) {
        return Some(repo);
    }
    let address = address(url)?;
    let (service, host, skip) = match public(&address.name) {
        Some((service, host)) => (service, host.to_owned(), 0),
        None => server_for(servers, &address)?,
    };
    // Past the path the server is installed under, if any.
    let path = &address.path[skip..];
    let fits = match service {
        Service::GitHub | Service::GitHubEnterprise | Service::Bitbucket => path.len() == 2,
        Service::Gitea | Service::Forgejo | Service::Codeberg => path.len() == 2,
        // GitLab paths never start with `-`, which also keeps the `/-/` of
        // its web addresses from passing for a group.
        Service::GitLab => path.len() >= 2 && path.iter().all(|part| !part.starts_with('-')),
        // Taken apart by `azure::parse_remote` above when it is one.
        Service::AzureDevOps => false,
    };
    if !fits {
        return None;
    }
    let (name, owner) = path.split_last()?;
    Some(RepoId { service, host, owner: owner.join("/"), name: name.clone() })
}

/// The server in `servers` that a remote on a host other than github.com
/// and gitlab.com is on: the service it runs, the server as `servers` names
/// it, and how many parts of the address's path are the server's own.
///
/// That is the server named by the host alone, or one installed under a
/// path (`example.com/gitlab`, a GitLab with a relative URL root). An HTTPS
/// address has that path in front of the repository's; an SSH address
/// never does, so on a host with both kinds of entry it goes to the one
/// under a path (one host's SSH cannot lead to two servers).
fn server_for(servers: &Servers, address: &Address) -> Option<(Service, String, usize)> {
    let host = address.host();
    let under = servers.iter().filter_map(|(key, kind)| {
        let (name, root) = key.split_once('/')?;
        let root: Vec<&str> = root.split('/').collect();
        if name != host || !root.iter().all(|part| path_part(part)) {
            return None;
        }
        let service = kind.service()?;
        if !address.web {
            return Some((key, service, 0));
        }
        // The server's path first, then at least an owner and a name.
        let leads = address.path.len() >= root.len() + 2 && address.path.iter().zip(&root).all(|(a, b)| a == b);
        leads.then_some((key, service, root.len()))
    });
    // The longest path that fits, should two servers share a host.
    if let Some((key, service, skip)) = under.max_by_key(|(_, _, skip)| *skip) {
        return Some((service, key.clone(), skip));
    }
    Some((servers.get(&host)?.service()?, host, 0))
}

/// Picks the remote to show: `origin` when it is hosted, else the first
/// remote that is. Returns the remote's name with the repository.
pub fn detect(remotes: &[RemoteInfo], servers: &Servers) -> Option<(String, RepoId)> {
    let hosted = |remote: &RemoteInfo| Some((remote.name.clone(), parse_remote(&remote.url, servers)?));
    remotes
        .iter()
        .find(|remote| remote.name == "origin")
        .and_then(hosted)
        .or_else(|| remotes.iter().find_map(hosted))
}

/// The remotes pointing at other repositories of `repo`'s service and
/// server than `repo` itself, in the order of `remotes`: a fork's parent
/// (`upstream`, by convention) or forks of `repo` the user pushes to. What a
/// pull request between forks can go from or into.
pub fn related(remotes: &[RemoteInfo], servers: &Servers, repo: &RepoId) -> Vec<(String, RepoId)> {
    let mut found: Vec<(String, RepoId)> = Vec::new();
    for remote in remotes {
        let Some(other) = parse_remote(&remote.url, servers) else { continue };
        let elsewhere = other.service == repo.service && other.host == repo.host && !other.same(repo);
        if elsewhere && !found.iter().any(|(name, _)| *name == remote.name) {
            found.push((remote.name.clone(), other));
        }
    }
    found
}

/// A remote on a server that may run GitHub Enterprise, GitLab, Gitea or
/// Forgejo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The server, as the settings name it.
    pub host: String,
    /// The repository's path there.
    pub path: String,
    /// The remote it was seen on.
    pub remote: String,
    /// What the server's name suggests: a pre-selection, nothing more.
    pub guess: Option<ServerKind>,
}

/// The self-hosted server `remote` points at, whether or not the user has
/// said what it is. `None` for github.com, gitlab.com, services we cannot
/// talk to, and anything that is not a network repository.
pub fn offer_for(remote: &RemoteInfo) -> Option<Offer> {
    let address = address(&remote.url)?;
    if public(&address.name).is_some() || unsupported(&address.name) {
        return None;
    }
    Some(Offer {
        host: address.host(),
        path: address.path.join("/"),
        remote: remote.name.clone(),
        guess: guess(&address.name),
    })
}

/// Servers the sidebar offers to connect: `origin`'s when the user has not
/// said what it is; otherwise, while no remote is on a service we can show,
/// every such server the remotes point at. Asking sends nothing anywhere.
pub fn offers(remotes: &[RemoteInfo], servers: &Servers) -> Vec<Offer> {
    // A remote on a server installed under a path is known by the path.
    let unknown = |remote: &RemoteInfo| {
        offer_for(remote)
            .filter(|offer| !servers.contains_key(&offer.host) && parse_remote(&remote.url, servers).is_none())
    };
    if let Some(offer) = remotes.iter().find(|remote| remote.name == "origin").and_then(unknown) {
        return vec![offer];
    }
    if detect(remotes, servers).is_some() {
        return Vec::new();
    }
    let mut offers: Vec<Offer> = Vec::new();
    for offer in remotes.iter().filter_map(unknown) {
        if !offers.iter().any(|seen| seen.host == offer.host) {
            offers.push(offer);
        }
    }
    offers
}

// ---- model ----------------------------------------------------------------

/// `Default` is there so a backend can leave fields it does not know (or
/// that were added after it was written) at their "unknown" value with
/// `..Default::default()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PullRequest {
    /// The number the service shows: a GitLab merge request's `iid`.
    pub number: u64,
    pub title: String,
    pub author: String,
    pub draft: bool,
    /// Branch the changes come from, and the commit at its tip.
    pub head_ref: String,
    pub head_sha: String,
    /// `owner/name` of the repository the head branch lives in. `None` when
    /// the fork it came from has been deleted or cannot be seen, and for a
    /// GitLab merge request from a fork until the fork's path has been
    /// looked up ([`Backend::details`]; its list names the fork by id only).
    pub head_repo: Option<String>,
    pub base_ref: String,
    /// Seconds since the epoch; 0 when unknown.
    pub updated: i64,
    /// What the reviewers say. [`Review::Unknown`] (shown as nothing) until
    /// the service has told, and for services that do not.
    pub review: Review,
    /// Approvals given and still needed, where the service counts them;
    /// `None` when unknown.
    pub approvals: Option<Approvals>,
}

impl PullRequest {
    /// Whether the head branch lives in `repo` itself rather than in a fork.
    pub fn is_from(&self, repo: &RepoId) -> bool {
        self.head_repo.as_deref().is_some_and(|full| full.eq_ignore_ascii_case(&repo.slug()))
    }
}

/// What the reviewers have said about a pull request: GitHub's review
/// decision, or what a GitLab merge request's approvals come to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Review {
    /// Not known: not asked (GitHub without a sign-in), not answered yet,
    /// or a service that does not say. Nothing is shown for it.
    #[default]
    Unknown,
    /// Nobody has approved it or asked for changes, and no rule requires it.
    Unreviewed,
    /// Approved: by the reviews the rules require, or by someone when no
    /// rule requires any.
    Approved,
    /// A reviewer asked for changes.
    ChangesRequested,
    /// The rules require reviews (approvals) that have not been given yet.
    Required,
}

/// A pull request's approvals: given so far and, where the rules require
/// some, how many more are needed. GitHub only says how many were given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Approvals {
    pub given: u32,
    /// Still needed before the rules are met; 0 when met or unknown.
    pub left: u32,
}

impl Approvals {
    /// `1/2`: approvals given out of those the rules ask for, while some
    /// are still missing.
    pub fn fraction(&self) -> Option<String> {
        (self.left > 0).then(|| format!("{}/{}", self.given, self.given + self.left))
    }
}

/// What a backend learns after the lists, at the end of a round
/// ([`Backend::details`]): how the open pull requests stand with their
/// reviewers, which forks they come from, and the repository's parent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    /// Pull requests whose [`PullRequest::review`], `approvals` or
    /// `head_repo` changed, by number; the other fields are not used.
    pub pulls: Vec<PullRequest>,
    /// The repository this one is a fork of, once the service has said.
    pub parent: Option<Parent>,
}

/// The repository a fork was made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parent {
    pub repo: RepoId,
    /// Its default branch: where a pull request into it merges unless told
    /// otherwise. Empty when the service did not say.
    pub default_branch: String,
}

/// A pull request between two repositories of the same service: from a
/// branch of `from` into `into`. One of them is the repository shown (a
/// fork's pull request into its parent, or one from a fork of it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Across {
    pub from: RepoId,
    pub into: RepoId,
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
    for run in runs.iter().filter(|run| same_commit(&run.head_sha, head_sha)) {
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

/// Whether two commit ids name the same commit: equal, or one the other's
/// abbreviation of at least 12 digits (Bitbucket lists a pull request's
/// head by its first 12).
pub fn same_commit(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    !short.is_empty()
        && (short == long || (short.len() >= 12 && long.get(..short.len()).is_some_and(|start| start.eq_ignore_ascii_case(short))))
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
        let repository = service.repo_word();
        match self {
            Problem::RateLimited { until } => format!("Rate limited until {}", clock(*until)),
            Problem::NotSignedIn => format!("Not signed in: private {repository}"),
            Problem::SignInRejected => format!("Sign-in refused: private {repository}"),
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
        let (repository, repositories) = (service.repo_word(), service.repos_word());
        let sign_in = sign_in_advice(service);
        let advice = match self {
            Problem::RateLimited { .. } => format!(
                "{host} asked us to slow down. What is shown is the last good answer; \
                 refreshing resumes by itself. Signing in raises the limit."
            ),
            Problem::NotSignedIn => format!(
                "{host} only shows public {repositories} without a sign-in, and it does not \
                 show this one. {sign_in}"
            ),
            Problem::SignInRejected => format!(
                "{host} refused the saved sign-in (expired or revoked?), and does not show \
                 this {repository} without one. {sign_in}"
            ),
            Problem::NoAccess => format!(
                "{host} answered \"not found\": the {repository} is gone, or the signed-in \
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

/// How to sign in to `service`, in one or two sentences.
pub fn sign_in_advice(service: Service) -> &'static str {
    match service {
        Service::GitHub => {
            "Set GITHUB_TOKEN, run `gh auth login`, or sign in through Git's credential \
             manager (any HTTPS fetch or push does it), then refresh."
        }
        Service::GitHubEnterprise => {
            "Set GH_ENTERPRISE_TOKEN, run `gh auth login --hostname` with this server's \
             name, or sign in through Git's credential manager (any HTTPS fetch or push \
             does it), then refresh."
        }
        Service::GitLab => {
            "Set GITLAB_TOKEN (and GITLAB_HOST for your own server), run `glab auth login`, \
             or sign in through Git's credential manager (any HTTPS fetch or push does it), \
             then refresh."
        }
        Service::Bitbucket => {
            "Set BITBUCKET_EMAIL and BITBUCKET_API_TOKEN (an Atlassian API token with Bitbucket \
             scopes), or BITBUCKET_ACCESS_TOKEN (a repository or workspace access token), or sign \
             in through Git's credential manager (any HTTPS fetch or push does it), then refresh."
        }
        Service::Codeberg => {
            "Set CODEBERG_TOKEN (an access token from Codeberg's Settings > Applications), sign \
             in with `tea login add`, or sign in through Git's credential manager (any HTTPS \
             fetch or push does it), then refresh."
        }
        Service::Gitea => {
            "Set GITEA_TOKEN and GITEA_HOST (this server's name), sign in with `tea login add`, \
             or sign in through Git's credential manager (any HTTPS fetch or push does it), \
             then refresh."
        }
        Service::Forgejo => {
            "Set FORGEJO_TOKEN and FORGEJO_HOST (this server's name), sign in with `tea login \
             add`, or sign in through Git's credential manager (any HTTPS fetch or push does \
             it), then refresh."
        }
        Service::AzureDevOps => {
            "Set AZURE_DEVOPS_EXT_PAT (a personal access token), run `az login`, or sign in \
             through Git's credential manager (any HTTPS fetch or push does it), then refresh."
        }
    }
}

/// A branch name a service answered with, when git could have it: no
/// spaces or control characters, at most 255 bytes. Empty otherwise.
fn branch_name(text: &str) -> String {
    let fits = text.len() <= 255 && text.chars().all(|c| !c.is_control() && !c.is_whitespace());
    if fits { text.to_owned() } else { String::new() }
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
    pub fn describe(self, service: Service) -> String {
        let repositories = format!("public {} only", service.repos_word());
        match self {
            Auth::Unknown => String::new(),
            Auth::Anonymous => format!("Not signed in: {repositories}"),
            Auth::SignedIn(TokenSource::Env(name)) => format!("Signed in with ${name}"),
            Auth::SignedIn(TokenSource::Cli) => match service {
                Service::GitLab => "Signed in through the GitLab CLI".to_owned(),
                Service::Gitea | Service::Forgejo | Service::Codeberg => "Signed in through tea's sign-in".to_owned(),
                Service::AzureDevOps => "Signed in through the Azure CLI".to_owned(),
                _ => "Signed in through the GitHub CLI".to_owned(),
            },
            Auth::SignedIn(TokenSource::CredentialHelper) => {
                "Signed in through Git's credential helper".to_owned()
            }
            Auth::Rejected => format!("The saved sign-in was refused: {repositories}"),
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
    /// The repository this one is a fork of, when the service says so.
    pub parent: Option<Parent>,
}

// ---- time -----------------------------------------------------------------

pub fn epoch_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `14:32` in local time.
pub fn clock(epoch: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(epoch as i64, 0).single() {
        Some(time) => crate::prefs::appearance::custom_clock(&time).unwrap_or_else(|| time.format("%H:%M").to_string()),
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
pub struct Token(String, Scheme);

/// How a token is presented.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Scheme {
    /// `Authorization: Bearer <token>`.
    Bearer,
    /// HTTP Basic with this user name and the token as the password:
    /// Bitbucket's API tokens and app passwords, Azure DevOps' personal
    /// access tokens (whose user name may be anything, even empty).
    Basic(String),
}

impl Token {
    /// Accepts anything that can be a token: printable ASCII without spaces.
    /// Microsoft's sign-in tokens run to a few kilobytes.
    fn new(raw: &str) -> Option<Token> {
        let raw = raw.trim();
        let plausible = (8..=16 * 1024).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_graphic());
        plausible.then(|| Token(raw.to_owned(), Scheme::Bearer))
    }

    /// A token sent as the password of HTTP Basic sign-in as `user`. A user
    /// name with a colon or anything unprintable cannot be sent that way.
    fn basic(user: &str, raw: &str) -> Option<Token> {
        let user = user.trim();
        let printable = user.chars().all(|c| !c.is_control()) && !user.contains(':') && user.len() <= 256;
        let token = Token::new(raw)?;
        printable.then(|| Token(token.0, Scheme::Basic(user.to_owned())))
    }

    fn authorization(&self) -> String {
        match &self.1 {
            Scheme::Bearer => format!("Bearer {}", self.0),
            Scheme::Basic(user) => format!("Basic {}", self.basic_credentials(user)),
        }
    }

    /// `user:token` in base64, as HTTP Basic sends it.
    fn basic_credentials(&self, user: &str) -> String {
        base64_padded(format!("{user}:{}", self.0).as_bytes())
    }

    /// `text` with the token blanked out, and the Basic credentials it was
    /// sent in, should something echo the header.
    fn scrub(&self, text: &str) -> String {
        let text = text.replace(&self.0, "***");
        match &self.1 {
            Scheme::Bearer => text,
            Scheme::Basic(user) => text.replace(&self.basic_credentials(user), "***"),
        }
    }
}

/// Standard base64 with padding.
fn base64_padded(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, &byte| (n << 8) | byte as u32) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            out.push(if i <= chunk.len() { DIGITS[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(***)")
    }
}

/// The places a token can come from. A trait so the lookup order and which
/// host each source is asked about can be tested without touching the real
/// environment.
pub trait Secrets: Send {
    fn env(&self, name: &str) -> Option<String>;
    /// The service's own command line tool (`gh`, `glab`): the token it
    /// keeps for exactly `host`, when installed and signed in there.
    fn cli(&self, service: Service, host: &str) -> Option<String>;
    /// Git's credential helper's password for `https://<host>`, asked in a
    /// way that can never prompt.
    fn credential_helper(&self, host: &str) -> Option<String>;

    /// The helper's user name and password for `https://<host>/<path>`:
    /// for services that sign in with both (Bitbucket), or keep one sign-in
    /// per path (Azure DevOps organisations; git only passes the path on
    /// where `credential.useHttpPath` says so). By default the password of
    /// [`Secrets::credential_helper`], with no user name.
    fn credential(&self, host: &str, path: &str) -> Option<(String, String)> {
        let _ = path;
        self.credential_helper(host).map(|password| (String::new(), password))
    }
}

/// The environment variables that may hold a token for `repo`'s host, in
/// the order they are tried.
///
/// A variable is only used for the host it belongs to. `GITHUB_TOKEN` and
/// `GH_TOKEN` are github.com's (as `gh` has it) and never go to an
/// Enterprise server. `GH_ENTERPRISE_TOKEN` is for Enterprise servers, but
/// not one other than the server `GH_HOST` names when that is set.
/// `GITLAB_TOKEN` belongs to the server `GITLAB_HOST` names, gitlab.com
/// when that is not set (as `glab` has it).
fn env_names(secrets: &dyn Secrets, repo: &RepoId) -> &'static [&'static str] {
    let names = |variable: &str| secrets.env(variable).map(|value| same_host(&value, &repo.host));
    match repo.service {
        Service::GitHub => &["GITHUB_TOKEN", "GH_TOKEN"],
        Service::GitHubEnterprise => match names("GH_HOST") {
            Some(false) => &[],
            Some(true) | None => &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"],
        },
        Service::GitLab => match names("GITLAB_HOST") {
            Some(true) => &["GITLAB_TOKEN"],
            None if repo.host == "gitlab.com" => &["GITLAB_TOKEN"],
            _ => &[],
        },
        Service::Gitea | Service::Forgejo | Service::Codeberg => gitea::env_names(repo.service, |name| names(name)),
        // Their tokens come with a sign-in scheme: see `bitbucket` and `azure`.
        Service::Bitbucket | Service::AzureDevOps => &[],
    }
}

/// Whether a `GH_HOST` / `GITLAB_HOST` value (`git.example.com`, or a URL
/// such as `https://git.example.com/`) names `host`. A server installed
/// under a path is named with its path or by its name alone.
fn same_host(value: &str, host: &str) -> bool {
    let value = value.trim();
    let value = value.strip_prefix("https://").or_else(|| value.strip_prefix("http://")).unwrap_or(value);
    let value = value.trim_end_matches('/').to_ascii_lowercase();
    let names = |host: &str| value == host || value.strip_suffix(":443") == Some(host);
    !value.is_empty() && (names(host) || (host.contains('/') && names(server_name(host))))
}

/// Looks for a token for `repo`'s host, cheapest and most explicit source
/// first. Each source is asked about that host only, so a token found here
/// is one that belongs to it. Tokens in `rejected` (already refused by the
/// service) are passed over, so a stale environment variable does not hide
/// a working sign-in further down.
pub fn find_token(secrets: &dyn Secrets, repo: &RepoId, rejected: &[Token]) -> Option<(Token, TokenSource)> {
    match repo.service {
        Service::Bitbucket => return bitbucket::find_token(secrets, repo, rejected),
        Service::AzureDevOps => return azure::find_token(secrets, repo, rejected),
        _ => {}
    }
    let usable = |raw: Option<String>| {
        raw.and_then(|raw| Token::new(&raw)).filter(|token| !rejected.contains(token))
    };
    for &name in env_names(secrets, repo) {
        if let Some(token) = usable(secrets.env(name)) {
            return Some((token, TokenSource::Env(name)));
        }
    }
    // Both know a server by its name, not by the path it is installed under.
    let server = server_name(&repo.host);
    if let Some(token) = usable(secrets.cli(repo.service, server)) {
        return Some((token, TokenSource::Cli));
    }
    usable(secrets.credential_helper(server)).map(|token| (token, TokenSource::CredentialHelper))
}

/// No token from anywhere: for the offline fixture.
struct NoSecrets;

impl Secrets for NoSecrets {
    fn env(&self, _name: &str) -> Option<String> {
        None
    }
    fn cli(&self, _service: Service, _host: &str) -> Option<String> {
        None
    }
    fn credential_helper(&self, _host: &str) -> Option<String> {
        None
    }
}

/// The real environment: variables, `gh` / `glab`, and `git credential`.
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

    fn cli(&self, service: Service, host: &str) -> Option<String> {
        match service {
            // tea keeps its sign-ins in a file and prints none of them.
            Service::Gitea | Service::Forgejo | Service::Codeberg => gitea::tea_token(host),
            // `az` takes a few seconds to start.
            Service::AzureDevOps => capture(cli_command(service, host)?, b"", Duration::from_secs(30)),
            _ => capture(cli_command(service, host)?, b"", Duration::from_secs(10)),
        }
    }

    fn credential_helper(&self, host: &str) -> Option<String> {
        let answer = capture(credential_command(&self.workdir), credential_query(host).as_bytes(), Duration::from_secs(15))?;
        password_from(&answer)
    }

    fn credential(&self, host: &str, path: &str) -> Option<(String, String)> {
        let query = credential_query_at(host, path);
        let answer = capture(credential_command(&self.workdir), query.as_bytes(), Duration::from_secs(15))?;
        let password = password_from(&answer)?;
        let user = answer.lines().find_map(|line| line.strip_prefix("username=")).unwrap_or_default();
        Some((user.to_owned(), password))
    }
}

/// Asks `gh`, `glab` or `az` for the token it keeps for `host`, without a
/// prompt; `None` for a service without such a tool.
///
/// Both tools let a token variable win over what they keep, for any host
/// (`glab config get token` returns `GITLAB_TOKEN` whatever `--host` says),
/// so those variables are taken out of the tool's environment: the answer
/// is then the token stored for exactly that host, or nothing. Update
/// checks and glab's usage reports are switched off, so the tool contacts
/// nothing on our behalf.
fn cli_command(service: Service, host: &str) -> Option<Command> {
    let mut cmd;
    match service {
        Service::AzureDevOps => return Some(azure::cli_command()),
        Service::Bitbucket | Service::Gitea | Service::Forgejo | Service::Codeberg => return None,
        Service::GitHub | Service::GitHubEnterprise => {
            cmd = Command::new("gh");
            cmd.args(["auth", "token", "--hostname", host])
                .env("GH_PROMPT_DISABLED", "1")
                .env("GH_NO_UPDATE_NOTIFIER", "1");
            for name in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "GH_HOST"] {
                cmd.env_remove(name);
            }
        }
        Service::GitLab => {
            cmd = Command::new("glab");
            cmd.args(["config", "get", "token", "--host", host])
                .env("GLAB_NO_PROMPT", "1")
                .env("NO_PROMPT", "1")
                .env("GLAB_CHECK_UPDATE", "false")
                .env("GLAB_SEND_TELEMETRY", "false");
            for name in ["GITLAB_TOKEN", "GITLAB_ACCESS_TOKEN", "OAUTH_TOKEN", "GITLAB_HOST", "GL_HOST"] {
                cmd.env_remove(name);
            }
        }
    }
    Some(cmd)
}

/// `git credential fill`, asked so that nothing can open a window or wait
/// for a keyboard: the credential manager is told not to interact, and
/// every askpass fallback and the terminal prompt are disabled. With
/// nothing stored, git simply fails.
fn credential_command(workdir: &Path) -> Command {
    let mut cmd = Command::new(crate::git::cli::program());
    cmd.current_dir(workdir)
        .args(["-c", "core.askPass=", "credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "");
    cmd
}

/// What `git credential fill` is asked: the HTTPS credential for `host`.
/// [`RepoId::host`] cannot hold a line break, so nothing else can be
/// smuggled into the query.
fn credential_query(host: &str) -> String {
    format!("protocol=https\nhost={host}\n\n")
}

/// [`credential_query`] for one address on the host. `path` is built from
/// a [`RepoId`]'s parts, which hold no line break either; an empty one is
/// left out.
fn credential_query_at(host: &str, path: &str) -> String {
    match path.trim_matches('/') {
        "" => credential_query(host),
        path => format!("protocol=https\nhost={host}\npath={path}\n\n"),
    }
}

/// The `password=` line of `git credential fill` output.
pub fn password_from(answer: &str) -> Option<String> {
    answer.lines().find_map(|line| line.strip_prefix("password=")).map(str::to_owned)
}


/// The offline fixture when its folder holds a file named `signed-in`: a
/// made-up token "from the CLI", so signed-in states (opening a pull
/// request) can be shown. It only ever reaches the canned transport.
/// Bitbucket has no CLI: there it comes from the credential helper.
struct FixtureSignIn;

impl Secrets for FixtureSignIn {
    fn env(&self, _name: &str) -> Option<String> {
        None
    }
    fn cli(&self, _service: Service, _host: &str) -> Option<String> {
        Some("fixture-token-not-a-real-one".to_owned())
    }
    fn credential_helper(&self, _host: &str) -> Option<String> {
        Some("fixture-token-not-a-real-one".to_owned())
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

/// HTTP for the clients. Any status is a response; `Err` means no answer
/// at all.
pub trait Transport: Send {
    fn get(&mut self, request: &HttpRequest<'_>) -> Result<HttpResponse, String>;

    /// A POST with a JSON body (`request.etag` is not used). Never retried
    /// and never redirected, so nothing can be created twice.
    fn post(&mut self, request: &HttpRequest<'_>, json: &str) -> Result<HttpResponse, String> {
        let _ = (request, json);
        Err("sending is not supported here".to_owned())
    }
}

/// Whether `url` is below `base`: `https://host/api/v4/...` for the base
/// `https://host/api/v4`, but not `https://host/api/v40` or
/// `https://host/api/v4.evil.example`. Nothing a URL parser could read
/// differently (a backslash, whitespace, control characters) may follow.
pub fn within(base: &str, url: &str) -> bool {
    url.strip_prefix(base)
        .is_some_and(|rest| rest.starts_with('/') && rest.bytes().all(|b| b.is_ascii_graphic() && b != b'\\'))
}

/// The real network, through `ureq`, for one service's API.
pub struct Wire {
    agent: ureq::Agent,
    /// The API's address (no trailing slash). Requests below it carry the
    /// token; anything else is sent without one.
    api: String,
    /// Headers the service wants on every request.
    headers: &'static [(&'static str, &'static str)],
}

impl Wire {
    pub fn new(api: String, headers: &'static [(&'static str, &'static str)]) -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            // Statuses are data here (304, 403, 404 all mean something).
            .http_status_as_error(false)
            // Redirects come back to the client, which only follows
            // them within the API.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .timeout_global(Some(Duration::from_secs(25)))
            .user_agent(concat!("GitKrill/", env!("CARGO_PKG_VERSION")))
            .build();
        Wire { agent: config.into(), api, headers }
    }

    /// The `Authorization` header for `request`: the one place a token
    /// leaves the process, and only towards this service's API over HTTPS
    /// (`https_only` refuses anything else before it is sent).
    fn authorization(&self, request: &HttpRequest<'_>) -> Option<String> {
        let token = request.token?;
        let allowed = within(&self.api, request.url) || beside(&self.api).as_deref() == Some(request.url);
        (self.api.starts_with("https://") && allowed).then(|| token.authorization())
    }
}

/// The one address outside an API's own that its token may go to: GitHub
/// Enterprise Server answers GraphQL at `/api/graphql`, beside its REST API
/// at `/api/v3` rather than below it. github.com's (and `*.ghe.com`'s) is
/// below the REST API's address already.
pub fn beside(api: &str) -> Option<String> {
    api.strip_suffix("/api/v3").map(|root| format!("{root}/api/graphql"))
}

impl Transport for Wire {
    fn get(&mut self, request: &HttpRequest<'_>) -> Result<HttpResponse, String> {
        let mut call = self.agent.get(request.url);
        for (name, value) in self.headers {
            call = call.header(*name, *value);
        }
        if let Some(etag) = request.etag {
            call = call.header("If-None-Match", etag);
        }
        if let Some(authorization) = self.authorization(request) {
            call = call.header("Authorization", authorization);
        }
        receive(call.call().map_err(explain)?)
    }

    fn post(&mut self, request: &HttpRequest<'_>, json: &str) -> Result<HttpResponse, String> {
        let mut call = self.agent.post(request.url).header("Content-Type", "application/json");
        for (name, value) in self.headers {
            call = call.header(*name, *value);
        }
        if let Some(authorization) = self.authorization(request) {
            call = call.header("Authorization", authorization);
        }
        receive(call.send(json).map_err(explain)?)
    }
}

/// A `ureq` answer as an [`HttpResponse`], with a bounded body.
fn receive(mut response: ureq::http::Response<ureq::Body>) -> Result<HttpResponse, String> {
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned())))
        .collect();
    let body = match status {
        304 => String::new(),
        _ => response.body_mut().with_config().limit(32 * 1024 * 1024).read_to_string().map_err(explain)?,
    };
    Ok(HttpResponse { status, headers, body })
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

/// Development stand-in for the network: answers from files in a folder.
///
/// A request is answered with `<endpoint>.json`, after the last part of its
/// path: `pulls`, `issues` and `runs` on GitHub, Gitea and Forgejo (`tasks`
/// for runs on servers older than the runs endpoint), `merge_requests`,
/// `issues` and `pipelines` on GitLab, `pullrequests`, `issues` and
/// `pipelines` on Bitbucket, and `pullrequests`, `wiql` (the work item
/// query), `workitems`, `builds` and `repositories` on Azure DevOps. An
/// optional `<endpoint>.head` gives a status
/// code on its first line and `name: value` headers after it, which is how
/// to stage a rate limit or an outage.
///
/// Opening a pull request (a POST) is answered from `create.json` and
/// `create.head` the same way, with 201 as the default status; the body that
/// was sent is left in `create.sent.json` to check what the dialog asked for.
/// Once one has been opened, `pulls.after.json` (if present) takes the place
/// of `pulls.json` (`pullrequests.after.json` of `pullrequests.json`), so
/// the list can be seen to pick it up.
///
/// What [`Backend::details`] asks comes from more files: GitHub's GraphQL
/// (review decisions) from `graphql.json`, its repository (a fork's parent)
/// from `repo.json`; on GitLab a merge request's approvals from
/// `approvals-<iid>.json`, the project (its parent) from `project.json` and
/// another project by id (a fork) from `project-<id>.json`.
struct Canned {
    dir: PathBuf,
    created: bool,
}

impl Canned {
    /// The canned answer called `name`; `status` when no `.head` file says
    /// otherwise.
    fn answer(&self, name: &str, status: u16) -> HttpResponse {
        let mut response = HttpResponse { status, ..Default::default() };
        if let Ok(head) = std::fs::read_to_string(self.dir.join(format!("{name}.head"))) {
            let mut lines = head.lines();
            response.status = lines.next().and_then(|line| line.trim().parse().ok()).unwrap_or(status);
            response.headers = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
                .collect();
        }
        match std::fs::read_to_string(self.dir.join(format!("{name}.json"))) {
            Ok(body) => response.body = body,
            Err(_) if response.status == status => response.status = 404,
            Err(_) => {}
        }
        response
    }
}

impl Transport for Canned {
    fn get(&mut self, request: &HttpRequest<'_>) -> Result<HttpResponse, String> {
        let path = request.url.split('?').next().unwrap_or_default();
        if let Some(name) = detail_fixture(path) {
            return Ok(self.answer(&name, 200));
        }
        let endpoints = ["pulls", "issues", "runs", "merge_requests", "pipelines"];
        // Bitbucket and Azure DevOps (`pullrequests`), Azure DevOps' builds,
        // work items and repositories, older Gitea and Forgejo runs (`tasks`).
        let more = ["pullrequests", "builds", "workitems", "repositories", "tasks"];
        let list = endpoints.into_iter().chain(more).find(|list| path.rsplit('/').next() == Some(*list));
        let list = list.ok_or("no fixture for this address")?;
        let after = format!("{list}.after");
        if matches!(list, "pulls" | "pullrequests") && self.created && self.dir.join(format!("{after}.json")).exists() {
            return Ok(self.answer(&after, 200));
        }
        Ok(self.answer(list, 200))
    }

    fn post(&mut self, request: &HttpRequest<'_>, body: &str) -> Result<HttpResponse, String> {
        let path = request.url.split('?').next().unwrap_or_default();
        if path.ends_with("/graphql") {
            return Ok(self.answer("graphql", 200));
        }
        // Azure DevOps asks for its work items with a query (`wiql`).
        if path.ends_with("/wiql") {
            return Ok(self.answer("wiql", 200));
        }
        if !["/pulls", "/merge_requests", "/pullrequests"].iter().any(|end| path.ends_with(end)) {
            return Err("no fixture for this address".to_owned());
        }
        let _ = std::fs::write(self.dir.join("create.sent.json"), body);
        let response = self.answer("create", 201);
        self.created |= response.status == 201;
        Ok(response)
    }
}

/// The fixture file (without `.json`) answering one of the requests of
/// [`Backend::details`], if `path` is one: see [`Canned`].
fn detail_fixture(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        [.., "merge_requests", iid, "approvals"] => Some(format!("approvals-{iid}")),
        [.., "projects", id] if id.bytes().all(|b| b.is_ascii_digit()) => Some(format!("project-{id}")),
        [.., "projects", _] => Some("project".to_owned()),
        [.., "repos", _, _] => Some("repo".to_owned()),
        _ => None,
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
    /// GitHub's (and Azure DevOps') `X-RateLimit-*` headers, or GitLab's
    /// `RateLimit-*`.
    pub fn of(response: &HttpResponse) -> Rate {
        let number = |name: &str| response.header(name).and_then(|v| v.trim().parse().ok());
        Rate {
            remaining: number("x-ratelimit-remaining").or_else(|| number("ratelimit-remaining")),
            reset: number("x-ratelimit-reset").or_else(|| number("ratelimit-reset")),
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
    /// Open a pull request from `new.head` in `across.from` into
    /// `new.base` in `across.into`: a fork's into its parent, or one from a
    /// fork of the repository. Blocking, like [`Backend::create_pull`].
    fn create_pull_across(&mut self, new: &NewPull, across: &Across, now: u64) -> Result<PullRequest, Problem> {
        let _ = (new, across, now);
        Err(Problem::Failed("Pull requests between forks are not supported here yet".to_owned()))
    }
    /// What there is to learn after the lists, asked at the end of each
    /// round: review states of the open pull requests, the forks they come
    /// from, the repository's parent ([`Details`]). Only what changed since
    /// the last call; `None` when nothing did, or the service tells nothing
    /// more. A failure here is not a failed round: what is not known stays
    /// unknown. The lists it is about are the last [`Backend::pulls`] gave,
    /// which should carry what this found out before, so a fresh list does
    /// not lose it.
    fn details(&mut self, now: u64) -> Option<Details> {
        let _ = now;
        None
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

/// The backend for `repo`: its service's client, talking to that service's
/// API with tokens looked up for its host, or with none at all when the
/// account there is `signed_out` (see [`Disconnected`]).
fn connect(repo: &RepoId, workdir: &Path, signed_out: bool) -> Box<dyn Backend> {
    let (transport, secrets): (Box<dyn Transport>, Box<dyn Secrets>) =
        match std::env::var_os("GITKRILL_HOSTING_FIXTURE") {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                let secrets: Box<dyn Secrets> =
                    if dir.join("signed-in").exists() { Box::new(FixtureSignIn) } else { Box::new(NoSecrets) };
                (Box::new(Canned { dir, created: false }), secrets)
            }
            None => (Box::new(Wire::new(repo.api_base(), headers(repo.service))), Box::new(System::new(workdir))),
        };
    let secrets = signed_out_secrets(signed_out, secrets);
    match repo.service {
        Service::GitHub | Service::GitHubEnterprise => {
            Box::new(github::Client::new(repo.clone(), transport, secrets))
        }
        Service::GitLab => Box::new(gitlab::Client::new(repo.clone(), transport, secrets)),
        Service::Bitbucket => Box::new(bitbucket::Client::new(repo.clone(), transport, secrets)),
        Service::Gitea | Service::Forgejo | Service::Codeberg => {
            Box::new(gitea::Client::new(repo.clone(), transport, secrets))
        }
        Service::AzureDevOps => Box::new(azure::Client::new(repo.clone(), transport, secrets)),
    }
}

/// The headers `service` wants on every request.
pub fn headers(service: Service) -> &'static [(&'static str, &'static str)] {
    match service {
        Service::GitHub | Service::GitHubEnterprise => github::HEADERS,
        Service::GitLab => gitlab::HEADERS,
        Service::Bitbucket => bitbucket::HEADERS,
        Service::Gitea | Service::Forgejo | Service::Codeberg => gitea::HEADERS,
        Service::AzureDevOps => azure::HEADERS,
    }
}

/// No sign-in at all for an account the user disconnected; else `secrets`.
fn signed_out_secrets(signed_out: bool, secrets: Box<dyn Secrets>) -> Box<dyn Secrets> {
    if signed_out { Box::new(NoSecrets) } else { secrets }
}

pub mod account;
pub mod azure;
pub mod bitbucket;
pub mod gitea;
pub mod github;
pub mod gitlab;
mod signin;

// ---- the background thread ------------------------------------------------

enum Order {
    Refresh {
        /// Asked for by the user rather than by the timer.
        manual: bool,
    },
    /// Open a pull request; answered with [`Event::Created`] under `id`.
    /// Between two repositories when `across` says so.
    CreatePull { id: u64, new: NewPull, across: Option<Box<Across>> },
}

enum Event {
    Auth(Auth),
    Pulls(Outcome<PullRequest>),
    Issues(Outcome<Issue>),
    Runs(Outcome<Run>),
    /// The round is over; refresh again by itself after `next`.
    Done { next: Duration },
    Created { id: u64, result: Result<PullRequest, Problem> },
    Details(Details),
}

fn troubled<T>(outcome: &Outcome<T>) -> bool {
    matches!(outcome, Outcome::Failed(problem) if !matches!(problem, Problem::RateLimited { .. }))
}

/// Serves orders until the tab goes away (`orders` closes) or the
/// integration is switched off (`stop`). `wake` asks the UI for a frame.
fn worker(
    mut backend: Box<dyn Backend>,
    orders: Receiver<Order>,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
    wake: impl Fn(),
) {
    let mut failures = 0u32;
    // Orders taken off the channel while gathering refreshes, kept in order.
    let mut later: std::collections::VecDeque<Order> = std::collections::VecDeque::new();
    loop {
        let order = match later.pop_front() {
            Some(order) => order,
            None => match orders.recv() {
                Ok(order) => order,
                Err(_) => return,
            },
        };
        let mut manual = match order {
            Order::Refresh { manual } => manual,
            Order::CreatePull { id, new, across } => {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let result = match across.as_deref() {
                    Some(across) => backend.create_pull_across(&new, across, epoch_now()),
                    None => backend.create_pull(&new, epoch_now()),
                };
                if events.send(Event::Auth(backend.auth())).is_err()
                    || events.send(Event::Created { id, result }).is_err()
                {
                    return;
                }
                wake();
                continue;
            }
        };
        // One round answers every refresh that queued up meanwhile.
        while let Ok(order) = orders.try_recv() {
            match order {
                Order::Refresh { manual: also } => manual |= also,
                other => later.push_back(other),
            }
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
        // Then what the lists do not say, once they are on screen.
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Some(details) = backend.details(epoch_now()) {
            if events.send(Event::Details(details)).is_err() {
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
    /// The account on the repository's host is disconnected ([`Disconnected`]):
    /// no sign-in is used, whatever is found.
    pub disconnected: bool,
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
    /// Pull requests asked for and not answered yet, by id.
    creating: Vec<u64>,
    /// Answers to those, until collected with [`HostingState::take_created`].
    created: Vec<(u64, Result<PullRequest, Problem>)>,
    /// Those of `creating` opened in another repository (a fork's parent),
    /// which this list does not show.
    elsewhere: Vec<u64>,
    last_id: u64,
    /// A pull request was opened during a refresh round: refresh once more
    /// when it ends, so the list has it for certain.
    refresh_after: bool,
    /// Started without a sign-in, the account being disconnected.
    signed_out: bool,
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
            creating: Vec::new(),
            created: Vec::new(),
            elsewhere: Vec::new(),
            last_id: 0,
            refresh_after: false,
            signed_out: false,
        };
        session.request(false);
        session
    }

    fn create(&mut self, new: NewPull, across: Option<Across>) -> u64 {
        self.last_id += 1;
        let id = self.last_id;
        let elsewhere = across.as_ref().is_some_and(|across| !across.into.same(&self.repo));
        if self.orders.send(Order::CreatePull { id, new, across: across.map(Box::new) }).is_ok() {
            self.creating.push(id);
            if elsewhere {
                self.elsewhere.push(id);
            }
        } else {
            self.created.push((id, Err(stopped_problem())));
        }
        id
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
        let problem = stopped_problem();
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
                        Event::Created { id, result } => {
                            self.creating.retain(|pending| *pending != id);
                            let listed = !self.elsewhere.contains(&id);
                            self.elsewhere.retain(|other| *other != id);
                            if let (Ok(pull), true) = (&result, listed) {
                                // Listed at once; the refresh below confirms it.
                                if !view.pulls.items.iter().any(|known| known.number == pull.number) {
                                    view.pulls.items.insert(0, pull.clone());
                                }
                                self.refresh_after = true;
                            }
                            self.created.push((id, result));
                        }
                        Event::Details(details) => {
                            for pull in &mut view.pulls.items {
                                let Some(known) = details.pulls.iter().find(|known| known.number == pull.number)
                                else {
                                    continue;
                                };
                                pull.review = known.review;
                                pull.approvals = known.approvals;
                                if known.head_repo.is_some() {
                                    pull.head_repo = known.head_repo.clone();
                                }
                            }
                            if details.parent.is_some() {
                                view.parent = details.parent;
                            }
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.view.busy {
                        self.stopped();
                    }
                    for id in std::mem::take(&mut self.creating) {
                        self.created.push((id, Err(stopped_problem())));
                    }
                    break;
                }
            }
        }
        if self.refresh_after && !self.view.busy {
            self.refresh_after = false;
            self.request(false);
        }
    }
}

fn stopped_problem() -> Problem {
    Problem::Failed("The background worker stopped".to_owned())
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
    /// switched off or no remote points at a known service: github.com,
    /// gitlab.com, or a server in `servers` the user said runs GitHub
    /// Enterprise or GitLab. A repository on a host in `disconnected` is
    /// shown without a sign-in.
    pub fn sync(
        &mut self,
        ctx: &egui::Context,
        workdir: &Path,
        remotes: &[RemoteInfo],
        servers: &Servers,
        disconnected: &Disconnected,
        enabled: bool,
    ) -> Option<Shown> {
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        // Only poll while someone is looking at the window.
        let attended = ctx.input(|input| input.focused);
        let connect = |repo: &RepoId, signed_out: bool| connect(repo, workdir, signed_out);
        let shown = self.sync_accounts(remotes, servers, disconnected, enabled, attended, connect, wake);
        if let Some(due) = self.session.as_ref().and_then(|session| session.next_auto)
            && attended
        {
            ctx.request_repaint_after(due.saturating_duration_since(Instant::now()));
        }
        shown
    }

    /// [`HostingState::sync_with`], in step with the accounts disconnected
    /// in Preferences > Integrations: a session runs signed out exactly when
    /// its host is in `disconnected`, and starts over when that changes.
    /// `connect` is told whether to sign in.
    #[allow(clippy::too_many_arguments)]
    fn sync_accounts(
        &mut self,
        remotes: &[RemoteInfo],
        servers: &Servers,
        disconnected: &Disconnected,
        enabled: bool,
        attended: bool,
        connect: impl FnOnce(&RepoId, bool) -> Box<dyn Backend>,
        wake: impl Fn() + Send + 'static,
    ) -> Option<Shown> {
        let signed_out = |repo: &RepoId| disconnected.contains(&repo.host);
        if self.session.as_ref().is_some_and(|session| session.signed_out != signed_out(&session.repo)) {
            // Dropping the session stops its thread.
            self.session = None;
        }
        let mut started = None;
        let mut shown = self.sync_with(
            remotes,
            servers,
            enabled,
            attended,
            |repo| {
                let out = signed_out(repo);
                started = Some(out);
                connect(repo, out)
            },
            wake,
        );
        if let (Some(out), Some(session)) = (started, &mut self.session) {
            session.signed_out = out;
        }
        if let (Some(shown), Some(session)) = (&mut shown, &self.session) {
            shown.disconnected = session.signed_out;
        }
        shown
    }

    fn sync_with(
        &mut self,
        remotes: &[RemoteInfo],
        servers: &Servers,
        enabled: bool,
        attended: bool,
        connect: impl FnOnce(&RepoId) -> Box<dyn Backend>,
        wake: impl Fn() + Send + 'static,
    ) -> Option<Shown> {
        let target = if enabled { detect(remotes, servers) } else { None };
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
            disconnected: session.signed_out,
        })
    }

    /// Refreshes now, at the user's request.
    pub fn refresh(&mut self) {
        if let Some(session) = &mut self.session {
            session.request(true);
        }
    }

    /// The remote and hosted repository the integration is running for, if
    /// it is: what "Create pull request" works with.
    pub fn connected(&self) -> Option<Shown> {
        let session = self.session.as_ref()?;
        Some(Shown {
            remote: session.remote.clone(),
            repo: session.repo.clone(),
            view: session.view.clone(),
            disconnected: session.signed_out,
        })
    }

    /// Takes in what the background thread has produced, without the rest
    /// of [`HostingState::sync`]: for views that wait on an answer while the
    /// sidebar may be hidden.
    pub fn poll(&mut self) {
        if let Some(session) = &mut self.session {
            session.poll();
        }
    }

    /// Asks the service to open a pull request, on the background thread.
    /// Returns the id to collect the answer with, or `None` when the
    /// integration is not running.
    pub fn create_pull(&mut self, new: NewPull) -> Option<u64> {
        Some(self.session.as_mut()?.create(new, None))
    }

    /// [`HostingState::create_pull`] between two repositories: from a fork
    /// into its parent. Answered the same way; a pull request opened in a
    /// repository other than the one shown is not added to its list.
    pub fn create_pull_across(&mut self, new: NewPull, across: Across) -> Option<u64> {
        Some(self.session.as_mut()?.create(new, Some(across)))
    }

    /// The answer to [`HostingState::create_pull`] once it has arrived.
    pub fn take_created(&mut self, id: u64) -> Option<Result<PullRequest, Problem>> {
        let session = self.session.as_mut()?;
        let index = session.created.iter().position(|(done, _)| *done == id)?;
        Some(session.created.remove(index).1)
    }

    /// Whether the answer to [`HostingState::create_pull`] is still on its
    /// way. False also when the integration stopped meanwhile: then the
    /// answer will never come.
    pub fn is_creating(&self, id: u64) -> bool {
        self.session.as_ref().is_some_and(|session| session.creating.contains(&id))
    }
}

/// What to tell the user when opening a pull request failed: one or two
/// sentences, with the way out when there is one.
pub fn create_problem(problem: &Problem, service: Service) -> String {
    let host = service.name();
    let word = service.pull_word();
    // The same ways to sign in as the sidebar's tooltip gives.
    let sign_in = sign_in_advice(service).trim_end_matches(", then refresh.");
    match problem {
        Problem::NotSignedIn => format!(
            "Opening a {word} from here needs a {host} sign-in. {sign_in}. Open on {host} works without one."
        ),
        Problem::SignInRejected => format!(
            "{host} refused the saved sign-in (expired or revoked?). {sign_in}. Open on {host} works without one."
        ),
        Problem::NoAccess => format!(
            "{host} answered \"not found\": the signed-in account cannot see this {}, or              its token is not allowed to open {word}s.",
            service.repo_word()
        ),
        Problem::RateLimited { until } => {
            format!("{host} asked us to slow down. Try again after {}.", clock(*until))
        }
        other => other.line(service),
    }
}

// ---- checking out a pull request ------------------------------------------

/// The local branch a pull request is checked out as: `pr/12`, or `mr/12`
/// for a GitLab merge request.
pub fn pull_branch(service: Service, number: u64) -> String {
    if service.is_gitlab() { format!("mr/{number}") } else { format!("pr/{number}") }
}

/// Fetches a pull request's head from `remote` into [`pull_branch`] and
/// checks it out. With `on_branch` (that branch is the current one, which
/// git refuses to fetch into) the branch is brought forward instead.
///
/// Neither form forces: if the local branch has commits the pull request
/// does not, git says so and nothing is lost. Only the pull request's
/// commits are fetched, not the remote's tags.
pub fn checkout_pull(remote: &str, service: Service, number: u64, on_branch: bool) -> GitCmd {
    let branch = pull_branch(service, number);
    let head = service.pull_ref(number);
    let what = format!("{} {}", service.pull_word(), service.pull_number(number));
    if on_branch {
        GitCmd::new(format!("Update {what}"), &["fetch", "--no-tags", remote, &head])
            .step(&["merge", "--ff-only", "FETCH_HEAD"])
            .undoable(UndoKind::HeadMove)
            .done(format!("Updated {branch} to {what}"))
    } else {
        GitCmd::new(format!("Check out {what}"), &["fetch", "--no-tags", remote, &format!("{head}:{branch}")])
            .step(&["checkout", &branch])
            .undoable(UndoKind::Checkout)
            .done(format!("Checked out {what} as {branch}"))
    }
}

/// How to check out `pr`: from the service's pull request ref where it
/// keeps one ([`checkout_pull`]), else (Bitbucket, Azure DevOps) from its
/// head branch, which only works when that branch is in the repository
/// itself. `None` when there is no way: a pull request from a fork there.
pub fn checkout_pull_request(remote: &str, service: Service, repo: &RepoId, pr: &PullRequest, on_branch: bool) -> Option<GitCmd> {
    if service.has_pull_refs() {
        return Some(checkout_pull(remote, service, pr.number, on_branch));
    }
    // The name comes from the service: take it only if git would have
    // made such a branch, so it cannot turn into another refspec.
    let plain = |name: &str| {
        !name.is_empty()
            && !name.starts_with(['-', '/', '.'])
            && !name.ends_with(['/', '.'])
            && !name.contains("..")
            && !name.contains("@{")
            && !name.contains("//")
            && !name.ends_with(".lock")
            && name.chars().all(|c| !c.is_control() && !" :?*[\\^~".contains(c))
    };
    if !pr.is_from(repo) || !plain(&pr.head_ref) {
        return None;
    }
    let branch = pull_branch(service, pr.number);
    let head = format!("refs/heads/{}", pr.head_ref);
    let what = format!("{} {}", service.pull_word(), service.pull_number(pr.number));
    Some(if on_branch {
        GitCmd::new(format!("Update {what}"), &["fetch", "--no-tags", remote, &head])
            .step(&["merge", "--ff-only", "FETCH_HEAD"])
            .undoable(UndoKind::HeadMove)
            .done(format!("Updated {branch} to {what}"))
    } else {
        GitCmd::new(format!("Check out {what}"), &["fetch", "--no-tags", remote, &format!("{head}:{branch}")])
            .step(&["checkout", &branch])
            .undoable(UndoKind::Checkout)
            .done(format!("Checked out {what} as {branch}"))
    })
}

#[cfg(test)]
mod review_fork_tests;

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicU32;

    use super::github::{API, Client, new_pull_json, parse_issues, parse_pull, parse_pulls, parse_runs, run_state};
    use super::*;

    pub(super) const NOW: u64 = 1_800_000_000;
    /// Shaped like a real token; never a real one.
    pub(super) const SECRET: &str = "ghp_FAKEfakeFAKEfakeFAKEfake0123456789";
    pub(super) const OTHER: &str = "gho_ANOTHERfakeTOKENforTESTS9876543210";
    /// No self-hosted servers named.
    pub(super) static NO_SERVERS: Servers = BTreeMap::new();

    // ---- fakes ----

    /// One request as the transport saw it.
    pub(super) struct Sent {
        pub(super) url: String,
        pub(super) etag: Option<String>,
        pub(super) token: Option<String>,
        /// The JSON of a POST; `None` for a GET.
        pub(super) body: Option<String>,
    }

    pub(super) type Log = Arc<Mutex<Vec<Sent>>>;

    /// A transport that plays back scripted answers and records requests.
    pub(super) struct Script {
        pub(super) replies: VecDeque<Result<HttpResponse, String>>,
        pub(super) sent: Log,
    }

    impl Script {
        fn answer(&mut self, request: &HttpRequest<'_>, body: Option<&str>) -> Result<HttpResponse, String> {
            self.sent.lock().unwrap().push(Sent {
                url: request.url.to_owned(),
                etag: request.etag.map(str::to_owned),
                token: request.token.map(|token| token.0.clone()),
                body: body.map(str::to_owned),
            });
            self.replies.pop_front().unwrap_or_else(|| Err("script exhausted".to_owned()))
        }
    }

    impl Transport for Script {
        fn get(&mut self, request: &HttpRequest<'_>) -> Result<HttpResponse, String> {
            self.answer(request, None)
        }
        fn post(&mut self, request: &HttpRequest<'_>, json: &str) -> Result<HttpResponse, String> {
            self.answer(request, Some(json))
        }
    }

    pub(super) fn reply(status: u16, headers: &[(&str, &str)], body: &str) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status,
            headers: headers.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            body: body.to_owned(),
        })
    }

    /// Token sources with fixed contents that record what they are asked:
    /// variable names, `gh <host>` / `glab <host>` and `git <host>`. The
    /// tool and the helper answer `cli` and `helper` whatever the host, so
    /// the record is what shows which host a token was looked up for.
    #[derive(Clone, Default)]
    pub(super) struct Fake {
        pub(super) env: Vec<(&'static str, &'static str)>,
        pub(super) cli: Option<&'static str>,
        pub(super) helper: Option<&'static str>,
        pub(super) asked: Arc<Mutex<Vec<String>>>,
    }

    impl Fake {
        pub(super) fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Secrets for Fake {
        fn env(&self, name: &str) -> Option<String> {
            self.asked.lock().unwrap().push(name.to_owned());
            self.env.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned())
        }
        fn cli(&self, service: Service, host: &str) -> Option<String> {
            let tool = service.cli_tool().unwrap_or("none");
            self.asked.lock().unwrap().push(format!("{tool} {host}"));
            self.cli.map(str::to_owned)
        }
        fn credential_helper(&self, host: &str) -> Option<String> {
            self.asked.lock().unwrap().push(format!("git {host}"));
            self.helper.map(str::to_owned)
        }
    }

    fn repo() -> RepoId {
        RepoId {
            service: Service::GitHub,
            host: "github.com".to_owned(),
            owner: "octo".to_owned(),
            name: "app".to_owned(),
        }
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
            assert_eq!(parse_remote(url, &NO_SERVERS), Some(repo()), "{url}");
        }

        let dotted = parse_remote("git@github.com:rust-lang/rust.vim.git", &NO_SERVERS).unwrap();
        assert_eq!((dotted.owner.as_str(), dotted.name.as_str()), ("rust-lang", "rust.vim"));
        assert_eq!(dotted.slug(), "rust-lang/rust.vim");
    }

    #[test]
    fn other_hosts_and_malformed_urls_are_rejected() {
        for url in [
            "",
            "https://gitee.com/octo/app.git",
            "git@gitee.com:octo/app.git",
            "https://bitbucket.org/octo",
            "https://bitbucket.org/octo/app/pull-requests/12",
            "https://codeberg.org/octo/app/pulls",
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
            assert_eq!(parse_remote(url, &NO_SERVERS), None, "{url}");
        }
    }

    #[test]
    fn origin_is_preferred_and_other_remotes_are_a_fallback() {
        let fork = remote("fork", "git@github.com:ada/app.git");
        let origin = remote("origin", "https://github.com/octo/app.git");
        let found = detect(&[fork.clone(), origin], &NO_SERVERS).unwrap();
        assert_eq!((found.0.as_str(), found.1.slug().as_str()), ("origin", "octo/app"));

        // origin elsewhere: the first hosted remote stands in.
        let local = remote("origin", "E:/demo-origin.git");
        let found = detect(&[local.clone(), fork], &NO_SERVERS).unwrap();
        assert_eq!((found.0.as_str(), found.1.slug().as_str()), ("fork", "ada/app"));

        assert_eq!(detect(&[local, remote("sr", "https://gitee.com/octo/app.git")], &NO_SERVERS), None);
        assert_eq!(detect(&[], &NO_SERVERS), None);
    }

    #[test]
    fn links_are_built_only_from_validated_parts() {
        let repo = parse_remote("git@github.com:rust-lang/rust.vim.git", &NO_SERVERS).unwrap();
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
        let (token, source) = find_token(&all, &repo(), &[]).unwrap();
        assert_eq!((token.0.as_str(), source), (SECRET, TokenSource::Env("GITHUB_TOKEN")));
        assert_eq!(all.asked(), ["GITHUB_TOKEN"]);

        // GH_TOKEN next.
        let fake = Fake { env: vec![("GH_TOKEN", OTHER)], cli: Some("gh-cli-token-123456"), ..Default::default() };
        let (token, source) = find_token(&fake, &repo(), &[]).unwrap();
        assert_eq!((token.0.as_str(), source), (OTHER, TokenSource::Env("GH_TOKEN")));
        assert_eq!(fake.asked(), ["GITHUB_TOKEN", "GH_TOKEN"]);

        // Then the GitHub CLI, before git is ever started.
        let fake = Fake { cli: Some("gh-cli-token-123456\n"), helper: Some("helper-token-123456"), ..Default::default() };
        let (token, source) = find_token(&fake, &repo(), &[]).unwrap();
        assert_eq!((token.0.as_str(), source), ("gh-cli-token-123456", TokenSource::Cli));
        assert_eq!(fake.asked(), ["GITHUB_TOKEN", "GH_TOKEN", "gh github.com"]);

        // Then git's credential helper.
        let fake = Fake { helper: Some("helper-token-123456"), ..Default::default() };
        let (token, source) = find_token(&fake, &repo(), &[]).unwrap();
        assert_eq!((token.0.as_str(), source), ("helper-token-123456", TokenSource::CredentialHelper));
        assert_eq!(fake.asked(), ["GITHUB_TOKEN", "GH_TOKEN", "gh github.com", "git github.com"]);

        // Nothing anywhere: anonymous.
        let fake = Fake::default();
        assert!(find_token(&fake, &repo(), &[]).is_none());
        assert_eq!(fake.asked(), ["GITHUB_TOKEN", "GH_TOKEN", "gh github.com", "git github.com"]);
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
        let (token, source) = find_token(&fake, &repo(), &[]).unwrap();
        assert_eq!((token.0.as_str(), source), ("helper-token-123456", TokenSource::CredentialHelper));

        // A token the service already refused does not hide the next source.
        let stale = Token::new(SECRET).unwrap();
        let fake = Fake { env: vec![("GITHUB_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() };
        let (token, source) = find_token(&fake, &repo(), std::slice::from_ref(&stale)).unwrap();
        assert_eq!((token.0.as_str(), source), (OTHER, TokenSource::Cli));
        assert!(find_token(&fake, &repo(), &[stale, Token::new(OTHER).unwrap()]).is_none());

        assert!(Token::new("").is_none());
        assert!(Token::new("short").is_none());
        assert!(Token::new("has\ttab-inside-it").is_none());
        assert!(Token::new("caf\u{e9}-caf\u{e9}-caf\u{e9}").is_none());
        // Microsoft Entra tokens run to a few kilobytes; nothing is bigger.
        assert!(Token::new(&"x".repeat(4000)).is_some());
        assert!(Token::new(&"x".repeat(20_000)).is_none());
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
            assert!(!auth.describe(Service::GitHub).contains("ghp_"));
        }
    }

    // ---- the client ----

    #[test]
    fn requests_go_to_the_api_with_the_token_and_only_there() {
        assert!(within(API, "https://api.github.com/repos/octo/app/pulls"));
        for url in [
            "http://api.github.com/repos/octo/app/pulls",
            "https://api.github.com.evil.example/repos/octo/app/pulls",
            "https://api.github.com@evil.example/x",
            "https://api.github.com:8443/x",
            "https://api.github.com",
            "https://github.com/octo/app",
            "https://evil.example/https://api.github.com/",
        ] {
            assert!(!within(API, url), "{url}");
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
        assert!(sent.iter().all(|request| within(API, &request.url)));
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
            assert!(within(API, &sent[0].url));
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
            fn cli(&self, service: Service, host: &str) -> Option<String> {
                self.0.cli(service, host)
            }
            fn credential_helper(&self, host: &str) -> Option<String> {
                self.0.credential_helper(host)
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
                    client.auth().describe(Service::GitHub),
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

        let mut client = Client::new(repo(), Box::new(Canned { dir: dir.clone(), created: false }), Box::new(NoSecrets));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
        assert_eq!(failure(client.issues(NOW)), Problem::RateLimited { until: NOW + 120 });
        // No runs.json: the list does not exist.
        assert_eq!(failure(client.runs(NOW + 120)), Problem::NotSignedIn);
        assert_eq!(client.auth(), Auth::Anonymous);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- opening a pull request ----

    /// The API's answer to a creation, trimmed down.
    const CREATED: &str = r#"{"number": 42, "title": "Add dark mode", "draft": true, "state": "open",
       "html_url": "https://github.com/octo/app/pull/42", "user": {"login": "ada"},
       "head": {"ref": "feature/dark", "sha": "5555555555555555555555555555555555555555",
                "repo": {"full_name": "octo/app"}},
       "base": {"ref": "main", "sha": "0000000000000000000000000000000000000000",
                "repo": {"full_name": "octo/app"}},
       "updated_at": "1970-01-02T00:00:00Z"}"#;

    fn new_pull() -> NewPull {
        NewPull {
            head: "feature/dark".to_owned(),
            base: "main".to_owned(),
            title: "Add dark mode".to_owned(),
            body: "- Dark palette\n- \"Quoted\" & <escaped>".to_owned(),
            draft: true,
        }
    }

    fn validation(errors: &str) -> String {
        format!(r#"{{"message": "Validation Failed", "errors": [{errors}], "documentation_url": "https://docs.github.com"}}"#)
    }

    #[test]
    fn a_pull_request_is_opened_with_one_post_to_the_api() {
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], with_token());
        let pull = client.create_pull(&new_pull(), NOW).unwrap();
        assert_eq!((pull.number, pull.title.as_str(), pull.draft), (42, "Add dark mode", true));
        assert_eq!((pull.head_ref.as_str(), pull.base_ref.as_str()), ("feature/dark", "main"));
        assert!(pull.is_from(&repo()));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("GITHUB_TOKEN")));

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].url, "https://api.github.com/repos/octo/app/pulls");
        assert!(within(API, &sent[0].url));
        assert_eq!(sent[0].token.as_deref(), Some(SECRET));
        assert_eq!(sent[0].etag, None);
        // The body is JSON with exactly these fields, quotes and all intact.
        let body: serde_json::Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "title": "Add dark mode",
                "head": "feature/dark",
                "base": "main",
                "body": "- Dark palette\n- \"Quoted\" & <escaped>",
                "draft": true,
            })
        );
        assert!(!sent[0].body.as_deref().unwrap().contains(SECRET));
        assert_eq!(parse_pull(CREATED).unwrap(), pull);
        assert!(new_pull_json(&NewPull { draft: false, ..new_pull() }).contains("\"draft\":false"));
    }

    #[test]
    fn opening_a_pull_request_needs_a_token_and_says_how_to_get_one() {
        // Nothing anywhere: nothing is sent at all.
        let (mut client, sent) = scripted(vec![reply(201, &[], CREATED)], Fake::default());
        let problem = client.create_pull(&new_pull(), NOW).unwrap_err();
        assert_eq!(problem, Problem::NotSignedIn);
        assert!(sent.lock().unwrap().is_empty());
        let text = create_problem(&problem, Service::GitHub);
        assert!(text.contains("GITHUB_TOKEN") && text.contains("gh auth login"), "{text}");
        assert!(text.contains("Open on GitHub"), "{text}");

        // A sign-in made since the app started is found when asked again.
        let late = Arc::new(AtomicBool::new(false));
        struct Late(Arc<AtomicBool>);
        impl Secrets for Late {
            fn env(&self, name: &str) -> Option<String> {
                (self.0.load(Ordering::Relaxed) && name == "GITHUB_TOKEN").then(|| SECRET.to_owned())
            }
            fn cli(&self, _service: Service, _host: &str) -> Option<String> {
                None
            }
            fn credential_helper(&self, _host: &str) -> Option<String> {
                None
            }
        }
        let sent = Log::default();
        let transport = Script { replies: vec![reply(201, &[], CREATED)].into(), sent: sent.clone() };
        let mut client = Client::new(repo(), Box::new(transport), Box::new(Late(late.clone())));
        assert_eq!(client.create_pull(&new_pull(), NOW).unwrap_err(), Problem::NotSignedIn);
        late.store(true, Ordering::Relaxed);
        assert_eq!(client.create_pull(&new_pull(), NOW).unwrap().number, 42);
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_refused_token_gives_way_to_the_next_and_then_to_a_clear_refusal() {
        let bad = r#"{"message": "Bad credentials"}"#;
        // GITHUB_TOKEN is stale; the CLI's token works. A 401 did nothing,
        // so asking again with the other token is safe.
        let secrets = Fake { env: vec![("GITHUB_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() };
        let (mut client, sent) = scripted(vec![reply(401, &[], bad), reply(201, &[], CREATED)], secrets);
        assert_eq!(client.create_pull(&new_pull(), NOW).unwrap().number, 42);
        let tokens: Vec<Option<String>> = sent.lock().unwrap().iter().map(|sent| sent.token.clone()).collect();
        assert_eq!(tokens, [Some(SECRET.to_owned()), Some(OTHER.to_owned())]);

        // The only token refused: no anonymous attempt follows.
        let (mut client, sent) = scripted(vec![reply(401, &[], bad), reply(201, &[], CREATED)], with_token());
        let problem = client.create_pull(&new_pull(), NOW).unwrap_err();
        assert_eq!(problem, Problem::SignInRejected);
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert!(create_problem(&problem, Service::GitHub).contains("refused the saved sign-in"));
    }

    #[test]
    fn creation_errors_are_put_in_plain_words() {
        let line = |replies: Vec<Result<HttpResponse, String>>| {
            let (mut client, _) = scripted(replies, with_token());
            let problem = client.create_pull(&new_pull(), NOW).unwrap_err();
            (problem.clone(), create_problem(&problem, Service::GitHub))
        };
        let refused = |status: u16, body: &str| line(vec![reply(status, &[], body)]).1;

        let exists = validation(
            r#"{"resource": "PullRequest", "code": "custom", "message": "A pull request already exists for octo:feature/dark."}"#,
        );
        assert_eq!(refused(422, &exists), "GitHub refused: A pull request from feature/dark into main is already open");
        let empty = validation(
            r#"{"resource": "PullRequest", "code": "custom", "message": "No commits between main and feature/dark"}"#,
        );
        assert_eq!(
            refused(422, &empty),
            "GitHub refused: No commits between main and feature/dark: there is nothing to merge"
        );
        let head = validation(r#"{"resource": "PullRequest", "field": "head", "code": "invalid"}"#);
        assert_eq!(refused(422, &head), "GitHub refused: GitHub does not have the branch feature/dark: push it first");
        let base = validation(r#"{"resource": "PullRequest", "field": "base", "code": "invalid"}"#);
        assert_eq!(refused(422, &base), "GitHub refused: GitHub does not have the branch main to merge into");
        let title = validation(r#"{"resource": "PullRequest", "field": "title", "code": "missing_field"}"#);
        assert_eq!(refused(422, &title), "GitHub refused: The title is missing");
        let draft = r#"{"message": "Draft pull requests are not supported in this repository."}"#;
        assert_eq!(
            refused(422, draft),
            "GitHub refused: Draft pull requests are not available in this repository: untick Draft"
        );
        // Something new: GitHub's own words.
        let other = validation(r#"{"resource": "PullRequest", "field": "maintainer_can_modify", "code": "invalid"}"#);
        assert_eq!(refused(422, &other), "GitHub refused: Validation Failed (maintainer_can_modify is invalid)");
        assert_eq!(refused(422, "not json"), "GitHub refused: Validation failed");

        // Permission.
        let forbidden = r#"{"message": "Resource not accessible by personal access token"}"#;
        assert_eq!(
            refused(403, forbidden),
            "GitHub refused: No permission to open pull requests in octo/app (Resource not accessible by personal access token)"
        );
        assert_eq!(refused(403, ""), "GitHub refused: No permission to open pull requests in octo/app");
        let (problem, text) = line(vec![reply(404, &[], r#"{"message": "Not Found"}"#)]);
        assert_eq!(problem, Problem::NoAccess);
        assert!(text.contains("not allowed to open pull requests"), "{text}");

        // Slow down, and when to try again.
        let secondary = r#"{"message": "You have exceeded a secondary rate limit"}"#;
        let (problem, text) = line(vec![reply(403, &[("retry-after", "120")], secondary)]);
        assert_eq!(problem, Problem::RateLimited { until: NOW + 120 });
        assert_eq!(text, format!("GitHub asked us to slow down. Try again after {}.", clock(NOW + 120)));

        // The service or the network failing.
        assert_eq!(refused(502, "<html>"), "GitHub answered 502");
        let (problem, _) = line(vec![reply(201, &[], "{ not json")]);
        assert!(matches!(problem, Problem::Failed(text) if text.starts_with("Unreadable answer")));
        let (problem, text) = line(vec![Err("timed out".to_owned())]);
        assert_eq!(problem, Problem::Unreachable("timed out".to_owned()));
        assert_eq!(text, "GitHub unreachable: timed out");
    }

    #[test]
    fn a_creation_is_never_sent_twice_or_anywhere_but_the_api() {
        // No answer: the pull request may exist now, so no second attempt.
        let (mut client, sent) = scripted(vec![Err("connection reset".to_owned()), reply(201, &[], CREATED)], with_token());
        assert!(client.create_pull(&new_pull(), NOW).is_err());
        assert_eq!(sent.lock().unwrap().len(), 1);

        // A renamed repository: 307 and 308 keep the POST, within the API.
        let moved = "https://api.github.com/repositories/42/pulls";
        let (mut client, sent) =
            scripted(vec![reply(307, &[("location", moved)], ""), reply(201, &[], CREATED)], with_token());
        assert_eq!(client.create_pull(&new_pull(), NOW).unwrap().number, 42);
        {
            let sent = sent.lock().unwrap();
            assert_eq!(sent[1].url, moved);
            assert_eq!(sent[1].body, sent[0].body);
        }

        // Elsewhere, or a redirect that would turn it into a GET: not followed.
        for (status, target) in [
            (307, "https://evil.example/collect"),
            (308, "http://api.github.com/repositories/42/pulls"),
            (301, moved),
            (302, moved),
            (303, moved),
        ] {
            let (mut client, sent) =
                scripted(vec![reply(status, &[("location", target)], ""), reply(201, &[], CREATED)], with_token());
            let problem = client.create_pull(&new_pull(), NOW).unwrap_err();
            assert_eq!(problem, Problem::Failed("GitHub sent an unexpected redirect".to_owned()), "{status} {target}");
            assert_eq!(sent.lock().unwrap().len(), 1, "{status} {target}");
        }

        // Nor is an endless chain.
        let replies = (0..10).map(|_| reply(307, &[("location", moved)], "")).collect();
        let (mut client, sent) = scripted(replies, with_token());
        assert!(client.create_pull(&new_pull(), NOW).is_err());
        assert_eq!(sent.lock().unwrap().len(), 4);

        // A rate limit already known holds the request back entirely.
        let reset = (NOW + 600).to_string();
        let spent = [("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", reset.as_str())];
        let (mut client, sent) = scripted(vec![reply(200, &spent, PULLS), reply(201, &[], CREATED)], with_token());
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(client.create_pull(&new_pull(), NOW + 1).unwrap_err(), Problem::RateLimited { until: NOW + 600 });
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_token_never_reaches_a_creation_error() {
        let echo = format!(r#"{{"message": "Bad token {SECRET} for Bearer {SECRET}"}}"#);
        let failing = format!(
            r#"{{"message": "Validation Failed", "errors": [{{"field": "x", "code": "c", "message": "saw {SECRET}"}}]}}"#
        );
        let cases: Vec<Vec<Result<HttpResponse, String>>> = vec![
            vec![Err(format!("tls: failed sending Authorization: Bearer {SECRET}"))],
            vec![reply(403, &[], &echo)],
            vec![reply(422, &[], &echo)],
            vec![reply(422, &[], &failing)],
            vec![reply(500, &[], &echo)],
            vec![reply(201, &[], &echo)],
            vec![reply(201, &[], SECRET)],
            vec![reply(307, &[("location", format!("https://evil.example/{SECRET}").as_str())], "")],
            vec![reply(401, &[], &echo), reply(422, &[], &failing)],
        ];
        for (index, replies) in cases.into_iter().enumerate() {
            let secrets = Fake { env: vec![("GITHUB_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() };
            let (mut client, _) = scripted(replies, secrets);
            let problem = client.create_pull(&new_pull(), NOW).unwrap_err();
            for text in [
                create_problem(&problem, Service::GitHub),
                problem.line(Service::GitHub),
                problem.detail(Service::GitHub),
                format!("{problem:?}"),
            ] {
                assert!(!text.contains(SECRET) && !text.contains("FAKEfake"), "case {index}: {text}");
                assert!(!text.contains(OTHER), "case {index}: {text}");
            }
        }
        // Scrubbed, not dropped: the rest of the message is still there.
        let (mut client, _) = scripted(vec![reply(422, &[], &failing)], with_token());
        assert_eq!(
            client.create_pull(&new_pull(), NOW).unwrap_err(),
            Problem::Refused("Validation Failed (saw ***)".to_owned())
        );
    }

    #[test]
    fn the_compare_page_link_escapes_branch_names() {
        let repo = repo();
        assert_eq!(
            repo.compare_url("main", "feature/dark-mode_2.0"),
            "https://github.com/octo/app/compare/main...feature/dark-mode_2.0?expand=1"
        );
        // Everything a shell or a URL could read as syntax is encoded.
        let link = repo.compare_url("release&calc", "ada/x%y#z+w?q=1 \"é\"|^!");
        assert_eq!(
            link,
            "https://github.com/octo/app/compare/release%26calc...ada/x%25y%23z%2Bw%3Fq%3D1%20%22%C3%A9%22%7C%5E%21?expand=1"
        );
        for link in [link, repo.compare_url("a", "b")] {
            assert!(link.starts_with("https://github.com/octo/app/compare/"), "{link}");
            assert!(link.bytes().all(|b| b.is_ascii_alphanumeric() || b"/:._-%?=".contains(&b)), "{link}");
            assert_eq!(link.matches('?').count(), 1, "{link}");
            // Each % starts an escape.
            for (at, _) in link.match_indices('%') {
                assert!(link[at + 1..].len() >= 2 && link[at + 1..at + 3].bytes().all(|b| b.is_ascii_hexdigit()));
            }
        }
    }

    #[test]
    fn the_offline_fixture_answers_a_creation_too() {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-canned-create-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("create.json"), CREATED).unwrap();

        std::fs::write(dir.join("pulls.json"), PULLS).unwrap();
        std::fs::write(dir.join("pulls.after.json"), format!("[{CREATED}]")).unwrap();

        let canned = Canned { dir: dir.clone(), created: false };
        let mut client = Client::new(repo(), Box::new(canned), Box::new(FixtureSignIn));
        let numbers = |outcome: Outcome<PullRequest>| match outcome {
            Outcome::Fresh { items, .. } => items.iter().map(|pull| pull.number).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert_eq!(numbers(client.pulls(NOW)), [12, 9]);
        assert_eq!(client.create_pull(&new_pull(), NOW).unwrap().number, 42);
        // The list the fixture serves from then on has it.
        assert_eq!(numbers(client.pulls(NOW)), [42]);
        let sent: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("create.sent.json")).unwrap()).unwrap();
        assert_eq!(sent["title"], "Add dark mode");
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Cli));

        // A staged refusal.
        std::fs::write(dir.join("create.head"), "422\n").unwrap();
        let exists = validation(r#"{"message": "A pull request already exists for octo:feature/dark."}"#);
        std::fs::write(dir.join("create.json"), exists).unwrap();
        let problem = client.create_pull(&new_pull(), NOW).unwrap_err();
        assert!(create_problem(&problem, Service::GitHub).contains("already open"));

        // Without the sign-in file the fixture has no token, as before.
        let mut anonymous = Client::new(repo(), Box::new(Canned { dir: dir.clone(), created: false }), Box::new(NoSecrets));
        assert_eq!(anonymous.create_pull(&new_pull(), NOW).unwrap_err(), Problem::NotSignedIn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_creation_goes_through_the_worker_and_the_list_follows() {
        let stub = Stub::default();
        let mut state = HostingState::default();
        {
            let stub = stub.clone();
            state.sync_with(&hosted(), &NO_SERVERS, true, true, move |_| Box::new(stub), || {});
        }
        settle(&mut state, &hosted());
        assert_eq!(stub.rounds.load(Ordering::SeqCst), 1);
        assert!(state.connected().is_some_and(|shown| shown.remote == "origin"));

        let id = state.create_pull(new_pull()).expect("connected");
        assert!(state.is_creating(id));
        let deadline = Instant::now() + Duration::from_secs(20);
        let answer = loop {
            state.poll();
            if let Some(answer) = state.take_created(id) {
                break answer;
            }
            assert!(Instant::now() < deadline, "no answer");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(answer.unwrap().number, 42);
        assert!(!state.is_creating(id));
        assert_eq!(stub.created.lock().unwrap().as_slice(), [new_pull()]);
        // Collected once only.
        assert!(state.take_created(id).is_none());

        // The list shows it straight away and is refreshed to be sure.
        let shown = state.connected().unwrap();
        assert_eq!(shown.view.pulls.items.first().map(|pull| pull.number), Some(42));
        let shown = settle(&mut state, &hosted());
        assert_eq!(stub.rounds.load(Ordering::SeqCst), 2);
        assert!(!shown.view.busy);

        // A failure comes back as it is, with no refresh.
        stub.fail.store(true, Ordering::SeqCst);
        let id = state.create_pull(new_pull()).unwrap();
        let answer = loop {
            state.poll();
            if let Some(answer) = state.take_created(id) {
                break answer;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(answer.unwrap_err(), Problem::Unreachable("no connection".to_owned()));

        // Nothing running: nothing to ask.
        assert!(HostingState::default().create_pull(new_pull()).is_none());
    }

    // ---- the background thread and the per-tab state ----

    /// A backend with fixed answers that counts what it is asked.
    #[derive(Clone, Default)]
    struct Stub {
        rounds: Arc<AtomicU32>,
        looked_again: Arc<AtomicU32>,
        fail: Arc<AtomicBool>,
        created: Arc<Mutex<Vec<NewPull>>>,
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
        fn create_pull(&mut self, new: &NewPull, _now: u64) -> Result<PullRequest, Problem> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(Problem::Unreachable("no connection".to_owned()));
            }
            self.created.lock().unwrap().push(new.clone());
            Ok(parse_pull(CREATED).unwrap())
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
            let shown = state.sync_with(remotes, &NO_SERVERS, true, false, connect, || {}).expect("still hosted");
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
        assert!(state.sync_with(&hosted(), &NO_SERVERS, false, true, never, || {}).is_none());
        assert!(state.session.is_none());
        // On, but the remotes are elsewhere.
        let elsewhere = [remote("origin", "E:/demo-origin.git"), remote("sr", "https://gitee.com/octo/app.git")];
        assert!(state.sync_with(&elsewhere, &NO_SERVERS, true, true, never, || {}).is_none());
        assert!(state.sync_with(&[], &NO_SERVERS, true, true, never, || {}).is_none());
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
            state.sync_with(&hosted(), &NO_SERVERS, true, false, move |_| Box::new(stub), wake).unwrap()
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
            state.sync_with(&hosted(), &NO_SERVERS, true, true, move |_| Box::new(stub), || {});
        }
        settle(&mut state, &hosted());
        let rounds = || stub.rounds.load(Ordering::SeqCst);
        assert_eq!(rounds(), 1);

        // Not due yet: syncing every frame asks for nothing.
        let connect = |_: &RepoId| -> Box<dyn Backend> { panic!("already connected") };
        for _ in 0..50 {
            assert!(!state.sync_with(&hosted(), &NO_SERVERS, true, true, connect, || {}).unwrap().view.busy);
        }
        assert_eq!(rounds(), 1);

        // Due, but nobody is looking at the window: still nothing.
        state.session.as_mut().unwrap().next_auto = Some(Instant::now());
        assert!(!state.sync_with(&hosted(), &NO_SERVERS, true, false, connect, || {}).unwrap().view.busy);
        // Due and attended: one automatic round, without a new sign-in search.
        assert!(state.sync_with(&hosted(), &NO_SERVERS, true, true, connect, || {}).unwrap().view.busy);
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
            state.sync_with(&hosted(), &NO_SERVERS, true, true, move |_| Box::new(stub), || {});
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
        /// Dropped with the worker thread that owns it. With a gate, the
        /// first request waits until the gate's sender is dropped, so the test
        /// can look at a session before its thread has answered anything.
        struct Tracked(Stub, #[allow(dead_code)] Arc<()>, Option<Receiver<()>>);
        impl Backend for Tracked {
            fn pulls(&mut self, now: u64) -> Outcome<PullRequest> {
                if let Some(gate) = self.2.take() {
                    let _ = gate.recv();
                }
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
            state.sync_with(&hosted(), &NO_SERVERS, true, true, move |_| Box::new(Tracked(Stub::default(), alive, None)), || {});
        }
        settle(&mut state, &hosted());

        // The remote now names another repository: a fresh session for it,
        // with none of the old repository's data.
        let moved = [remote("origin", "https://github.com/octo/other.git")];
        let second = Arc::new(());
        let (open_gate, gate) = channel::<()>();
        let shown = {
            let second = second.clone();
            let connect = move |_: &RepoId| -> Box<dyn Backend> { Box::new(Tracked(Stub::default(), second, Some(gate))) };
            state.sync_with(&moved, &NO_SERVERS, true, true, connect, || {}).unwrap()
        };
        assert_eq!(shown.repo.slug(), "octo/other");
        assert!(shown.view.pulls.items.is_empty() && !shown.view.pulls.loaded);
        drop(open_gate);
        wait_until_alone(&alive);

        // Switched off: the session and its thread go away.
        let never = |_: &RepoId| -> Box<dyn Backend> { panic!("must not connect") };
        assert!(state.sync_with(&moved, &NO_SERVERS, false, true, never, || {}).is_none());
        assert!(state.session.is_none());
        wait_until_alone(&second);
    }

    #[test]
    fn a_renamed_remote_keeps_its_session_and_a_removed_one_ends_it() {
        let stub = Stub::default();
        let mut state = HostingState::default();
        {
            let stub = stub.clone();
            state.sync_with(&hosted(), &NO_SERVERS, true, true, move |_| Box::new(stub), || {});
        }
        let before = settle(&mut state, &hosted());

        // Renamed (`git remote rename origin upstream`): the same repository,
        // so the lists stay and nothing is asked again; the name follows.
        let renamed = [remote("upstream", "git@github.com:octo/app.git")];
        let shown = settle(&mut state, &renamed);
        assert_eq!(shown.remote, "upstream");
        assert!(shown.view.pulls.loaded);
        assert_eq!(shown.view.pulls.items.len(), before.view.pulls.items.len());
        assert_eq!(stub.rounds.load(Ordering::SeqCst), 1);
        assert_eq!(state.connected().map(|shown| shown.remote).as_deref(), Some("upstream"));

        // Removed: no lists, no session.
        let never = |_: &RepoId| -> Box<dyn Backend> { panic!("must not connect") };
        assert!(state.sync_with(&[], &NO_SERVERS, true, true, never, || {}).is_none());
        assert!(state.session.is_none() && state.connected().is_none());
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
        state.sync_with(&hosted(), &NO_SERVERS, true, true, |_| Box::new(Broken), || {});
        let shown = settle(&mut state, &hosted());
        let stopped = Some(Problem::Failed("The background worker stopped".to_owned()));
        assert_eq!(shown.view.pulls.problem, stopped);
        assert_eq!(shown.view.runs.problem, stopped);
        // Asking again does not hang or panic either.
        state.refresh();
        assert!(!settle(&mut state, &hosted()).view.busy);
    }

    #[test]
    fn a_disconnected_account_is_never_signed_in_to() {
        // Whatever sign-in is around, the backend of a disconnected host
        // sends no token and reports itself signed out.
        let sent = Log::default();
        let transport = Script { replies: vec![reply(200, &[], PULLS)].into(), sent: sent.clone() };
        let mut client = Client::new(repo(), Box::new(transport), signed_out_secrets(true, Box::new(with_token())));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(client.auth(), Auth::Anonymous);
        assert_eq!(sent.lock().unwrap()[0].token, None);

        // Connected, the same sign-in is used.
        let transport = Script { replies: vec![reply(200, &[], PULLS)].into(), sent: sent.clone() };
        let mut client = Client::new(repo(), Box::new(transport), signed_out_secrets(false, Box::new(with_token())));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { .. }));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("GITHUB_TOKEN")));
        assert_eq!(sent.lock().unwrap()[1].token.as_deref(), Some(SECRET));
    }

    #[test]
    fn disconnecting_or_connecting_an_account_starts_the_session_over() {
        let mut state = HostingState::default();
        let mut out = Disconnected::new();
        out.insert("github.com".to_owned());
        let starts = Arc::new(Mutex::new(Vec::new()));
        let connect = |starts: &Arc<Mutex<Vec<bool>>>| {
            let starts = starts.clone();
            move |_: &RepoId, signed_out: bool| -> Box<dyn Backend> {
                starts.lock().unwrap().push(signed_out);
                Box::new(Stub::default())
            }
        };

        // Disconnected: started signed out, and shown as such.
        let shown = state.sync_accounts(&hosted(), &NO_SERVERS, &out, true, false, connect(&starts), || {}).unwrap();
        assert!(shown.disconnected);
        assert!(state.connected().unwrap().disconnected);
        // Nothing changed: the same session goes on.
        state.sync_accounts(&hosted(), &NO_SERVERS, &out, true, false, connect(&starts), || {});
        assert_eq!(*starts.lock().unwrap(), [true]);

        // Connected again: a new session that may sign in.
        out.clear();
        let shown = state.sync_accounts(&hosted(), &NO_SERVERS, &out, true, false, connect(&starts), || {}).unwrap();
        assert!(!shown.disconnected);
        assert_eq!(*starts.lock().unwrap(), [true, false]);

        // Another host's account changes nothing here.
        out.insert("gitlab.com".to_owned());
        state.sync_accounts(&hosted(), &NO_SERVERS, &out, true, false, connect(&starts), || {});
        assert_eq!(*starts.lock().unwrap(), [true, false]);
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

    // ---- more services: GitLab, GitHub Enterprise, self-hosted servers ----

    fn servers(entries: &[(&str, ServerKind)]) -> Servers {
        entries.iter().map(|(host, kind)| ((*host).to_owned(), *kind)).collect()
    }

    fn at(service: Service, host: &str, owner: &str, name: &str) -> RepoId {
        RepoId { service, host: host.to_owned(), owner: owner.to_owned(), name: name.to_owned() }
    }

    #[test]
    fn gitlab_com_remotes_are_recognised_with_nested_groups() {
        let plain = at(Service::GitLab, "gitlab.com", "group", "app");
        for url in [
            "https://gitlab.com/group/app",
            "https://gitlab.com/group/app.git",
            "https://gitlab.com/group/app.git/",
            "https://www.gitlab.com/group/app",
            "https://GitLab.com/group/app.git",
            "https://ada:glpat-hunter2@gitlab.com/group/app.git",
            "https://gitlab.com:443/group/app.git",
            "git@gitlab.com:group/app.git",
            "gitlab.com:group/app",
            "ssh://git@gitlab.com/group/app.git",
            "ssh://git@altssh.gitlab.com:443/group/app.git",
            "git+ssh://git@gitlab.com/group/app.git",
        ] {
            assert_eq!(parse_remote(url, &NO_SERVERS), Some(plain.clone()), "{url}");
        }

        let nested = parse_remote("git@gitlab.com:group/sub/deeper/my.app.git", &NO_SERVERS).unwrap();
        assert_eq!(nested, at(Service::GitLab, "gitlab.com", "group/sub/deeper", "my.app"));
        assert_eq!(nested.slug(), "group/sub/deeper/my.app");
        assert_eq!(gitlab::project_id(&nested), "group%2Fsub%2Fdeeper%2Fmy.app");
        // Names keep their case; the host is lower-cased.
        let cased = parse_remote("https://GITLAB.COM/Group/App.git", &NO_SERVERS).unwrap();
        assert_eq!((cased.host.as_str(), cased.slug().as_str()), ("gitlab.com", "Group/App"));

        for url in [
            "https://gitlab.com/group",
            "https://gitlab.com/group/",
            "https://gitlab.com/group/app/-/merge_requests",
            "https://gitlab.com/group/-app",
            "https://gitlab.com/group//app",
            "https://gitlab.com/group/../app",
            "https://gitlab.com/group/app b",
            "https://gitlab.com/group/app?x=1",
            "https://gitlab.com/group/app#readme",
            "https://gitlab.com.evil.example/group/app.git",
            "https://evilgitlab.com/group/app.git",
            "https://gitlab.co/group/app.git",
            "https://gitlab.com@evil.example/group/app.git",
            "https://evil.example/gitlab.com/group/app",
            "git@gitlab.com.evil.example:group/app.git",
            "https://registry.gitlab.com/group/app",
            "file:///srv/gitlab.com/group/app.git",
            "C:/repos/gitlab.com/group/app",
        ] {
            assert_eq!(parse_remote(url, &NO_SERVERS), None, "{url}");
        }
    }

    #[test]
    fn a_self_hosted_server_is_used_only_once_the_user_has_named_it() {
        let urls = [
            "https://git.acme.com/team/app.git",
            "git@git.acme.com:team/app.git",
            "ssh://git@git.acme.com/team/app.git",
            // SSH's port says nothing about the web server.
            "ssh://git@git.acme.com:2222/team/app.git",
            "https://GIT.ACME.COM/team/app",
            "https://git.acme.com:443/team/app.git",
            // Plain HTTP's port is not where HTTPS listens either.
            "http://git.acme.com:8080/team/app.git",
        ];
        for url in urls {
            assert_eq!(parse_remote(url, &NO_SERVERS), None, "{url}");
        }

        let lab = servers(&[("git.acme.com", ServerKind::GitLab)]);
        for url in urls {
            assert_eq!(parse_remote(url, &lab), Some(at(Service::GitLab, "git.acme.com", "team", "app")), "{url}");
        }
        assert_eq!(
            parse_remote("git@git.acme.com:team/backend/api.git", &lab),
            Some(at(Service::GitLab, "git.acme.com", "team/backend", "api"))
        );

        // An HTTPS port is part of where the server is.
        let ported = "https://git.acme.com:8443/team/app.git";
        assert_eq!(parse_remote(ported, &lab), None);
        let lab_on_port = servers(&[("git.acme.com:8443", ServerKind::GitLab)]);
        let repo = parse_remote(ported, &lab_on_port).unwrap();
        assert_eq!(repo.host, "git.acme.com:8443");
        assert_eq!(repo.api_base(), "https://git.acme.com:8443/api/v4");
        assert_eq!(parse_remote("git@git.acme.com:team/app.git", &lab_on_port), None);

        // GitHub Enterprise: exactly owner/name.
        let ghe = servers(&[("ghe.acme.com", ServerKind::GitHubEnterprise)]);
        let repo = parse_remote("git@ghe.acme.com:octo/app.git", &ghe).unwrap();
        assert_eq!(repo, at(Service::GitHubEnterprise, "ghe.acme.com", "octo", "app"));
        assert_eq!(repo.api_base(), "https://ghe.acme.com/api/v3");
        assert_eq!(parse_remote("https://ghe.acme.com/octo/app/pulls", &ghe), None);
        let cloud = servers(&[("acme.ghe.com", ServerKind::GitHubEnterprise)]);
        assert_eq!(parse_remote("https://acme.ghe.com/octo/app", &cloud).unwrap().api_base(), "https://api.acme.ghe.com");

        // "Neither", or a kind this version does not know: never contacted.
        let ignored = servers(&[("git.acme.com", ServerKind::Ignored)]);
        assert_eq!(parse_remote("https://git.acme.com/team/app.git", &ignored), None);

        // Hosts that only look like the one named.
        for url in [
            "https://git.acme.com.evil.example/team/app.git",
            "https://evil-git.acme.com/team/app.git",
            "https://xgit.acme.com/team/app.git",
            "https://acme.com/team/app.git",
            "https://git.acme.co/team/app.git",
            "https://git.acme.com@evil.example/team/app.git",
            "https://evil.example/git.acme.com/team/app.git",
            "git@git.acme.com.evil.example:team/app.git",
        ] {
            assert_eq!(parse_remote(url, &lab), None, "{url}");
        }

        // A saved choice cannot turn github.com or gitlab.com into
        // something else.
        let confused = servers(&[("github.com", ServerKind::GitLab), ("gitlab.com", ServerKind::GitHubEnterprise)]);
        assert_eq!(parse_remote("https://github.com/octo/app", &confused).unwrap().service, Service::GitHub);
        assert_eq!(parse_remote("https://gitlab.com/group/app", &confused).unwrap().service, Service::GitLab);
    }

    #[test]
    fn addresses_cannot_carry_anything_but_a_host_and_a_path() {
        for url in [
            "https://git.acme.com\nhost=evil.example/team/app",
            "https://git.acme.com%0a/team/app",
            "https://git.acme.com\r/team/app",
            "https://[::1]:8443/team/app",
            "https://-git.acme.com/team/app",
            "https://git..acme.com/team/app",
            "https://git.acme.com:99999/team/app",
            "https://git.acme.com:port/team/app",
            "https://git_acme.com/team/app",
            "https://git.acme.com/team\\app/x",
            "x:team/app",
        ] {
            assert_eq!(address(url), None, "{url:?}");
        }
        // What does parse is made of safe characters only.
        let address = address("ssh://Git@Git.Acme.Com:22/Team/App.git").unwrap();
        assert_eq!((address.host(), address.path.join("/")), ("git.acme.com".to_owned(), "Team/App".to_owned()));
        assert_eq!(credential_query("git.acme.com:8443"), "protocol=https\nhost=git.acme.com:8443\n\n");
    }

    #[test]
    fn links_for_every_service_are_built_only_from_validated_parts() {
        let lab = servers(&[("git.acme.com:8443", ServerKind::GitLab), ("ghe.acme.com", ServerKind::GitHubEnterprise)]);
        let nested = parse_remote("git@gitlab.com:group/sub/app.git", &lab).unwrap();
        assert_eq!(nested.pulls_url(), "https://gitlab.com/group/sub/app/-/merge_requests");
        assert_eq!(nested.pull_url(7), "https://gitlab.com/group/sub/app/-/merge_requests/7");
        assert_eq!(nested.issues_url(), "https://gitlab.com/group/sub/app/-/issues");
        assert_eq!(nested.issue_url(3), "https://gitlab.com/group/sub/app/-/issues/3");
        assert_eq!(nested.runs_url(), "https://gitlab.com/group/sub/app/-/pipelines");
        assert_eq!(nested.run_url(99), "https://gitlab.com/group/sub/app/-/pipelines/99");
        assert_eq!(nested.api_base(), "https://gitlab.com/api/v4");

        let ported = parse_remote("https://git.acme.com:8443/team/app.git", &lab).unwrap();
        assert_eq!(ported.pull_url(1), "https://git.acme.com:8443/team/app/-/merge_requests/1");
        let ghe = parse_remote("https://ghe.acme.com/octo/app", &lab).unwrap();
        assert_eq!(ghe.pull_url(12), "https://ghe.acme.com/octo/app/pull/12");
        assert_eq!(ghe.run_url(5), "https://ghe.acme.com/octo/app/actions/runs/5");

        for repo in [nested, ported, ghe] {
            for link in [
                repo.pulls_url(),
                repo.pull_url(12),
                repo.issues_url(),
                repo.issue_url(31),
                repo.runs_url(),
                repo.run_url(9001),
                repo.api_base(),
            ] {
                assert!(link.starts_with("https://"), "{link}");
                assert!(link.bytes().all(|b| b.is_ascii_alphanumeric() || b"/:._-".contains(&b)), "{link}");
            }
        }
    }

    #[test]
    fn unknown_servers_are_offered_and_nothing_else_is() {
        let none = &NO_SERVERS;
        // origin on a server nobody has named: offered, with a guess.
        let remotes = [remote("origin", "git@gitlab.acme.com:team/app.git")];
        let offers = offers(&remotes, none);
        assert_eq!(
            offers,
            [Offer {
                host: "gitlab.acme.com".to_owned(),
                path: "team/app".to_owned(),
                remote: "origin".to_owned(),
                guess: Some(ServerKind::GitLab),
            }]
        );
        // And nothing is hosted until the user answers.
        assert_eq!(detect(&remotes, none), None);

        // Once answered, whatever the answer, it is no longer offered.
        for kind in [ServerKind::GitLab, ServerKind::GitHubEnterprise, ServerKind::Ignored] {
            assert!(super::offers(&remotes, &servers(&[("gitlab.acme.com", kind)])).is_empty());
        }

        // origin unknown while a fork is on github.com: the fork is shown,
        // and origin's server is still offered (it would take precedence).
        let mixed = [remote("fork", "git@github.com:ada/app.git"), remote("origin", "https://code.acme.com/team/app")];
        assert_eq!(detect(&mixed, none).unwrap().0, "fork");
        let offered = super::offers(&mixed, none);
        assert_eq!(offered.iter().map(|o| (o.host.as_str(), o.guess)).collect::<Vec<_>>(), [("code.acme.com", None)]);

        // origin hosted: a mirror elsewhere is not worth a row.
        let mirrored = [remote("origin", "git@github.com:octo/app.git"), remote("mirror", "git@code.acme.com:octo/app.git")];
        assert!(super::offers(&mirrored, none).is_empty());

        // Nothing hosted: every unknown server, once each.
        let several = [
            remote("origin", "E:/demo-origin.git"),
            remote("a", "git@code.acme.com:team/app.git"),
            remote("b", "https://code.acme.com/team/app.git"),
            remote("c", "https://ghe.example.org/team/app.git"),
        ];
        let hosts: Vec<String> = super::offers(&several, none).into_iter().map(|o| o.host).collect();
        assert_eq!(hosts, ["code.acme.com", "ghe.example.org"]);

        // Public services, services we cannot show, gists, local paths: no offer.
        for url in [
            "git@bitbucket.org:team/app.git",
            "https://codeberg.org/team/app",
            "https://dev.azure.com/org/project/_git/app",
            "https://acme.visualstudio.com/project/_git/app",
            "https://gitee.com/team/app.git",
            "https://git.sr.ht/team/app",
            "https://gist.github.com/ada/0123abcd",
            "E:/demo-origin.git",
            "/srv/git/app.git",
            "file:///srv/git/app.git",
        ] {
            assert!(super::offers(&[remote("origin", url)], none).is_empty(), "{url}");
        }

        // Guesses never decide anything; they only pre-select.
        for (host, expected) in [
            ("gitlab.acme.com", Some(ServerKind::GitLab)),
            ("gitlab-ce.acme.com", Some(ServerKind::GitLab)),
            ("src.gitlab.acme.org", Some(ServerKind::GitLab)),
            ("github.acme.com", Some(ServerKind::GitHubEnterprise)),
            ("ghe.acme.com", Some(ServerKind::GitHubEnterprise)),
            ("acme.ghe.com", Some(ServerKind::GitHubEnterprise)),
            ("code.acme.com", None),
            ("notgitlab.acme.com", None),
        ] {
            assert_eq!(guess(host), expected, "{host}");
        }
    }

    #[test]
    fn server_choices_are_kept_in_the_settings_and_survive_unknown_entries() {
        let saved = servers(&[
            ("git.acme.com", ServerKind::GitLab),
            ("ghe.acme.com", ServerKind::GitHubEnterprise),
            ("old.acme.com", ServerKind::Ignored),
        ]);
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(json, r#"{"ghe.acme.com":"github-enterprise","git.acme.com":"gitlab","old.acme.com":"ignored"}"#);
        assert_eq!(serde_json::from_str::<Servers>(&json).unwrap(), saved);

        // A kind from a later version reads as "do not contact".
        let later: Servers = serde_json::from_str(r#"{"git.acme.com": "phabricator"}"#).unwrap();
        assert_eq!(later["git.acme.com"], ServerKind::Ignored);

        // Settings written before this existed still load, with no servers.
        let settings: crate::state::Settings = serde_json::from_str(r#"{"tree_view": true}"#).unwrap();
        assert!(settings.tree_view && settings.hosting_servers.is_empty());
        let settings: crate::state::Settings =
            serde_json::from_str(r#"{"hosting_servers": {"git.acme.com": "gitlab"}}"#).unwrap();
        assert_eq!(settings.hosting_servers["git.acme.com"], ServerKind::GitLab);
    }

    #[test]
    fn each_service_speaks_its_own_words() {
        assert_eq!(Service::GitLab.pulls_title(), "MERGE REQUESTS");
        assert_eq!(Service::GitLab.ci_title(), "PIPELINES");
        assert_eq!(Service::GitLab.pull_number(7), "!7");
        assert_eq!(Service::GitHubEnterprise.pulls_title(), "PULL REQUESTS");
        assert_eq!(Service::GitHubEnterprise.pull_number(7), "#7");
        assert_eq!(pull_branch(Service::GitLab, 7), "mr/7");
        assert_eq!(pull_branch(Service::GitHubEnterprise, 7), "pr/7");

        assert_eq!(Problem::NotSignedIn.line(Service::GitLab), "Not signed in: private project");
        assert_eq!(Problem::Unreachable("timed out".to_owned()).line(Service::GitLab), "GitLab unreachable: timed out");
        let gitlab = Problem::NotSignedIn.detail(Service::GitLab);
        assert!(gitlab.contains("GITLAB_TOKEN") && gitlab.contains("glab auth login") && gitlab.contains("projects"));
        assert!(!gitlab.contains("GITHUB_TOKEN"));
        let enterprise = Problem::SignInRejected.detail(Service::GitHubEnterprise);
        assert!(enterprise.contains("GH_ENTERPRISE_TOKEN") && enterprise.contains("--hostname"));
        assert!(!enterprise.contains("GITHUB_TOKEN,"));

        assert_eq!(Auth::SignedIn(TokenSource::Cli).describe(Service::GitLab), "Signed in through the GitLab CLI");
        assert_eq!(Auth::Anonymous.describe(Service::GitLab), "Not signed in: public projects only");
        assert_eq!(Auth::SignedIn(TokenSource::Cli).describe(Service::GitHubEnterprise), "Signed in through the GitHub CLI");
        assert_eq!(Auth::Anonymous.describe(Service::GitHub), "Not signed in: public repositories only");
        assert!(Problem::NotSignedIn.detail(Service::GitHub).contains("only shows public repositories without"));

        let gitlab_com = at(Service::GitLab, "gitlab.com", "g", "a");
        let own = at(Service::GitLab, "git.acme.com", "g", "a");
        assert_eq!((gitlab_com.site(), own.site()), ("GitLab".to_owned(), "git.acme.com".to_owned()));
        assert!(!gitlab_com.self_hosted() && own.self_hosted());

        let cmd = checkout_pull("origin", Service::GitLab, 7, false);
        assert_eq!(cmd.label, "Check out merge request !7");
    }

    // ---- which token goes where ----

    fn gitlab_com() -> RepoId {
        at(Service::GitLab, "gitlab.com", "group", "app")
    }

    fn own_gitlab() -> RepoId {
        at(Service::GitLab, "git.acme.com", "team", "app")
    }

    fn enterprise() -> RepoId {
        at(Service::GitHubEnterprise, "ghe.acme.com", "octo", "app")
    }

    fn found(fake: &Fake, repo: &RepoId) -> Option<(String, TokenSource)> {
        find_token(fake, repo, &[]).map(|(token, source)| (token.0, source))
    }

    #[test]
    fn github_com_tokens_never_go_to_an_enterprise_server_or_the_other_way_round() {
        let everything = Fake {
            env: vec![
                ("GITHUB_TOKEN", SECRET),
                ("GH_TOKEN", SECRET),
                ("GITLAB_TOKEN", SECRET),
                ("GH_ENTERPRISE_TOKEN", OTHER),
            ],
            ..Default::default()
        };
        // The Enterprise server gets the Enterprise variable, nothing else.
        assert_eq!(found(&everything, &enterprise()), Some((OTHER.to_owned(), TokenSource::Env("GH_ENTERPRISE_TOKEN"))));
        assert_eq!(everything.asked(), ["GH_HOST", "GH_ENTERPRISE_TOKEN"]);

        // Without it: GITHUB_ENTERPRISE_TOKEN, then gh and git for that host.
        let github_only = Fake { env: vec![("GITHUB_TOKEN", SECRET), ("GH_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(found(&github_only, &enterprise()), None);
        assert_eq!(
            github_only.asked(),
            ["GH_HOST", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "gh ghe.acme.com", "git ghe.acme.com"]
        );
        let fallback = Fake { env: vec![("GITHUB_ENTERPRISE_TOKEN", OTHER)], ..Default::default() };
        assert_eq!(found(&fallback, &enterprise()).unwrap().1, TokenSource::Env("GITHUB_ENTERPRISE_TOKEN"));

        // github.com never reads the Enterprise variables.
        let enterprise_only = Fake { env: vec![("GH_ENTERPRISE_TOKEN", OTHER)], ..Default::default() };
        assert_eq!(found(&enterprise_only, &repo()), None);
        assert_eq!(enterprise_only.asked(), ["GITHUB_TOKEN", "GH_TOKEN", "gh github.com", "git github.com"]);

        // GH_HOST naming another server keeps the variable from this one.
        let elsewhere = Fake { env: vec![("GH_HOST", "ghe.other.example"), ("GH_ENTERPRISE_TOKEN", OTHER)], ..Default::default() };
        assert_eq!(found(&elsewhere, &enterprise()), None);
        assert_eq!(elsewhere.asked(), ["GH_HOST", "gh ghe.acme.com", "git ghe.acme.com"]);
        for value in ["ghe.acme.com", "GHE.acme.com", "https://ghe.acme.com/", "https://ghe.acme.com:443"] {
            let here = Fake { env: vec![("GH_HOST", value), ("GH_ENTERPRISE_TOKEN", OTHER)], ..Default::default() };
            assert_eq!(found(&here, &enterprise()).unwrap().0, OTHER, "{value}");
        }
    }

    #[test]
    fn gitlab_token_variables_belong_to_the_host_gitlab_host_names() {
        // gitlab.com: GITLAB_TOKEN when GITLAB_HOST is unset or names it.
        let plain = Fake { env: vec![("GITLAB_TOKEN", SECRET), ("GITHUB_TOKEN", OTHER)], ..Default::default() };
        assert_eq!(found(&plain, &gitlab_com()), Some((SECRET.to_owned(), TokenSource::Env("GITLAB_TOKEN"))));
        assert_eq!(plain.asked(), ["GITLAB_HOST", "GITLAB_TOKEN"]);
        let named = Fake { env: vec![("GITLAB_HOST", "https://gitlab.com"), ("GITLAB_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(found(&named, &gitlab_com()).unwrap().0, SECRET);

        // GITLAB_HOST names another server: the variable is that server's.
        let theirs = Fake { env: vec![("GITLAB_HOST", "git.acme.com"), ("GITLAB_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(found(&theirs, &gitlab_com()), None);
        assert_eq!(theirs.asked(), ["GITLAB_HOST", "glab gitlab.com", "git gitlab.com"]);
        assert_eq!(found(&theirs, &own_gitlab()).unwrap().0, SECRET);

        // A self-hosted server gets GITLAB_TOKEN only when GITLAB_HOST names it.
        let unnamed = Fake { env: vec![("GITLAB_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(found(&unnamed, &own_gitlab()), None);
        assert_eq!(unnamed.asked(), ["GITLAB_HOST", "glab git.acme.com", "git git.acme.com"]);
        for value in ["git.acme.com.evil.example", "acme.com", "evil.example", "", "https://git.acme.com/gitlab"] {
            let wrong = Fake { env: vec![("GITLAB_HOST", value), ("GITLAB_TOKEN", SECRET)], ..Default::default() };
            assert_eq!(found(&wrong, &own_gitlab()), None, "{value}");
        }
        // With a port, the port is part of the name.
        let ported = at(Service::GitLab, "git.acme.com:8443", "team", "app");
        let host_only = Fake { env: vec![("GITLAB_HOST", "git.acme.com"), ("GITLAB_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(found(&host_only, &ported), None);
        let with_port = Fake { env: vec![("GITLAB_HOST", "https://git.acme.com:8443"), ("GITLAB_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(found(&with_port, &ported).unwrap().0, SECRET);

        // GitLab never reads GitHub's variables, nor GitHub GitLab's.
        let github = Fake { env: vec![("GITHUB_TOKEN", SECRET), ("GH_TOKEN", SECRET), ("GH_ENTERPRISE_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(found(&github, &gitlab_com()), None);
        assert!(github.asked().iter().all(|asked| !asked.contains("GH")));
        let gitlab = Fake { env: vec![("GITLAB_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(found(&gitlab, &repo()), None);
        assert_eq!(found(&gitlab, &enterprise()), None);
    }

    #[test]
    fn the_cli_and_the_credential_helper_are_asked_about_the_repositorys_host_only() {
        let tools = Fake { cli: Some("glpat-cli-token-123456"), helper: Some("helper-token-123456"), ..Default::default() };
        assert_eq!(found(&tools, &own_gitlab()).unwrap().1, TokenSource::Cli);
        assert_eq!(tools.asked(), ["GITLAB_HOST", "glab git.acme.com"]);

        let helper = Fake { helper: Some("helper-token-123456"), ..Default::default() };
        let ported = at(Service::GitLab, "git.acme.com:8443", "team", "app");
        assert_eq!(found(&helper, &ported).unwrap().1, TokenSource::CredentialHelper);
        assert_eq!(helper.asked(), ["GITLAB_HOST", "glab git.acme.com:8443", "git git.acme.com:8443"]);

        // The commands themselves: one host, no token variables to fall back on.
        let args = |cmd: &Command| cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let removed = |cmd: &Command| {
            cmd.get_envs().filter(|(_, value)| value.is_none()).map(|(name, _)| name.to_string_lossy().into_owned()).collect::<Vec<_>>()
        };
        let glab = cli_command(Service::GitLab, "git.acme.com").unwrap();
        assert_eq!(glab.get_program(), "glab");
        assert_eq!(args(&glab), ["config", "get", "token", "--host", "git.acme.com"]);
        for name in ["GITLAB_TOKEN", "GITLAB_ACCESS_TOKEN", "OAUTH_TOKEN", "GITLAB_HOST"] {
            assert!(removed(&glab).contains(&name.to_owned()), "{name}");
        }
        let gh = cli_command(Service::GitHubEnterprise, "ghe.acme.com").unwrap();
        assert_eq!(gh.get_program(), "gh");
        assert_eq!(args(&gh), ["auth", "token", "--hostname", "ghe.acme.com"]);
        for name in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "GH_HOST"] {
            assert!(removed(&gh).contains(&name.to_owned()), "{name}");
        }
        assert_eq!(args(&cli_command(Service::GitHub, "github.com").unwrap()), ["auth", "token", "--hostname", "github.com"]);
        let git = credential_command(Path::new("."));
        assert_eq!(args(&git), ["-c", "core.askPass=", "credential", "fill"]);
        assert!(git.get_envs().any(|(name, value)| name == "GIT_TERMINAL_PROMPT" && value == Some(std::ffi::OsStr::new("0"))));
    }

    #[test]
    fn the_wire_attaches_a_token_only_below_its_own_api() {
        let token = Token::new(SECRET).unwrap();
        let ask = |wire: &Wire, url: &str| wire.authorization(&HttpRequest { url, etag: None, token: Some(&token) });
        let gitlab = Wire::new("https://git.acme.com/api/v4".to_owned(), gitlab::HEADERS);
        assert_eq!(ask(&gitlab, "https://git.acme.com/api/v4/projects/team%2Fapp/merge_requests"), Some(format!("Bearer {SECRET}")));
        for url in [
            "https://git.acme.com/team/app",
            "https://git.acme.com/api/v40/projects",
            "https://git.acme.com/api/v4.evil.example/x",
            "https://git.acme.com/api/v4@evil.example/x",
            "https://git.acme.com/api/v4/\\@evil.example",
            "https://git.acme.com/api/v4/ x",
            "http://git.acme.com/api/v4/projects",
            "https://git.acme.com.evil.example/api/v4/projects",
            "https://gitlab.com/api/v4/projects",
            "https://api.github.com/repos/octo/app",
        ] {
            assert_eq!(ask(&gitlab, url), None, "{url}");
        }
        // No token, no header; an API address that is not HTTPS never gets one.
        assert_eq!(gitlab.authorization(&HttpRequest { url: "https://git.acme.com/api/v4/x", etag: None, token: None }), None);
        let plain = Wire::new("http://git.acme.com/api/v4".to_owned(), gitlab::HEADERS);
        assert_eq!(ask(&plain, "http://git.acme.com/api/v4/projects"), None);
        let github = Wire::new(API.to_owned(), github::HEADERS);
        assert!(ask(&github, "https://api.github.com/repos/octo/app/pulls").is_some());
        assert_eq!(ask(&github, "https://ghe.acme.com/api/v3/repos/octo/app/pulls"), None);
    }

    #[test]
    fn an_enterprise_server_is_asked_through_its_own_api_with_its_own_token() {
        let sent = Log::default();
        let moved = "https://ghe.acme.com/api/v3/repositories/42/pulls?state=open&per_page=30";
        let transport = Script {
            replies: vec![
                reply(301, &[("location", moved)], ""),
                reply(200, &[], PULLS),
                reply(302, &[("location", "https://api.github.com/repos/octo/app/issues")], ""),
                reply(502, &[], "<html>"),
            ]
            .into(),
            sent: sent.clone(),
        };
        let secrets = Fake { env: vec![("GITHUB_TOKEN", SECRET), ("GH_ENTERPRISE_TOKEN", OTHER)], ..Default::default() };
        let mut client = Client::new(enterprise(), Box::new(transport), Box::new(secrets));

        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, .. } if items.len() == 2));
        assert_eq!(client.auth(), Auth::SignedIn(TokenSource::Env("GH_ENTERPRISE_TOKEN")));
        // A redirect to github.com is not followed from an Enterprise server.
        assert_eq!(
            failure(client.issues(NOW)),
            Problem::Failed("GitHub Enterprise sent an unexpected redirect".to_owned())
        );
        assert_eq!(failure(client.runs(NOW)), Problem::Failed("GitHub Enterprise answered 502".to_owned()));

        let sent = sent.lock().unwrap();
        let urls: Vec<&str> = sent.iter().map(|request| request.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://ghe.acme.com/api/v3/repos/octo/app/pulls?state=open&per_page=30",
                moved,
                "https://ghe.acme.com/api/v3/repos/octo/app/issues?state=open&per_page=50",
                "https://ghe.acme.com/api/v3/repos/octo/app/actions/runs?per_page=20&exclude_pull_requests=true",
            ]
        );
        assert!(sent.iter().all(|request| request.token.as_deref() == Some(OTHER)));
    }

    #[test]
    fn gitlab_rate_limit_headers_are_understood() {
        let reset = (NOW + 600).to_string();
        let response = HttpResponse {
            status: 429,
            headers: vec![("ratelimit-remaining".to_owned(), "0".to_owned()), ("ratelimit-reset".to_owned(), reset)],
            body: "Retry later".to_owned(),
        };
        let rate = Rate::of(&response);
        assert_eq!(rate, Rate { remaining: Some(0), reset: Some(NOW + 600), retry_after: None });
        assert_eq!(limited_until(429, &rate, "Retry later", 0, NOW), Some(NOW + 600));
    }

    #[test]
    fn the_offline_fixture_serves_gitlab_endpoints_too() {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-canned-gitlab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("merge_requests.json"), r#"[{"iid": 4, "title": "Speed up", "source_branch": "fast"}]"#).unwrap();
        std::fs::write(dir.join("pipelines.json"), r#"[{"id": 9, "status": "running"}]"#).unwrap();

        let mut client = gitlab::Client::new(own_gitlab(), Box::new(Canned { dir: dir.clone(), created: false }), Box::new(NoSecrets));
        assert!(matches!(client.pulls(NOW), Outcome::Fresh { ref items, .. } if items[0].number == 4));
        assert!(matches!(client.runs(NOW), Outcome::Fresh { ref items, .. } if items[0].state == RunState::Running));
        // No issues.json: the project looks private.
        assert_eq!(failure(client.issues(NOW)), Problem::NotSignedIn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_merge_request_is_checked_out_from_gitlabs_ref() {
        use crate::tests::{Fixture, git};

        let fx = Fixture::new("hosting-mr");
        let origin = fx.dir.with_file_name(format!("hosting-mr-origin-{}.git", std::process::id()));
        let _ = std::fs::remove_dir_all(&origin);
        let origin_arg = origin.to_string_lossy().into_owned();
        fx.write("a.txt", "base\n");
        fx.commit_all("base");
        git(&fx.dir, &["clone", "-q", "--bare", ".", &origin_arg]);
        git(&fx.dir, &["remote", "add", "origin", &origin_arg]);

        // GitLab keeps a merge request's head at refs/merge-requests/<iid>/head.
        git(&fx.dir, &["checkout", "-q", "--detach"]);
        fx.write("mr.txt", "one\n");
        fx.commit_all("MR: first");
        git(&fx.dir, &["push", "-q", "origin", "HEAD:refs/merge-requests/3/head"]);
        let head = git(&fx.dir, &["rev-parse", "HEAD"]);
        git(&fx.dir, &["checkout", "-q", "main"]);

        let done = fx.run(checkout_pull("origin", Service::GitLab, 3, false));
        assert_eq!(done.snap.head.branch.as_deref(), Some("mr/3"));
        assert_eq!(done.snap.head.oid.unwrap().to_string(), head);
        assert!(done.undo.is_some());

        drop(fx);
        let _ = std::fs::remove_dir_all(&origin);
    }
}
