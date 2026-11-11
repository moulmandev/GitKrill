//! Preferences > Integrations: the hosting accounts GitKrill talks to.
//!
//! A column of services (GitHub, GitHub Enterprise Server, GitLab, GitLab
//! Self-Managed, Bitbucket, Bitbucket Data Center, Azure DevOps, Codeberg,
//! Gitea, Forgejo, SourceHut, with the servers connected under the
//! self-hosted ones) beside the one chosen: the sign-in GitKrill finds there
//! and where it came from, the account it belongs to, and that account's
//! SSH keys.
//!
//! Nothing is looked up before an account is shown, and nothing at all while
//! the hosting integration is off. Each account's lookups run on a thread of
//! its own ([`AccountSession`]) that ends when the page closes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui::{self, Align, Frame, Label, Layout, Margin, RichText, Sense, Stroke, TextEdit, Ui, vec2};

use super::{Page, form};
use crate::hosting::account::{self, AccountId, AccountSession, AccountView, SshKey, Trouble, Work};
use crate::hosting::{Offer, ServerKind, Service, TokenSource};
use crate::state::Dialog;
use crate::theme::{self, icons};
use crate::ui::widgets;

/// Width of the service column.
const COLUMN: f32 = 250.0;
/// Widest the account card gets.
const CARD: f32 = 560.0;

/// What the service column has chosen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Entry {
    #[default]
    GitHub,
    /// GitHub Enterprise servers: one of them, or (`None`) the list.
    Enterprise(Option<String>),
    GitLab,
    /// Self-managed GitLab servers: one of them, or (`None`) the list.
    SelfManaged(Option<String>),
    Bitbucket,
    /// Bitbucket Data Center servers: one of them, or (`None`) the list.
    DataCenter(Option<String>),
    Azure,
    Codeberg,
    /// Gitea servers: one of them, or (`None`) the list.
    Gitea(Option<String>),
    /// Forgejo servers: one of them, or (`None`) the list.
    Forgejo(Option<String>),
    SourceHut,
}

/// Servers the remotes of the open repositories point at, read once in the
/// background when a list of servers is first shown.
#[derive(Default)]
enum Found {
    #[default]
    NotRead,
    Reading(Receiver<Vec<Offer>>),
    Read(Vec<Offer>),
}

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    entry: Entry,
    /// Each account shown so far, by host: its lookups and what they found.
    accounts: BTreeMap<String, AccountSession>,
    found: Found,
    /// "Add a server": what is typed, and why it was turned down.
    new_server: String,
    /// The path a GitLab or Bitbucket Data Center is installed under, when
    /// it is not at the root.
    new_server_path: String,
    new_server_error: Option<String>,
    /// A server the connect dialog was opened for: chosen once it is added.
    awaiting: Option<(String, ServerKind)>,
    /// The title typed for a new key.
    title: String,
    /// The file "Add existing SSH key" sends; the default once it is known.
    key_path: String,
    /// The path was typed or picked: the default no longer replaces it.
    key_path_set: bool,
}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    form::intro(
        ui,
        "The accounts GitKrill signs in to for pull requests, issues, CI and SSH keys. It uses the sign-ins \
         that git and the services' own tools already have, and never stores a password or a token.",
    );
    let settings = &mut page.shared.settings;
    form::row(ui, "Hosting integration", |ui| {
        form::checkbox(ui, &mut settings.hosting_integration, "Pull requests, issues and CI from your hosting services");
        if !settings.hosting_integration {
            form::help(
                ui,
                "Integration is off: GitKrill sends nothing to any hosting service, so no account is shown here. \
                 Turn it on to see your accounts.",
            );
        }
    });
    if !settings.hosting_integration {
        // Ends every lookup under way.
        state.accounts.clear();
        return;
    }
    follow_settings(page, state);

    ui.add_space(6.0);
    ui.horizontal_top(|ui| {
        services(ui, page, state);
        ui.add_space(32.0);
        ui.vertical(|ui| detail(ui, page, state));
    });
}

/// Keeps the choice in step with the servers in the settings: a server
/// forgotten elsewhere gives way to its list, and one just connected from
/// here is chosen.
fn follow_settings(page: &Page<'_>, state: &mut State) {
    let servers = &page.shared.settings.hosting_servers;
    if let Some((host, kind)) = &state.awaiting {
        if servers.get(host) == Some(kind) {
            state.entry = entry_for(*kind, Some(host.clone()));
            state.awaiting = None;
        } else if page.shared.dialog.is_none() {
            state.awaiting = None;
        }
    }
    match &state.entry {
        Entry::Enterprise(Some(host)) if servers.get(host) != Some(&ServerKind::GitHubEnterprise) => {
            state.entry = Entry::Enterprise(None);
        }
        Entry::SelfManaged(Some(host)) if servers.get(host) != Some(&ServerKind::GitLab) => {
            state.entry = Entry::SelfManaged(None);
        }
        Entry::Gitea(Some(host)) if servers.get(host) != Some(&ServerKind::Gitea) => {
            state.entry = Entry::Gitea(None);
        }
        Entry::Forgejo(Some(host)) if servers.get(host) != Some(&ServerKind::Forgejo) => {
            state.entry = Entry::Forgejo(None);
        }
        Entry::DataCenter(Some(host)) if servers.get(host) != Some(&ServerKind::BitbucketDataCenter) => {
            state.entry = Entry::DataCenter(None);
        }
        _ => {}
    }
}

