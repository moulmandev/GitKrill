//! Tests for what round five added to the shared hosting code: servers
//! installed under a path, the repositories related to the one shown
//! (forks and parents), and the details step of a round (review states,
//! fork names, the parent) as the per-tab state takes it in; then the
//! sidebar's switch between a fork and its parent ([`Family`]), and the
//! review cache and request budget the backends share.

use std::sync::atomic::AtomicU32;

use super::tests::{Fake, NO_SERVERS, OTHER, SECRET};
use super::*;

fn servers(entries: &[(&str, ServerKind)]) -> Servers {
    entries.iter().map(|(host, kind)| ((*host).to_owned(), *kind)).collect()
}

fn at(service: Service, host: &str, owner: &str, name: &str) -> RepoId {
    RepoId { service, host: host.to_owned(), owner: owner.to_owned(), name: name.to_owned() }
}

fn remote(name: &str, url: &str) -> RemoteInfo {
    RemoteInfo { name: name.to_owned(), url: url.to_owned() }
}

// ---- a server installed under a path ----

#[test]
fn a_gitlab_installed_under_a_path_is_found_through_https_and_ssh() {
    let lab = servers(&[("example.com/gitlab", ServerKind::GitLab)]);
    let plain = at(Service::GitLab, "example.com/gitlab", "group", "app");
    for url in [
        "https://example.com/gitlab/group/app.git",
        "https://example.com/gitlab/group/app",
        "https://ada@Example.com/gitlab/group/app.git/",
        "http://example.com/gitlab/group/app.git",
        // SSH addresses never carry the path GitLab is installed under.
        "git@example.com:group/app.git",
        "ssh://git@example.com:2222/group/app.git",
    ] {
        assert_eq!(parse_remote(url, &lab), Some(plain.clone()), "{url}");
    }
    let nested = parse_remote("https://example.com/gitlab/group/sub/app.git", &lab).unwrap();
    assert_eq!(nested, at(Service::GitLab, "example.com/gitlab", "group/sub", "app"));

    // Everything is addressed below the path.
    assert_eq!(plain.api_base(), "https://example.com/gitlab/api/v4");
    assert_eq!(plain.pull_url(12), "https://example.com/gitlab/group/app/-/merge_requests/12");
    assert_eq!(plain.pulls_url(), "https://example.com/gitlab/group/app/-/merge_requests");
    assert_eq!(gitlab::project_id(&nested), "group%2Fsub%2Fapp");
    assert!(plain.self_hosted());
    assert_eq!(plain.site(), "example.com/gitlab");
    assert_eq!(server_name(&plain.host), "example.com");

    // Not under the path, or nothing left for an owner and a name.
    for url in [
        "https://example.com/group/app.git",
        "https://example.com/gitlab/app.git",
        "https://example.com/gitlab",
        "https://example.com/gitlabs/group/app.git",
        "https://example.com/Gitlab/group/app.git",
        "https://example.com/gitlab/-/group/app.git",
        "https://other.example.com/gitlab/group/app.git",
    ] {
        assert_eq!(parse_remote(url, &lab), None, "{url}");
    }
}

#[test]
fn a_path_server_and_a_root_server_on_one_host_are_told_apart() {
    let both = servers(&[("example.com", ServerKind::GitLab), ("example.com/gitlab", ServerKind::GitLab)]);
    let root = parse_remote("https://example.com/group/app.git", &both).unwrap();
    assert_eq!(root.host, "example.com");
    let under = parse_remote("https://example.com/gitlab/group/app.git", &both).unwrap();
    assert_eq!(under.host, "example.com/gitlab");
    // One host's SSH leads to one server: the one the user added with its path.
    assert_eq!(parse_remote("git@example.com:group/app.git", &both).unwrap().host, "example.com/gitlab");

    // The longest path wins.
    let deeper = servers(&[("example.com/tools", ServerKind::GitLab), ("example.com/tools/gitlab", ServerKind::GitLab)]);
    assert_eq!(parse_remote("https://example.com/tools/gitlab/group/app", &deeper).unwrap().host, "example.com/tools/gitlab");
    assert_eq!(
        parse_remote("https://example.com/tools/group/app", &deeper).unwrap(),
        at(Service::GitLab, "example.com/tools", "group", "app")
    );

    // An HTTPS port is part of the server; SSH says nothing about it.
    let ported = servers(&[("example.com:8443/gitlab", ServerKind::GitLab)]);
    let repo = parse_remote("https://example.com:8443/gitlab/group/app.git", &ported).unwrap();
    assert_eq!(repo.api_base(), "https://example.com:8443/gitlab/api/v4");
    assert_eq!(server_name(&repo.host), "example.com:8443");
    assert_eq!(parse_remote("https://example.com/gitlab/group/app.git", &ported), None);
    assert_eq!(parse_remote("git@example.com:group/app.git", &ported), None);

    // "Neither", or a path no URL could hold: never contacted.
    let ignored = servers(&[("example.com/gitlab", ServerKind::Ignored)]);
    assert_eq!(parse_remote("https://example.com/gitlab/group/app.git", &ignored), None);
    let odd = servers(&[("example.com/../gitlab", ServerKind::GitLab), ("example.com/a b", ServerKind::GitLab)]);
    assert_eq!(parse_remote("https://example.com/../gitlab/group/app.git", &odd), None);
}