fn entry_for(kind: ServerKind, host: Option<String>) -> Entry {
    match kind {
        ServerKind::GitLab => Entry::SelfManaged(host),
        ServerKind::Gitea => Entry::Gitea(host),
        ServerKind::Forgejo => Entry::Forgejo(host),
        ServerKind::BitbucketDataCenter => Entry::DataCenter(host),
        _ => Entry::Enterprise(host),
    }
}

/// The servers of one kind in the settings, by name.
fn servers_of(page: &Page<'_>, kind: ServerKind) -> Vec<String> {
    let servers = &page.shared.settings.hosting_servers;
    servers.iter().filter(|(_, of)| **of == kind).map(|(host, _)| host.clone()).collect()
}

// ---- the service column -----------------------------------------------------

fn services(ui: &mut Ui, page: &Page<'_>, state: &mut State) {
    let enterprise = servers_of(page, ServerKind::GitHubEnterprise);
    let gitlab = servers_of(page, ServerKind::GitLab);
    let gitea = servers_of(page, ServerKind::Gitea);
    let forgejo = servers_of(page, ServerKind::Forgejo);
    let data_center = servers_of(page, ServerKind::BitbucketDataCenter);
    Frame::new()
        .fill(theme::bg_panel())
        .corner_radius(6.0)
        .inner_margin(Margin::symmetric(0, 6))
        .show(ui, |ui| {
            // Top to bottom, inside the page's left-to-right row.
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                column(ui, state, enterprise, gitlab);
                more_services(ui, state, data_center, gitea, forgejo);
            });
        });
}

/// Bitbucket and Bitbucket Data Center with its servers, Azure DevOps,
/// Codeberg, Gitea and Forgejo with their servers, then SourceHut.
fn more_services(ui: &mut Ui, state: &mut State, data_center: Vec<String>, gitea: Vec<String>, forgejo: Vec<String>) {
    service_row(ui, state, Entry::Bitbucket, icons::PAINT_BUCKET, "Bitbucket", false);
    service_row(ui, state, Entry::DataCenter(None), icons::PAINT_BUCKET, "Bitbucket Data Center", false);
    for host in data_center {
        service_row(ui, state, Entry::DataCenter(Some(host.clone())), icons::HARD_DRIVES, &host, true);
    }
    service_row(ui, state, Entry::Azure, icons::INFINITY, "Azure DevOps", false);
    service_row(ui, state, Entry::Codeberg, icons::MOUNTAINS, "Codeberg", false);
    service_row(ui, state, Entry::Gitea(None), icons::COFFEE, "Gitea", false);
    for host in gitea {
        service_row(ui, state, Entry::Gitea(Some(host.clone())), icons::HARD_DRIVES, &host, true);
    }
    service_row(ui, state, Entry::Forgejo(None), icons::HAMMER, "Forgejo", false);
    for host in forgejo {
        service_row(ui, state, Entry::Forgejo(Some(host.clone())), icons::HARD_DRIVES, &host, true);
    }
    service_row(ui, state, Entry::SourceHut, icons::CIRCLE, "SourceHut", false);
}

/// The services, each followed by its servers.
fn column(ui: &mut Ui, state: &mut State, enterprise: Vec<String>, gitlab: Vec<String>) {
    ui.set_width(COLUMN);
    ui.spacing_mut().item_spacing.y = 0.0;
    service_row(ui, state, Entry::GitHub, icons::GITHUB_LOGO, "GitHub", false);
    service_row(ui, state, Entry::Enterprise(None), icons::GITHUB_LOGO, "GitHub Enterprise Server", false);
    for host in enterprise {
        service_row(ui, state, Entry::Enterprise(Some(host.clone())), icons::HARD_DRIVES, &host, true);
    }
    service_row(ui, state, Entry::GitLab, icons::GITLAB_LOGO, "GitLab", false);
    service_row(ui, state, Entry::SelfManaged(None), icons::GITLAB_LOGO, "GitLab Self-Managed", false);
    for host in gitlab {
        service_row(ui, state, Entry::SelfManaged(Some(host.clone())), icons::HARD_DRIVES, &host, true);
    }
}