#[test]
fn a_remote_under_a_connected_path_is_not_offered_again() {
    let remotes = [remote("origin", "https://example.com/gitlab/group/app.git")];
    // Before: its host is offered, with the whole path, as it always was.
    let offered = offers(&remotes, &NO_SERVERS);
    assert_eq!((offered[0].host.as_str(), offered[0].path.as_str()), ("example.com", "gitlab/group/app"));
    // After: it is a hosted repository, so there is nothing to offer.
    let lab = servers(&[("example.com/gitlab", ServerKind::GitLab)]);
    assert!(offers(&remotes, &lab).is_empty());
    assert_eq!(detect(&remotes, &lab).unwrap().1.host, "example.com/gitlab");
}

#[test]
fn tokens_for_a_path_server_are_looked_up_by_the_servers_name() {
    let repo = at(Service::GitLab, "example.com/gitlab", "group", "app");
    let fake = Fake { cli: Some(OTHER), ..Default::default() };
    assert_eq!(find_token(&fake, &repo, &[]).map(|(_, source)| source), Some(TokenSource::Cli));
    assert_eq!(fake.asked(), ["GITLAB_HOST", "glab example.com"]);
    let fake = Fake { helper: Some(OTHER), ..Default::default() };
    find_token(&fake, &repo, &[]);
    assert_eq!(fake.asked(), ["GITLAB_HOST", "glab example.com", "git example.com"]);
    assert_eq!(credential_query(server_name(&repo.host)), "protocol=https\nhost=example.com\n\n");

    // GITLAB_HOST may name it with its path or by its name alone.
    for host in ["https://example.com/gitlab/", "example.com/gitlab", "example.com", "https://example.com"] {
        let fake = Fake { env: vec![("GITLAB_HOST", host), ("GITLAB_TOKEN", SECRET)], ..Default::default() };
        assert_eq!(find_token(&fake, &repo, &[]).map(|(_, source)| source), Some(TokenSource::Env("GITLAB_TOKEN")), "{host}");
    }
    let fake = Fake { env: vec![("GITLAB_HOST", "other.example.com"), ("GITLAB_TOKEN", SECRET)], ..Default::default() };
    assert_eq!(find_token(&fake, &repo, &[]), None);
    // A host without a path is still only its exact name.
    let root = at(Service::GitLab, "example.com", "group", "app");
    let fake = Fake { env: vec![("GITLAB_HOST", "example.com/gitlab"), ("GITLAB_TOKEN", SECRET)], ..Default::default() };
    assert_eq!(find_token(&fake, &root, &[]), None);
}

#[test]
fn the_wire_sends_a_path_servers_token_below_its_api_only() {
    let wire = Wire::new(api_base(Service::GitLab, "example.com/gitlab"), gitlab::HEADERS);
    let token = Token::new(SECRET).unwrap();
    let carries = |url: &str| wire.authorization(&HttpRequest { url, etag: None, token: Some(&token) }).is_some();
    assert!(carries("https://example.com/gitlab/api/v4/projects/group%2Fapp/merge_requests"));
    assert!(!carries("https://example.com/api/v4/projects/group%2Fapp/merge_requests"));
    assert!(!carries("https://example.com/gitlab/api/v40/x"));
    assert!(!carries("https://example.com/gitlab/group/app"));
}

#[test]
fn an_enterprise_servers_graphql_beside_its_api_gets_the_token_and_nothing_else_does() {
    assert_eq!(beside("https://ghe.acme.com/api/v3").as_deref(), Some("https://ghe.acme.com/api/graphql"));
    assert_eq!(beside(github::API), None);
    assert_eq!(beside("https://gitlab.com/api/v4"), None);
    assert_eq!(github::graphql_url(github::API), "https://api.github.com/graphql");
    assert_eq!(github::graphql_url("https://ghe.acme.com/api/v3"), "https://ghe.acme.com/api/graphql");
    assert_eq!(github::graphql_url("https://api.acme.ghe.com"), "https://api.acme.ghe.com/graphql");

    let wire = Wire::new("https://ghe.acme.com/api/v3".to_owned(), github::HEADERS);
    let token = Token::new(SECRET).unwrap();
    let carries = |url: &str| wire.authorization(&HttpRequest { url, etag: None, token: Some(&token) }).is_some();
    assert!(carries("https://ghe.acme.com/api/v3/repos/octo/app/pulls"));
    assert!(carries("https://ghe.acme.com/api/graphql"));
    for url in [
        "https://ghe.acme.com/api/graphql/x",
        "https://ghe.acme.com/api/graphqlx",
        "https://ghe.acme.com/api/graphql?x",
        "https://ghe.acme.com/graphql",
        "https://evil.example/api/graphql",
    ] {
        assert!(!carries(url), "{url}");
    }
}