/// One row of the column; `server` rows sit under their service, smaller.
fn service_row(ui: &mut Ui, state: &mut State, entry: Entry, icon: &str, label: &str, server: bool) {
    let height = if server { 32.0 } else { 40.0 };
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let selected = state.entry == entry;
    let painter = ui.painter();
    widgets::row_background(painter, rect, selected, response.hovered());
    let size = if server { 13.5 } else { 15.0 };
    let color = if selected || response.hovered() { theme::text_bright() } else { theme::text() };
    let icon_color = if selected { theme::accent() } else { theme::text_dim() };
    let x = rect.left() + if server { 44.0 } else { 18.0 };
    let icon_width = widgets::text_left(painter, rect, x, icon, theme::font(size + 1.5), icon_color);
    let text_x = x + icon_width + 10.0;
    let galley = widgets::elided(painter, label, theme::font(size), color, rect.right() - text_x - 10.0);
    widgets::galley_left(painter, rect, text_x, galley);
    let response = if server { response.on_hover_text(label) } else { response };
    if response.clicked() {
        state.entry = entry;
    }
}

// ---- the chosen entry -------------------------------------------------------

fn detail(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    match state.entry.clone() {
        Entry::GitHub => {
            title(ui, "GitHub", None);
            account_rows(ui, page, state, AccountId::github());
        }
        Entry::GitLab => {
            title(ui, "GitLab", None);
            account_rows(ui, page, state, AccountId::gitlab());
        }
        Entry::Enterprise(None) => server_list(ui, page, state, ServerKind::GitHubEnterprise),
        Entry::SelfManaged(None) => server_list(ui, page, state, ServerKind::GitLab),
        Entry::Enterprise(Some(host)) => {
            server_page(ui, page, state, AccountId { service: Service::GitHubEnterprise, host });
        }
        Entry::SelfManaged(Some(host)) => server_page(ui, page, state, AccountId { service: Service::GitLab, host }),
        Entry::Bitbucket => {
            title(ui, "Bitbucket", Some("Bitbucket Cloud, on bitbucket.org."));
            account_rows(ui, page, state, AccountId::bitbucket());
        }
        Entry::Azure => {
            title(ui, "Azure DevOps", Some("Azure Repos, Pipelines and Boards, on dev.azure.com."));
            account_rows(ui, page, state, AccountId::azure());
        }
        Entry::Codeberg => {
            title(ui, "Codeberg", None);
            account_rows(ui, page, state, AccountId::codeberg());
        }
        Entry::Gitea(None) => server_list(ui, page, state, ServerKind::Gitea),
        Entry::Forgejo(None) => server_list(ui, page, state, ServerKind::Forgejo),
        Entry::Gitea(Some(host)) => server_page(ui, page, state, AccountId { service: Service::Gitea, host }),
        Entry::Forgejo(Some(host)) => server_page(ui, page, state, AccountId { service: Service::Forgejo, host }),
        Entry::DataCenter(None) => server_list(ui, page, state, ServerKind::BitbucketDataCenter),
        Entry::DataCenter(Some(host)) => {
            server_page(ui, page, state, AccountId { service: Service::BitbucketDataCenter, host });
        }
        Entry::SourceHut => {
            title(ui, "SourceHut", Some("git.sr.ht, with its tickets on todo.sr.ht and builds on builds.sr.ht."));
            account_rows(ui, page, state, AccountId::sourcehut());
        }
    }
}

fn title(ui: &mut Ui, text: &str, under: Option<&str>) {
    ui.label(RichText::new(text).font(theme::bold(19.0)).color(theme::text_bright()));
    if let Some(under) = under {
        ui.add(Label::new(RichText::new(under).size(12.0).color(theme::text_dim())).wrap());
    }
    ui.add_space(14.0);
}

/// What the account card asked for.
enum CardAct {
    Disconnect,
    Connect,
    CheckAgain,
}

/// The account card, then (once the account is known) its SSH keys.
fn account_rows(ui: &mut Ui, page: &mut Page<'_>, state: &mut State, account: AccountId) {
    let host = account.host.clone();
    if page.shared.settings.hosting_disconnected.contains(&host) {
        // Nothing is looked up for a disconnected account.
        state.accounts.remove(&host);
        let act = form::row(ui, "Account", |ui| disconnected_card(ui, &account));
        if let Some(CardAct::Connect) = act {
            page.shared.settings.hosting_disconnected.remove(&host);
        }
        return;
    }

    let State { accounts, title, key_path, key_path_set, .. } = state;
    if accounts.get(&host).is_some_and(|session| session.account != account) {
        // The server was said to run something else since.
        accounts.remove(&host);
    }
    let session = accounts.entry(host.clone()).or_insert_with(|| AccountSession::start(account.clone(), page.ctx));
    session.poll();

    let act = form::row(ui, "Account", |ui| card(ui, page, session));
    match act {
        Some(CardAct::Disconnect) => {
            page.shared.settings.hosting_disconnected.insert(host.clone());
            accounts.remove(&host);
            return;
        }
        Some(CardAct::CheckAgain) => session.check(),
        Some(CardAct::Connect) | None => {}
    }
    if matches!(session.view.profile, Some(Ok(_))) {
        if account.keys_supported() {
            key_rows(ui, session, title, key_path, key_path_set);
        } else {
            form::row(ui, "SSH Keys", |ui| {
                form::help(
                    ui,
                    "Azure DevOps has no API for SSH keys. Add yours in User settings > SSH public keys \
                     on dev.azure.com; Preferences > SSH can generate a key and copy its public half.",
                );
            });
        }
    }
}

/// The frame of the account card, as wide as the column allows.
fn card_frame<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(theme::bg_panel())
        .stroke(Stroke::new(1.0, theme::border()))
        .corner_radius(6.0)
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width().min(CARD) - 28.0);
            ui.horizontal(|ui| {
                ui.set_min_height(44.0);
                add(ui)
            })
            .inner
        })
        .inner
}

/// The picture's place on the card: 44 points square.
fn picture_slot(ui: &mut Ui) -> egui::Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
    ui.add_space(12.0);
    rect
}

fn icon_slot(ui: &mut Ui, icon: &str, color: egui::Color32) {
    let rect = picture_slot(ui);
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, theme::font(34.0), color);
}

/// The two lines beside the picture, leaving `room` for what goes at the
/// card's right end.
fn names(ui: &mut Ui, first: &str, second: &str, room: f32) {
    let width = (ui.available_width() - room).max(100.0);
    ui.vertical(|ui| {
        ui.set_max_width(width);
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.add(Label::new(RichText::new(first).size(15.0).color(theme::text_bright())).truncate());
        ui.add(Label::new(RichText::new(second).size(13.0).color(theme::text_dim())).truncate());
    });
}

/// The card's right end: buttons and the status, right to left.
fn right<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.with_layout(Layout::right_to_left(Align::Center), add).inner
}

/// "Check again", or a spinner while a check is under way.
fn check_again(ui: &mut Ui, checking: bool) -> bool {
    if checking {
        ui.add(egui::Spinner::new().size(16.0).color(theme::accent()));
        return false;
    }
    form::button(ui, "Check again", theme::accent()).clicked()
}

fn card(ui: &mut Ui, page: &mut Page<'_>, session: &AccountSession) -> Option<CardAct> {
    let account = &session.account;
    let view = &session.view;
    let mut act = None;
    card_frame(ui, |ui| match &view.profile {
        None => {
            let rect = picture_slot(ui);
            widgets::spinner(ui.painter(), rect.center(), 13.0, theme::accent());
            ui.ctx().request_repaint();
            names(ui, "Looking for a sign-in…", &account.host, 0.0);
        }
        Some(Ok(profile)) => {
            let rect = picture_slot(ui);
            let email = profile.avatar_email(account);
            let settings = &page.shared.settings;
            let picture = email.as_deref().and_then(|email| page.shared.avatars.get(email, settings.fetch_avatars));
            let key = email.as_deref().unwrap_or(&profile.login);
            widgets::avatar_picture(ui.painter(), picture, rect.center(), 21.0, profile.display_name(), key, None);
            names(ui, profile.display_name(), &profile.login, 240.0);
            right(ui, |ui| {
                if form::button(ui, "Disconnect", theme::red()).clicked() {
                    act = Some(CardAct::Disconnect);
                }
                ui.add_space(18.0);
                let status = format!("{}  Connected", icons::CHECK_CIRCLE);
                ui.label(RichText::new(status).size(15.0).color(theme::green()));
            });
        }
        Some(Err(Trouble::NoToken)) => {
            icon_slot(ui, icons::USER_CIRCLE, theme::text_faint());
            names(ui, "Not signed in", &format!("No sign-in found for {}", account.host), 100.0);
            right(ui, |ui| {
                if check_again(ui, view.checking) {
                    act = Some(CardAct::CheckAgain);
                }
            });
        }
        Some(Err(Trouble::Rejected)) => {
            icon_slot(ui, icons::WARNING_CIRCLE, theme::red());
            names(ui, "Sign-in refused", "Expired or revoked?", 100.0);
            right(ui, |ui| {
                if check_again(ui, view.checking) {
                    act = Some(CardAct::CheckAgain);
                }
            });
        }
        Some(Err(_)) => {
            icon_slot(ui, icons::WARNING_CIRCLE, theme::orange());
            let second = match view.source {
                Some(source) => format!("Signed in through {}", source_words(account, source)),
                None => account.host.clone(),
            };
            names(ui, "Account not read", &second, 190.0);
            right(ui, |ui| {
                if view.source.is_some() && form::button(ui, "Disconnect", theme::red()).clicked() {
                    act = Some(CardAct::Disconnect);
                }
                ui.add_space(8.0);
                if check_again(ui, view.checking) {
                    act = Some(CardAct::CheckAgain);
                }
            });
        }
    });

    // Under the card: where the sign-in came from, or how to get one.
    match &view.profile {
        None => {}
        Some(Ok(_)) => {
            let from = view.source.map(|source| source_words(account, source)).unwrap_or_default();
            form::help(
                ui,
                &format!(
                    "Signed in through {from}. Disconnect makes GitKrill stop using this sign-in, but git and \
                     other tools keep it, so pushing and fetching still work."
                ),
            );
        }
        Some(Err(Trouble::NoToken | Trouble::Rejected)) => form::help(ui, &sign_in_help(account)),
        Some(Err(trouble)) => form::error(ui, &trouble.explain(account, view.source, "read your account")),
    }
    act
}