#[test]
fn fixture_files_are_named_after_the_detail_asked_for() {
    for (path, name) in [
        ("https://gitlab.com/api/v4/projects/group%2Fapp/merge_requests/12/approvals", Some("approvals-12")),
        ("https://gitlab.com/api/v4/projects/77", Some("project-77")),
        ("https://gitlab.com/api/v4/projects/group%2Fapp", Some("project")),
        ("https://api.github.com/repos/octo/app", Some("repo")),
        ("https://api.github.com/repos/octo/app/pulls/12/reviews", Some("reviews-12")),
        ("https://codeberg.org/api/v1/repos/team/app/pulls/7/reviews", Some("reviews-7")),
        ("https://codeberg.org/api/v1/repos/team/app", Some("repo")),
        ("https://api.bitbucket.org/2.0/repositories/team/app", Some("repo")),
        ("https://api.github.com/repos/octo/app/pulls/x/reviews", None),
        ("https://api.github.com/repos/octo/app/pulls", None),
        ("https://api.bitbucket.org/2.0/repositories/team/app/pullrequests", None),
        // Azure DevOps lists below `repositories/<id>/`: still its lists.
        ("https://dev.azure.com/org/proj/_apis/git/repositories/app/pullrequests", None),
        ("https://gitlab.com/api/v4/projects/group%2Fapp/merge_requests", None),
        ("https://gitlab.com/api/v4/projects/7/pipelines", None),
    ] {
        assert_eq!(detail_fixture(path).as_deref(), name, "{path}");
    }
}

// ---- forks ----

#[test]
fn related_remotes_are_other_repositories_on_the_same_server() {
    let shown = at(Service::GitHub, "github.com", "ada", "app");
    let remotes = [
        remote("origin", "git@github.com:ada/app.git"),
        remote("upstream", "https://github.com/octo/app.git"),
        // The same repository under another spelling is not another one.
        remote("mirror", "https://github.com/Ada/App"),
        remote("lab", "git@gitlab.com:octo/app.git"),
        remote("local", "E:/demo-origin.git"),
        remote("grace", "git@github.com:grace/app.git"),
    ];
    let found = related(&remotes, &NO_SERVERS, &shown);
    let names: Vec<(&str, String)> = found.iter().map(|(name, repo)| (name.as_str(), repo.slug())).collect();
    assert_eq!(names, [("upstream", "octo/app".to_owned()), ("grace", "grace/app".to_owned())]);
    assert!(related(&[], &NO_SERVERS, &shown).is_empty());
}

#[test]
fn a_repository_named_by_a_service_is_validated_like_a_remote() {
    let hub = at(Service::GitHub, "github.com", "ada", "app");
    assert_eq!(hub.sibling("octo/app"), Some(at(Service::GitHub, "github.com", "octo", "app")));
    for path in ["octo", "octo/app/extra", "../app", "octo/..", "oc to/app", "octo/app?x", "octo/app\n", "", "/app"] {
        assert_eq!(hub.sibling(path), None, "{path:?}");
    }
    let lab = at(Service::GitLab, "example.com/gitlab", "group", "app");
    assert_eq!(lab.sibling("team/sub/app"), Some(at(Service::GitLab, "example.com/gitlab", "team/sub", "app")));
    assert_eq!(lab.sibling("team/-/app"), None);
    assert!(hub.same(&at(Service::GitHub, "github.com", "ADA", "App")));
    assert!(!hub.same(&at(Service::GitHubEnterprise, "github.com", "ada", "app")));
}

#[test]
fn the_compare_page_across_forks_names_the_forks_owner() {
    let parent = at(Service::GitHub, "github.com", "octo", "app");
    let fork = at(Service::GitHub, "github.com", "ada", "app");
    assert_eq!(
        parent.compare_url_from("main", &fork, "feature/dark mode"),
        "https://github.com/octo/app/compare/main...ada:feature/dark%20mode?expand=1"
    );
    assert_eq!(parent.compare_url_from("main", &parent, "x"), parent.compare_url("main", "x"));
    // GitLab starts on the fork, whose form offers its parent as the target.
    let lab_parent = at(Service::GitLab, "gitlab.com", "group", "app");
    let lab_fork = at(Service::GitLab, "gitlab.com", "ada", "app");
    assert_eq!(lab_parent.compare_url_from("main", &lab_fork, "x"), lab_fork.compare_url("main", "x"));
}