fn disconnected_card(ui: &mut Ui, account: &AccountId) -> Option<CardAct> {
    let mut act = None;
    card_frame(ui, |ui| {
        icon_slot(ui, icons::LINK_BREAK, theme::text_dim());
        names(ui, "Disconnected", &format!("GitKrill does not use your {} sign-in", account.host), 80.0);
        right(ui, |ui| {
            if form::button(ui, "Connect", theme::green()).clicked() {
                act = Some(CardAct::Connect);
            }
        });
    });
    form::help(
        ui,
        &format!(
            "The sidebar shows {} as it looks to someone signed out. Your sign-in itself is untouched: Connect \
             makes GitKrill use it again.",
            account.site()
        ),
    );
    act
}

/// Where a sign-in came from, in words.
fn source_words(account: &AccountId, source: TokenSource) -> String {
    match source {
        TokenSource::Env(name) => format!("${name}"),
        TokenSource::Cli if account.service.is_gitlab() => "the GitLab CLI (glab)".to_owned(),
        TokenSource::Cli if account.service.is_gitea() => "the Gitea CLI (tea)".to_owned(),
        TokenSource::Cli if account.service == Service::AzureDevOps => "the Azure CLI (az)".to_owned(),
        TokenSource::Cli if account.service == Service::SourceHut => "hut's sign-in".to_owned(),
        TokenSource::Cli => "the GitHub CLI (gh)".to_owned(),
        TokenSource::CredentialHelper => "Git's credential manager".to_owned(),
    }
}

/// What GitKrill looks for, and how to give it one.
fn sign_in_help(account: &AccountId) -> String {
    let host = &account.host;
    let (variables, tool) = match account.service {
        Service::GitHub => ("$GITHUB_TOKEN or $GH_TOKEN".to_owned(), "the GitHub CLI (run `gh auth login`)".to_owned()),
        Service::GitHubEnterprise => (
            "$GH_ENTERPRISE_TOKEN".to_owned(),
            format!("the GitHub CLI (run `gh auth login --hostname {host}`)"),
        ),
        Service::GitLab if !account.self_hosted() => {
            ("$GITLAB_TOKEN".to_owned(), "the GitLab CLI (run `glab auth login`)".to_owned())
        }
        Service::GitLab => (
            format!("$GITLAB_TOKEN while $GITLAB_HOST is {host}"),
            format!("the GitLab CLI (run `glab auth login --hostname {host}`)"),
        ),
        Service::Bitbucket => {
            return "GitKrill looks for a sign-in for bitbucket.org in $BITBUCKET_ACCESS_TOKEN (a repository or \
                    workspace access token), then $BITBUCKET_API_TOKEN with $BITBUCKET_EMAIL (an Atlassian API \
                    token with Bitbucket scopes; reading your account needs read:user, your keys \
                    read:ssh-key), then Git's credential manager, which keeps one after a push or fetch over \
                    HTTPS. Sign in one of these ways, then click Check again."
                .to_owned();
        }
        Service::AzureDevOps => {
            return "GitKrill looks for a sign-in for Azure DevOps in $AZURE_DEVOPS_EXT_PAT (a personal access \
                    token; reading your profile needs its User Profile scope), then the Azure CLI (run `az \
                    login`), then Git's credential manager. The credential manager signs in per organisation, \
                    so its sign-in is used in the sidebar of each repository but cannot be shown here. Sign in \
                    one of these ways, then click Check again."
                .to_owned();
        }
        Service::BitbucketDataCenter => {
            return format!(
                "GitKrill looks for a sign-in for {host} in $BITBUCKET_SERVER_TOKEN while $BITBUCKET_SERVER_HOST \
                 is {host} (an HTTP access token), then Git's credential manager, which keeps one after a push \
                 or fetch over HTTPS. Sign in one of these ways, then click Check again."
            );
        }
        Service::SourceHut => {
            return "GitKrill looks for a sign-in for SourceHut in $SRHT_TOKEN, then hut's sign-in (run `hut \
                    init`), then Git's credential manager. Use a personal access token from meta.sr.ht > OAuth \
                    2.0: reading your account needs its PROFILE grant, your keys SSH_KEYS. Sign in one of these \
                    ways, then click Check again."
                .to_owned();
        }
        Service::Codeberg => ("$CODEBERG_TOKEN".to_owned(), "tea's sign-in (run `tea login add`)".to_owned()),
        Service::Gitea => (
            format!("$GITEA_TOKEN while $GITEA_HOST is {host}"),
            format!("tea's sign-in (run `tea login add --url https://{host}`)"),
        ),
        Service::Forgejo => (
            format!("$FORGEJO_TOKEN while $FORGEJO_HOST is {host}"),
            format!("tea's sign-in (run `tea login add --url https://{host}`)"),
        ),
    };
    format!(
        "GitKrill looks for a sign-in for {host} in {variables}, then {tool}, then Git's credential manager, \
         which keeps one after a push or fetch over HTTPS. Sign in one of these ways, then click Check again."
    )
}