#[test]
fn a_pull_request_from_a_fork_is_checked_out_from_the_base_repository() {
    use crate::tests::{Fixture, git};

    let fx = Fixture::new("hosting-fork-pr");
    let base = fx.dir.with_file_name(format!("hosting-fork-base-{}.git", std::process::id()));
    let fork = fx.dir.with_file_name(format!("hosting-fork-fork-{}.git", std::process::id()));
    for dir in [&base, &fork] {
        let _ = std::fs::remove_dir_all(dir);
    }
    let (base_arg, fork_arg) = (base.to_string_lossy().into_owned(), fork.to_string_lossy().into_owned());

    fx.write("a.txt", "base\n");
    fx.commit_all("base");
    git(&fx.dir, &["clone", "-q", "--bare", ".", &base_arg]);
    git(&fx.dir, &["clone", "-q", "--bare", ".", &fork_arg]);
    git(&fx.dir, &["remote", "add", "origin", &base_arg]);

    // Someone else's fork gets a branch called main too, with their work;
    // the service publishes its head in the base repository.
    git(&fx.dir, &["checkout", "-q", "--detach"]);
    fx.write("their file.txt", "from the fork\n");
    fx.commit_all("Work done in the fork");
    let head = git(&fx.dir, &["rev-parse", "HEAD"]);
    git(&fx.dir, &["push", "-q", &fork_arg, "HEAD:refs/heads/main"]);
    git(&base, &["fetch", "-q", &fork_arg, "main:refs/pull/3/head"]);
    git(&base, &["fetch", "-q", &fork_arg, "main:refs/merge-requests/4/head"]);
    git(&fx.dir, &["checkout", "-q", "main"]);
    git(&fx.dir, &["reset", "-q", "--hard", "HEAD"]);

    // Checking out needs only the base repository's remote; the local
    // main (a branch of the same name as the fork's) is left alone.
    let main = git(&fx.dir, &["rev-parse", "main"]);
    let done = fx.run(checkout_pull("origin", Service::GitHub, 3, false));
    assert_eq!(done.snap.head.branch.as_deref(), Some("pr/3"));
    assert_eq!(done.snap.head.oid.unwrap().to_string(), head);
    assert_eq!(fx.read("their file.txt"), "from the fork\n");
    assert_eq!(git(&fx.dir, &["rev-parse", "main"]), main);
    // The branch does not track anything: the fork is not a remote here.
    assert!(done.snap.branches.iter().find(|branch| branch.name == "pr/3").unwrap().upstream.is_none());

    git(&fx.dir, &["checkout", "-q", "main"]);
    let done = fx.run(checkout_pull("origin", Service::GitLab, 4, false));
    assert_eq!(done.snap.head.branch.as_deref(), Some("mr/4"));
    assert_eq!(done.snap.head.oid.unwrap().to_string(), head);

    drop(fx);
    for dir in [&base, &fork] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

// ---- the details step, through the worker and the per-tab state ----

/// A backend whose lists are fixed and whose details say what `details`
/// holds, once.
#[derive(Clone, Default)]
struct Detailed {
    details: Arc<Mutex<Option<Details>>>,
    asked: Arc<AtomicU32>,
    across: Arc<Mutex<Vec<(NewPull, Across)>>>,
}

fn listed(number: u64, head_repo: Option<&str>) -> PullRequest {
    PullRequest {
        number,
        title: format!("Pull {number}"),
        head_ref: "feature".to_owned(),
        head_repo: head_repo.map(str::to_owned),
        base_ref: "main".to_owned(),
        ..Default::default()
    }
}

impl Backend for Detailed {
    fn pulls(&mut self, _now: u64) -> Outcome<PullRequest> {
        Outcome::Fresh { items: vec![listed(12, Some("octo/app")), listed(9, None)], more: false }
    }
    fn issues(&mut self, _now: u64) -> Outcome<Issue> {
        Outcome::Unchanged
    }
    fn runs(&mut self, _now: u64) -> Outcome<Run> {
        Outcome::Unchanged
    }
    fn auth(&self) -> Auth {
        Auth::SignedIn(TokenSource::Cli)
    }
    fn look_again(&mut self) {}
    fn blocked_until(&self, _now: u64) -> Option<u64> {
        None
    }
    fn create_pull_across(&mut self, new: &NewPull, across: &Across, _now: u64) -> Result<PullRequest, Problem> {
        self.across.lock().unwrap().push((new.clone(), across.clone()));
        Ok(listed(77, Some("ada/app")))
    }
    fn details(&mut self, _now: u64) -> Option<Details> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.details.lock().unwrap().take()
    }
}

fn shown_remotes() -> Vec<RemoteInfo> {
    vec![remote("origin", "git@github.com:octo/app.git")]
}