// ---- SSH keys ---------------------------------------------------------------

fn key_rows(ui: &mut Ui, session: &mut AccountSession, title: &mut String, key_path: &mut String, key_path_set: &mut bool) {
    let account = session.account.clone();
    let site = account.site();
    let view = &session.view;

    let mut recheck = false;
    form::row(ui, "SSH Keys", |ui| match &view.keys {
        None => form::loading(ui),
        Some(Ok(keys)) if keys.is_empty() => form::help(ui, &format!("No SSH keys on this {site} account yet.")),
        Some(Ok(keys)) => {
            // The ones this computer has first, then the newest.
            let mut keys: Vec<&SshKey> = keys.iter().collect();
            keys.sort_by_key(|key| (view.local_file(key).is_none(), -key.created));
            for key in keys {
                key_entry(ui, view, key);
            }
        }
        Some(Err(trouble)) => {
            form::error(ui, &trouble.explain(&account, view.source, "read your SSH keys"));
            // Once the sign-in has the scope, reading needs asking again.
            recheck = check_again(ui, view.checking || view.keys_loading);
        }
    });

    let busy = session.busy();
    let mut generate = false;
    form::row(ui, "Generate SSH Key", |ui| {
        form::text(ui, title, "(optional) Enter a title for your new SSH key");
        ui.add_enabled_ui(!busy, |ui| {
            generate = form::button(ui, &format!("Generate SSH key and add to {site}"), theme::green()).clicked();
        });
        work_line(ui, &account, &session.view, Work::Generating);
        form::help(
            ui,
            "Makes a new Ed25519 key without a passphrase in your .ssh folder (named id_ed25519 if you have \
             none yet) and adds its public half to your account.",
        );
    });

    if !*key_path_set
        && key_path.is_empty()
        && let Some(default) = session.view.default_key()
    {
        *key_path = default.display().to_string();
    }
    let mut add = false;
    form::row(ui, "Add existing SSH key", |ui| {
        if form::path(ui, key_path, "~/.ssh/id_ed25519.pub", form::Pick::File, "Choose a public SSH key") {
            *key_path_set = true;
        }
        let ready = !busy && !key_path.trim().is_empty();
        ui.add_enabled_ui(ready, |ui| {
            add = form::button(ui, &format!("Add public key to {site}"), theme::accent()).clicked();
        });
        work_line(ui, &account, &session.view, Work::Adding);
        form::help(
            ui,
            "Sends the public key (.pub), titled as typed above or after the key's comment. A private key is \
             never sent: choosing one sends the .pub beside it.",
        );
    });

    if recheck {
        session.check();
    } else if generate {
        session.generate(title);
    } else if add {
        session.add(title, PathBuf::from(key_path.trim()));
    }
}

fn key_entry(ui: &mut Ui, view: &AccountView, key: &SshKey) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(icons::KEY).size(14.0).color(theme::text_dim()));
        let mut when = Vec::new();
        if key.created > 0 {
            when.push(format!("added {}", date(key.created)));
        }
        if key.expires > 0 {
            when.push(format!("expires {}", date(key.expires)));
        }
        let title = if key.title.is_empty() { "Untitled key" } else { key.title.as_str() };
        ui.label(RichText::new(title).size(13.5).color(theme::text_bright()));
        if !when.is_empty() {
            ui.label(RichText::new(when.join(", ")).size(12.0).color(theme::text_dim()));
        }
    });
    let indented = |ui: &mut Ui, text: RichText| {
        ui.horizontal(|ui| {
            ui.add_space(22.0);
            ui.add(Label::new(text).truncate());
        });
    };
    if !key.fingerprint.is_empty() {
        indented(ui, RichText::new(&key.fingerprint).font(theme::mono(12.0)).color(theme::text_dim()));
    }
    if let Some(path) = view.local_file(key) {
        let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let text = format!("{}  On this computer: {name}", icons::DESKTOP);
        indented(ui, RichText::new(text).size(12.0).color(theme::green()));
    }
    ui.add_space(6.0);
}

/// Under a key button: the upload it started, while it runs and once done.
fn work_line(ui: &mut Ui, account: &AccountId, view: &AccountView, work: Work) {
    if view.working == Some(work) {
        let text = match work {
            Work::Generating => "Generating a key and adding it…",
            Work::Adding => "Adding the key…",
        };
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(13.0).color(theme::accent()));
            ui.label(RichText::new(text).size(12.0).color(theme::text_dim()));
        });
        return;
    }
    let Some(done) = view.done.as_ref().filter(|done| done.work == work) else { return };
    match &done.result {
        Ok(key) => {
            success(ui, &format!("Added \u{201c}{}\u{201d} to {}.", key.title, account.site()));
            if let Some(path) = &done.generated {
                form::help(ui, &generated_help(path));
            }
        }
        Err(trouble) => {
            form::error(ui, &trouble.explain(account, view.source, "add SSH keys"));
            if let Some(path) = &done.generated {
                form::help(ui, &format!("The key itself was made: {}.", path.display()));
            }
        }
    }
}

/// Where a generated key went, and whether ssh finds it by itself.
fn generated_help(path: &Path) -> String {
    let default = path.file_name().is_some_and(|name| name == "id_ed25519");
    if default {
        format!("The new key is {}; ssh uses it without further setup.", path.display())
    } else {
        format!(
            "The new key is {}. ssh only uses it once told to: choose it in Preferences > SSH, or name it in \
             ~/.ssh/config.",
            path.display()
        )
    }
}

/// A green tick and a dim sentence, like [`form::error`] for good news.
fn success(ui: &mut Ui, text: &str) {
    ui.scope(|ui| {
        ui.set_max_width(form::HELP_WIDTH);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(RichText::new(icons::CHECK_CIRCLE).size(13.0).color(theme::green()));
            ui.add(Label::new(RichText::new(text).size(12.0).color(theme::text_dim())).wrap());
        });
    });
}

/// `Feb 23, 2025` in local time.
fn date(epoch: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|time| crate::prefs::appearance::custom_date(&time, false).unwrap_or_else(|| time.format("%b %-d, %Y").to_string()))
        .unwrap_or_default()
}

// ---- self-hosted servers ----------------------------------------------------

fn kind_name(kind: ServerKind) -> &'static str {
    match kind {
        ServerKind::GitLab => "GitLab Self-Managed",
        ServerKind::Gitea => "Gitea",
        ServerKind::Forgejo => "Forgejo",
        ServerKind::BitbucketDataCenter => "Bitbucket Data Center",
        _ => "GitHub Enterprise Server",
    }
}

/// The servers of one kind: those connected, those the open repositories
/// point at, and a field to add one.
fn server_list(ui: &mut Ui, page: &mut Page<'_>, state: &mut State, kind: ServerKind) {
    title(
        ui,
        kind_name(kind),
        Some("Servers you connected. GitKrill sends nothing to a server until you have said what runs on it."),
    );
    let connected = servers_of(page, kind);
    let mut open = None;
    let mut forget = None;
    form::row(ui, "Servers", |ui| {
        if connected.is_empty() {
            form::help(ui, "None yet: connect one found in your repositories, or add one below.");
        }
        for host in &connected {
            ui.horizontal(|ui| {
                let text = format!("{}  {host}", icons::HARD_DRIVES);
                let label = Label::new(RichText::new(text).size(13.5).color(theme::text_bright())).sense(Sense::click());
                if ui.add(label).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    open = Some(host.clone());
                }
                if page.shared.settings.hosting_disconnected.contains(host) {
                    ui.label(RichText::new("disconnected").size(12.0).color(theme::text_faint()));
                }
                right(ui, |ui| {
                    if form::button(ui, "Forget", theme::red()).on_hover_text(forget_help(host)).clicked() {
                        forget = Some(host.clone());
                    }
                    if form::button(ui, "Account", theme::accent()).clicked() {
                        open = Some(host.clone());
                    }
                });
            });
        }
    });

    let mut connect = None;
    form::row(ui, "Found in your repositories", |ui| match found(page, &mut state.found) {
        None => form::loading(ui),
        Some(offers) => {
            let servers = &page.shared.settings.hosting_servers;
            // A server connected under a path counts for its host too.
            let rooted = |host: &str| servers.keys().any(|key| key.split_once('/').is_some_and(|(name, _)| name == host));
            let fitting: Vec<&Offer> = offers
                .iter()
                .filter(|offer| {
                    !servers.contains_key(&offer.host)
                        && !rooted(&offer.host)
                        && offer.guess.is_none_or(|guess| guess == kind)
                })
                .collect();
            if fitting.is_empty() {
                form::help(ui, "No other servers in the remotes of your open repositories.");
            }
            for offer in fitting {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icons::HARD_DRIVES, offer.host)).size(13.5).color(theme::text()));
                    ui.add(Label::new(RichText::new(&offer.path).size(12.0).color(theme::text_dim())).truncate());
                    right(ui, |ui| {
                        if form::button(ui, "Connect…", theme::accent()).clicked() {
                            connect = Some((offer.host.clone(), offer.path.clone()));
                        }
                    });
                });
            }
        }
    });

    form::row(ui, "Add a server", |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let field = form::text(ui, &mut state.new_server, "git.example.com");
            if field.changed() {
                state.new_server_error = None;
            }
            let mut entered = field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            // GitLab and Bitbucket Data Center can be installed under a path
            // (a relative URL root, a context path); the others cannot.
            let rooted = matches!(kind, ServerKind::GitLab | ServerKind::BitbucketDataCenter);
            if rooted {
                ui.label(RichText::new("/").size(14.0).color(theme::text_dim()));
                let path = TextEdit::singleline(&mut state.new_server_path)
                    .hint_text("path")
                    .desired_width(110.0)
                    .margin(vec2(8.0, 5.0));
                let tip = match kind {
                    ServerKind::GitLab => "Only when GitLab is not at the root of the server: gitlab for example.com/gitlab",
                    _ => "Only when Bitbucket is not at the root of the server: bitbucket for example.com/bitbucket",
                };
                let path = ui.add(path).on_hover_text(tip);
                if path.changed() {
                    state.new_server_error = None;
                }
                entered |= path.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            }
            let clicked = form::button(ui, "Connect…", theme::accent()).clicked();
            if (entered || clicked) && !state.new_server.trim().is_empty() {
                let path = if rooted { state.new_server_path.as_str() } else { "" };
                match account::server_host(&state.new_server).and_then(|host| account::under_path(&host, path)) {
                    Ok(host) => {
                        connect = Some((host, String::new()));
                        state.new_server.clear();
                        state.new_server_path.clear();
                    }
                    Err(error) => state.new_server_error = Some(error),
                }
            }
        });
        if let Some(error) = &state.new_server_error {
            form::error(ui, error);
        }
        let help = match kind {
            ServerKind::GitLab => {
                "Its name, as in your remotes' addresses, and the path GitLab is installed under when it is not \
                 at the root (gitlab for example.com/gitlab). You confirm what runs there before anything is sent to it."
            }
            ServerKind::BitbucketDataCenter => {
                "Its name, as in your remotes' addresses, and the path Bitbucket is installed under when it is \
                 not at the root (what comes before /scm/ in its HTTPS addresses). You confirm what runs there \
                 before anything is sent to it."
            }
            _ => "Its name, as in your remotes' addresses. You confirm what runs there before anything is sent to it.",
        };
        form::help(ui, help);
    });

    if let Some(host) = open {
        state.entry = entry_for(kind, Some(host));
    }
    if let Some(host) = forget {
        forget_server(page, state, &host);
    }
    if let Some((host, path)) = connect {
        // The name it is kept under, which may take the path in front of
        // a Bitbucket Data Center's `/scm/`.
        state.awaiting = Some((crate::hosting::server_key(kind, &host, &path), kind));
        page.shared.show(Dialog::ConnectServer { host, path, choice: Some(kind) });
    }
}