fn settle(state: &mut HostingState) -> Shown {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let connect = |_: &RepoId| -> Box<dyn Backend> { panic!("already connected") };
        let shown = state.sync_with(&shown_remotes(), &NO_SERVERS, true, false, connect, || {}).unwrap();
        if !shown.view.busy {
            return shown;
        }
        assert!(Instant::now() < deadline, "the worker never finished");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn details_arrive_after_the_lists_and_fill_in_the_pull_requests() {
    let backend = Detailed::default();
    let parent = Parent { repo: at(Service::GitHub, "github.com", "upstream", "app"), default_branch: "trunk".to_owned() };
    *backend.details.lock().unwrap() = Some(Details {
        pulls: vec![
            PullRequest {
                number: 12,
                review: Review::Approved,
                approvals: Some(Approvals { given: 2, left: 0 }),
                ..Default::default()
            },
            PullRequest {
                number: 9,
                review: Review::Required,
                approvals: Some(Approvals { given: 1, left: 1 }),
                head_repo: Some("ada/app".to_owned()),
                ..Default::default()
            },
            // Not in the list (closed meanwhile): nothing to fill in.
            PullRequest { number: 5, review: Review::ChangesRequested, ..Default::default() },
        ],
        parent: Some(parent.clone()),
    });
    let mut state = HostingState::default();
    {
        let backend = backend.clone();
        state.sync_with(&shown_remotes(), &NO_SERVERS, true, true, move |_| Box::new(backend), || {});
    }
    let shown = settle(&mut state);
    let pulls = &shown.view.pulls.items;
    assert_eq!(pulls.len(), 2);
    assert_eq!((pulls[0].review, pulls[0].approvals), (Review::Approved, Some(Approvals { given: 2, left: 0 })));
    // What the list said stays when the details say nothing of it.
    assert_eq!(pulls[0].head_repo.as_deref(), Some("octo/app"));
    assert_eq!(pulls[0].title, "Pull 12");
    assert_eq!(pulls[1].review, Review::Required);
    assert_eq!(pulls[1].approvals.and_then(|approvals| approvals.fraction()).as_deref(), Some("1/2"));
    assert_eq!(pulls[1].head_repo.as_deref(), Some("ada/app"));
    assert_eq!(shown.view.parent, Some(parent));
    assert_eq!(backend.asked.load(Ordering::SeqCst), 1);

    // A later round with nothing new: the list comes fresh from the backend
    // (which keeps what it learnt; this one does not) and the parent stays.
    state.refresh();
    let shown = settle(&mut state);
    assert_eq!(backend.asked.load(Ordering::SeqCst), 2);
    assert!(shown.view.parent.is_some());
}

#[test]
fn a_pull_request_opened_in_another_repository_is_not_added_to_this_list() {
    let backend = Detailed::default();
    let mut state = HostingState::default();
    {
        let backend = backend.clone();
        state.sync_with(&shown_remotes(), &NO_SERVERS, true, true, move |_| Box::new(backend), || {});
    }
    settle(&mut state);
    let new = NewPull {
        head: "feature".to_owned(),
        base: "main".to_owned(),
        title: "Into the parent".to_owned(),
        body: String::new(),
        draft: false,
    };
    let across = Across {
        from: at(Service::GitHub, "github.com", "octo", "app"),
        into: at(Service::GitHub, "github.com", "upstream", "app"),
    };
    let id = state.create_pull_across(new.clone(), across.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let answer = loop {
        state.poll();
        if let Some(answer) = state.take_created(id) {
            break answer;
        }
        assert!(Instant::now() < deadline, "no answer");
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(answer.unwrap().number, 77);
    assert_eq!(backend.across.lock().unwrap().as_slice(), [(new.clone(), across)]);
    let shown = state.connected().unwrap();
    assert!(shown.view.pulls.items.iter().all(|pull| pull.number != 77));

    // From a fork into the repository shown: it is this list's.
    let across = Across {
        from: at(Service::GitHub, "github.com", "ada", "app"),
        into: at(Service::GitHub, "github.com", "Octo", "App"),
    };
    let id = state.create_pull_across(new, across).unwrap();
    loop {
        state.poll();
        if state.take_created(id).is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "no answer");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(state.connected().unwrap().view.pulls.items.first().map(|pull| pull.number), Some(77));
}

#[test]
fn approvals_read_as_a_fraction_only_while_some_are_missing() {
    assert_eq!(Approvals { given: 1, left: 2 }.fraction().as_deref(), Some("1/3"));
    assert_eq!(Approvals { given: 0, left: 1 }.fraction().as_deref(), Some("0/1"));
    assert_eq!(Approvals { given: 2, left: 0 }.fraction(), None);
    assert_eq!(PullRequest::default().review, Review::Unknown);
    assert_eq!(PullRequest::default().approvals, None);
}

// ---- a fork and its parent ----

#[test]
fn the_parent_is_the_one_the_service_names_else_the_upstream_remote() {
    let fork = at(Service::GitHub, "github.com", "ada", "app");
    let named = Parent { repo: at(Service::GitHub, "github.com", "octo", "app"), default_branch: "main".to_owned() };
    let origin = remote("origin", "git@github.com:ada/app.git");
    let upstream = remote("upstream", "https://github.com/Octo/App.git");
    let other = remote("upstream", "https://github.com/grace/app.git");

    // Named, with the remote that points at it if one does.
    assert_eq!(parent_of(&fork, Some(&named), std::slice::from_ref(&origin), &NO_SERVERS), Some((named.repo.clone(), None)));
    let found = parent_of(&fork, Some(&named), &[origin.clone(), upstream.clone()], &NO_SERVERS).unwrap();
    assert_eq!(found.1.as_deref(), Some("upstream"));
    // The service's word wins over a remote called upstream.
    let found = parent_of(&fork, Some(&named), &[origin.clone(), other.clone()], &NO_SERVERS).unwrap();
    assert_eq!((found.0.slug(), found.1), ("octo/app".to_owned(), None));
    // Not named: `upstream`, on the same server only.
    let found = parent_of(&fork, None, &[origin.clone(), other], &NO_SERVERS).unwrap();
    assert_eq!((found.0.slug(), found.1.as_deref()), ("grace/app".to_owned(), Some("upstream")));
    let lab = remote("upstream", "git@gitlab.com:octo/app.git");
    assert_eq!(parent_of(&fork, None, &[origin.clone(), lab], &NO_SERVERS), None);
    assert_eq!(parent_of(&fork, None, &[origin], &NO_SERVERS), None);

    // Fetched from its own address when no remote points at it.
    assert_eq!(named.repo.clone_url().as_deref(), Some("https://github.com/octo/app.git"));
    let lab = at(Service::GitLab, "example.com/gitlab", "group/sub", "app");
    assert_eq!(lab.clone_url().as_deref(), Some("https://example.com/gitlab/group/sub/app.git"));
    assert_eq!(at(Service::AzureDevOps, "dev.azure.com", "org/proj", "app").clone_url(), None);
}

/// One repository of a fork and its parent: fixed lists, its parent told
/// once, and a count of its rounds.
#[derive(Clone, Default)]
struct Member {
    parent: Option<Parent>,
    pulls: Vec<PullRequest>,
    rounds: Arc<AtomicU32>,
}

impl Backend for Member {
    fn pulls(&mut self, _now: u64) -> Outcome<PullRequest> {
        self.rounds.fetch_add(1, Ordering::SeqCst);
        Outcome::Fresh { items: self.pulls.clone(), more: false }
    }
    fn issues(&mut self, _now: u64) -> Outcome<Issue> {
        Outcome::Unchanged
    }
    fn runs(&mut self, _now: u64) -> Outcome<Run> {
        Outcome::Unchanged
    }
    fn auth(&self) -> Auth {
        Auth::SignedIn(TokenSource::Cli)
    }
    fn look_again(&mut self) {}
    fn blocked_until(&self, _now: u64) -> Option<u64> {
        None
    }
    fn create_pull_across(&mut self, _new: &NewPull, _across: &Across, _now: u64) -> Result<PullRequest, Problem> {
        Ok(listed(77, Some("ada/app")))
    }
    fn details(&mut self, _now: u64) -> Option<Details> {
        self.parent.take().map(|parent| Details { pulls: Vec::new(), parent: Some(parent) })
    }
}

struct Household {
    fork: Member,
    parent: Member,
    /// The repositories sessions were started for, in order.
    connected: Arc<Mutex<Vec<String>>>,
}

impl Household {
    fn new() -> Self {
        let named = Parent { repo: at(Service::GitHub, "github.com", "octo", "app"), default_branch: "main".to_owned() };
        Household {
            fork: Member { parent: Some(named), pulls: Vec::new(), ..Default::default() },
            parent: Member { pulls: vec![listed(5, Some("octo/app")), listed(6, Some("ada/app"))], ..Default::default() },
            connected: Arc::default(),
        }
    }

    /// One frame of the sidebar.
    fn sync(&self, state: &mut HostingState, remotes: &[RemoteInfo], show_parent: bool) -> Option<Shown> {
        let connect = |repo: &RepoId, _signed_out: bool| -> Box<dyn Backend> {
            self.connected.lock().unwrap().push(repo.slug());
            Box::new(if repo.owner == "ada" { self.fork.clone() } else { self.parent.clone() })
        };
        state.sync_family(remotes, &NO_SERVERS, &Disconnected::new(), (true, true, show_parent), connect, || {})
    }

    /// Frames until `done` says the sidebar shows what is awaited, nothing
    /// being refreshed.
    fn until(
        &self,
        state: &mut HostingState,
        remotes: &[RemoteInfo],
        show_parent: bool,
        done: impl Fn(&Shown) -> bool,
    ) -> Shown {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let shown = self.sync(state, remotes, show_parent).unwrap();
            let quiet = !shown.view.busy && !state.connected().unwrap().view.busy;
            if quiet && done(&shown) {
                return shown;
            }
            assert!(Instant::now() < deadline, "the sidebar never settled");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn connected(&self) -> Vec<String> {
        self.connected.lock().unwrap().clone()
    }
}

fn fork_remotes() -> Vec<RemoteInfo> {
    vec![remote("origin", "git@github.com:ada/app.git")]
}

#[test]
fn a_fork_shows_its_own_lists_until_switched_to_its_parent() {
    let house = Household::new();
    let mut state = HostingState::default();
    let remotes = fork_remotes();
    // The fork's lists, and once its parent is known, the switch.
    let shown = house.until(&mut state, &remotes, false, |shown| shown.family.is_some());
    let family = shown.family.clone().unwrap();
    assert_eq!((shown.repo.slug(), shown.remote.as_str()), ("ada/app".to_owned(), "origin"));
    assert_eq!((family.fork.slug(), family.fork_remote.as_str(), family.parent.slug()), ("ada/app".to_owned(), "origin", "octo/app".to_owned()));
    assert!(!family.parent_shown && family.parent_remote.is_none());
    // Nothing asked of the parent until it is shown.
    assert_eq!(house.connected(), ["ada/app"]);
    assert!(state.parent_shown().is_none());

    // Switched: the parent's lists, fetched from its address (no remote
    // points at it), while pull requests are still opened from the fork.
    let shown = house.until(&mut state, &remotes, true, |shown| shown.view.pulls.loaded);
    assert_eq!((shown.repo.slug(), shown.remote.as_str()), ("octo/app".to_owned(), "https://github.com/octo/app.git"));
    let family = shown.family.clone().unwrap();
    assert!(family.parent_shown);
    let from_fork: Vec<u64> = shown.view.pulls.items.iter().filter(|pr| family.is_from_fork(pr)).map(|pr| pr.number).collect();
    assert_eq!(from_fork, [6]);
    let connected = state.connected().unwrap();
    assert_eq!((connected.repo.slug(), connected.remote.as_str()), ("ada/app".to_owned(), "origin"));
    assert_eq!(connected.family.as_ref().map(|family| family.parent_shown), Some(true));
    assert_eq!(state.parent_shown().unwrap().repo.slug(), "octo/app");
    assert_eq!(house.connected(), ["ada/app", "octo/app"]);

    // A refresh by hand refreshes what is shown; the fork's only on purpose.
    let (fork_rounds, parent_rounds) = (house.fork.rounds.load(Ordering::SeqCst), house.parent.rounds.load(Ordering::SeqCst));
    state.refresh();
    house.until(&mut state, &remotes, true, |_| house.parent.rounds.load(Ordering::SeqCst) > parent_rounds);
    assert_eq!(house.fork.rounds.load(Ordering::SeqCst), fork_rounds);
    state.refresh_connected();
    house.until(&mut state, &remotes, true, |_| house.fork.rounds.load(Ordering::SeqCst) > fork_rounds);

    // Back to the fork: its lists again, the parent's session kept for
    // the next switch rather than started over.
    let shown = house.until(&mut state, &remotes, false, |shown| shown.repo.owner == "ada");
    assert_eq!(shown.family.map(|family| family.parent_shown), Some(false));
    assert!(state.parent_shown().is_none());
    house.until(&mut state, &remotes, true, |shown| shown.repo.owner == "octo");
    assert_eq!(house.connected(), ["ada/app", "octo/app"]);

    // A remote pointing at the parent is what it is fetched from.
    let with_upstream = vec![remote("origin", "git@github.com:ada/app.git"), remote("upstream", "https://github.com/octo/app.git")];
    let shown = house.until(&mut state, &with_upstream, true, |shown| shown.remote == "upstream");
    assert_eq!(shown.family.unwrap().parent_remote.as_deref(), Some("upstream"));
}

#[test]
fn a_pull_request_opened_from_the_fork_shows_in_the_parents_list_at_once() {
    let house = Household::new();
    let mut state = HostingState::default();
    let remotes = fork_remotes();
    house.until(&mut state, &remotes, true, |shown| shown.repo.owner == "octo" && shown.view.pulls.loaded);
    let new = NewPull {
        head: "feature".to_owned(),
        base: "main".to_owned(),
        title: "Into the parent".to_owned(),
        body: String::new(),
        draft: false,
    };
    let across = Across { from: at(Service::GitHub, "github.com", "ada", "app"), into: at(Service::GitHub, "github.com", "octo", "app") };
    let id = state.create_pull_across(new, across).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let answer = loop {
        state.poll();
        if let Some(answer) = state.take_created(id) {
            break answer;
        }
        assert!(Instant::now() < deadline, "no answer");
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(answer.unwrap().number, 77);
    // In the parent's list before its refresh confirms it; not the fork's.
    assert_eq!(state.parent_shown().unwrap().view.pulls.items.first().map(|pull| pull.number), Some(77));
    assert!(state.connected().unwrap().view.pulls.items.iter().all(|pull| pull.number != 77));
}

#[test]
fn a_repository_that_is_no_fork_has_no_switch_and_no_second_session() {
    let house = Household { fork: Member::default(), ..Household::new() };
    let mut state = HostingState::default();
    let remotes = fork_remotes();
    let shown = house.until(&mut state, &remotes, true, |shown| shown.view.pulls.loaded);
    assert!(shown.family.is_none() && shown.repo.owner == "ada");
    assert!(state.parent_shown().is_none());
    assert_eq!(house.connected(), ["ada/app"]);
    // Switched off: nothing at all.
    let connect = |_: &RepoId, _: bool| -> Box<dyn Backend> { panic!("not connected while off") };
    let off = state.sync_family(&remotes, &NO_SERVERS, &Disconnected::new(), (false, true, true), connect, || {});
    assert!(off.is_none() && state.connected().is_none());
}

// ---- what the backends share ----

#[test]
fn the_limiter_tells_what_is_left_of_the_window_while_it_lasts() {
    let mut limiter = Limiter::default();
    assert_eq!(limiter.left(1000), None);
    let rate = Rate { remaining: Some(42), reset: Some(1600), retry_after: None };
    assert_eq!(limiter.record(200, &rate, "", 1000), None);
    assert_eq!(limiter.left(1000), Some(42));
    // A new window: unknown again.
    assert_eq!(limiter.left(1600), None);
    // An answer without the headers leaves what was said.
    limiter.record(200, &Rate::default(), "", 1100);
    assert_eq!(limiter.left(1100), Some(42));
}

#[test]
fn reviews_are_kept_until_their_pull_request_changes() {
    let mut reviews = Reviews::default();
    let pull = |number: u64, updated: i64| PullRequest { number, updated, ..Default::default() };
    let mut list = vec![pull(3, 10), pull(2, 10), pull(1, 10)];
    reviews.listed(&mut list);
    assert_eq!(reviews.stale(2), [(3, 10), (2, 10)]);
    let approved = Some(Approvals { given: 1, left: 0 });
    assert_eq!(reviews.learnt(3, 10, Review::Approved, approved).map(|pull| pull.review), Some(Review::Approved));
    // Unknown is what was shown before: nothing to tell.
    assert_eq!(reviews.learnt(2, 10, Review::Unknown, None), None);
    assert_eq!(reviews.stale(30), [(1, 10)]);
    // The same again tells nothing new.
    assert_eq!(reviews.learnt(3, 10, Review::Approved, approved), None);

    // A fresh list: what is known is filled in; a changed one is stale;
    // a closed one is forgotten.
    let mut list = vec![pull(3, 20), pull(1, 10)];
    reviews.listed(&mut list);
    assert_eq!((list[0].review, list[0].approvals), (Review::Approved, approved));
    assert_eq!(reviews.stale(30), [(3, 20), (1, 10)]);
    let mut again = vec![pull(2, 10)];
    reviews.listed(&mut again);
    assert_eq!(again[0].review, Review::Unknown);
    assert_eq!(reviews.stale(30), [(2, 10)]);

    // A refresh by hand: all asked again, shown as they were meanwhile.
    reviews.learnt(2, 10, Review::ChangesRequested, None);
    reviews.ask_again();
    assert_eq!(reviews.stale(30), [(2, 10)]);
    let mut shown = vec![pull(2, 10)];
    reviews.listed(&mut shown);
    assert_eq!(shown[0].review, Review::ChangesRequested);
}

#[test]
fn a_repository_with_a_fixture_folder_of_its_own_is_answered_from_it() {
    let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("hosting-fixture-dirs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("octo").join("app")).unwrap();
    std::fs::create_dir_all(dir.join("group").join("sub").join("app")).unwrap();
    let parent = at(Service::GitHub, "github.com", "octo", "app");
    assert_eq!(fixture_dir(&dir, &parent), dir.join("octo").join("app"));
    assert_eq!(fixture_dir(&dir, &at(Service::GitHub, "github.com", "ada", "app")), dir);
    let nested = at(Service::GitLab, "gitlab.com", "group/sub", "app");
    assert_eq!(fixture_dir(&dir, &nested), dir.join("group").join("sub").join("app"));
    let _ = std::fs::remove_dir_all(&dir);
}