/// One server's account, and a way to forget the server.
fn server_page(ui: &mut Ui, page: &mut Page<'_>, state: &mut State, account: AccountId) {
    let kind = ServerKind::of(account.service).unwrap_or(ServerKind::GitHubEnterprise);
    let under = format!("{}. Requests go to {} only.", kind_name(kind), account.api());
    title(ui, &account.host, Some(&under));
    let host = account.host.clone();
    account_rows(ui, page, state, account);
    ui.add_space(8.0);
    let mut forget = false;
    form::row(ui, "Server", |ui| {
        forget = form::button(ui, "Forget this server", theme::red()).clicked();
        form::help(ui, &forget_help(&host));
    });
    if forget {
        forget_server(page, state, &host);
    }
}

fn forget_help(host: &str) -> String {
    format!(
        "GitKrill stops contacting {host}: its repositories show no pull requests, issues or CI until you \
         connect it again."
    )
}

fn forget_server(page: &mut Page<'_>, state: &mut State, host: &str) {
    let settings = &mut page.shared.settings;
    if let Some(kind) = settings.hosting_servers.remove(host) {
        state.entry = entry_for(kind, None);
    }
    settings.hosting_disconnected.remove(host);
    state.accounts.remove(host);
}

/// The servers found in the open repositories, once read. The first call
/// starts reading them in the background.
fn found<'a>(page: &Page<'_>, found: &'a mut Found) -> Option<&'a [Offer]> {
    if matches!(found, Found::NotRead) {
        let paths = page.shared.settings.tabs.clone();
        let ctx = page.ctx.clone();
        let (sender, receiver) = channel();
        let spawned = std::thread::Builder::new().name("found-servers".into()).spawn(move || {
            let _ = sender.send(account::found_servers(&paths));
            ctx.request_repaint();
        });
        *found = if spawned.is_ok() { Found::Reading(receiver) } else { Found::Read(Vec::new()) };
    }
    if let Found::Reading(receiver) = found {
        match receiver.try_recv() {
            Ok(offers) => *found = Found::Read(offers),
            Err(TryRecvError::Disconnected) => *found = Found::Read(Vec::new()),
            Err(TryRecvError::Empty) => {}
        }
    }
    match found {
        Found::Read(offers) => Some(offers.as_slice()),
        _ => None,
    }
}
