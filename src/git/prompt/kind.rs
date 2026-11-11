//! What a prompt from git, ssh or gpg is asking, read from its text, so the
//! dialog can say it in its own words and pick the right field.
//!
//! The texts are the programs' own: git asks `Username for
//! 'https://example.com': ` and `Password for 'https://ada@example.com': `,
//! OpenSSH `Enter passphrase for key '/home/ada/.ssh/id_ed25519': `,
//! `ada@example.com's password: ` and the host key question ending in
//! `(yes/no/[fingerprint])? `. Anything not recognised is shown as it is.

use serde::{Deserialize, Serialize};

/// What the GPG wrapper (`super::gpg`) says about the key it needs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Signing {
    /// The key git signs with (`user.signingkey`): an id, a fingerprint or
    /// an email address.
    pub key: String,
    /// The key's user id (`Ada Lovelace <ada@example.com>`), when gpg said.
    pub user: Option<String>,
    /// The passphrase given before was not the right one.
    pub retry: bool,
}

/// The kinds of question there are.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// An HTTPS remote's user name; git asks for the password next.
    Username { url: String },
    /// An HTTPS remote's password, for `user` when the URL names one.
    Password { url: String, user: Option<String> },
    /// The passphrase of an SSH key (`key` is its file, when named).
    KeyPassphrase { key: Option<String> },
    /// An SSH server's own password login (`ada@example.com`).
    SshPassword { login: String },
    /// OpenSSH meets a host it does not know and asks whether to trust it.
    HostKey { host: Option<String> },
    /// A yes / no question (`SSH_ASKPASS_PROMPT=confirm`: a key that asks
    /// before each use).
    Confirm,
    /// Something to read while ssh waits (`SSH_ASKPASS_PROMPT=none`: touch
    /// the security key). There is nothing to answer.
    Notice,
    /// The passphrase (or card PIN) of the OpenPGP key a commit or tag is
    /// signed with.
    Signing(Signing),
    /// Anything else: shown as it is, answered in a masked field.
    Other,
}

/// One question, as the dialog shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub kind: Kind,
    /// The text the program asked with, trimmed.
    pub prompt: String,
}

impl Question {
    /// Reads `prompt`. `hint` is `SSH_ASKPASS_PROMPT` as ssh set it
    /// (`confirm`, `none`, or empty); `signing` comes from the GPG wrapper.
    pub fn parse(prompt: &str, hint: &str, signing: Option<Signing>) -> Question {
        let text = prompt.trim().to_owned();
        let kind = match (signing, hint) {
            (Some(signing), _) => Kind::Signing(signing),
            (None, "confirm") => Kind::Confirm,
            (None, "none") => Kind::Notice,
            (None, _) => classify(&text),
        };
        Question { kind, prompt: text }
    }

    /// The dialog's title.
    pub fn title(&self) -> String {
        match &self.kind {
            Kind::Username { url } | Kind::Password { url, .. } => format!("Sign in to {}", host_of(url)),
            Kind::KeyPassphrase { .. } => "Unlock SSH key".to_owned(),
            Kind::SshPassword { login } => format!("Password for {login}"),
            Kind::HostKey { host: Some(host) } => format!("Trust {host}?"),
            Kind::HostKey { host: None } => "Trust this host?".to_owned(),
            Kind::Confirm => "Allow?".to_owned(),
            Kind::Notice => "Waiting for your key".to_owned(),
            Kind::Signing(_) => "Unlock signing key".to_owned(),
            Kind::Other => "Git needs an answer".to_owned(),
        }
    }

    /// What the command that asked fails with once the user said no, in
    /// place of git's own words (which talk of a terminal).
    pub fn cancelled(&self) -> String {
        match &self.kind {
            Kind::Username { url } | Kind::Password { url, .. } => {
                format!("Signing in to {} was cancelled.", host_of(url))
            }
            Kind::KeyPassphrase { key: Some(key) } => format!("Unlocking the SSH key {key} was cancelled."),
            Kind::KeyPassphrase { key: None } => "Unlocking the SSH key was cancelled.".to_owned(),
            Kind::SshPassword { login } => format!("Signing in as {login} was cancelled."),
            Kind::HostKey { host: Some(host) } => {
                format!("{host} was not trusted, so GitKrill did not connect to it.")
            }
            Kind::HostKey { host: None } => "The host was not trusted, so GitKrill did not connect to it.".to_owned(),
            Kind::Signing(signing) => format!(
                "Signing was cancelled: the passphrase of the key {} was not given.",
                signing.user.as_deref().unwrap_or(&signing.key)
            ),
            Kind::Confirm | Kind::Notice | Kind::Other => "Cancelled at GitKrill's prompt.".to_owned(),
        }
    }
}

fn classify(text: &str) -> Kind {
    let lower = text.to_lowercase();
    // git: "Username for 'https://example.com': "
    if let Some(url) = text.strip_prefix("Username for ").and_then(quoted) {
        return Kind::Username { url: url.to_owned() };
    }
    // git: "Password for 'https://ada@example.com': "
    if let Some(url) = text.strip_prefix("Password for ").and_then(quoted) {
        let user = user_of(url);
        return Kind::Password { url: url.to_owned(), user };
    }
    // OpenSSH, before its fingerprint question: "Are you sure you want to
    // continue connecting (yes/no/[fingerprint])?"
    if lower.contains("continue connecting") && lower.contains("(yes/no") {
        let host = text
            .split_once("authenticity of host '")
            .and_then(|(_, rest)| rest.split_once('\''))
            .map(|(host, _)| host.split_once(" (").map_or(host, |(name, _)| name).to_owned());
        return Kind::HostKey { host };
    }
    // ssh: "Enter passphrase for key '/home/ada/.ssh/id_ed25519': ";
    // ssh-keygen (signing): `Enter passphrase for "/home/ada/.ssh/key": `
    // or plain "Enter passphrase: ".
    if lower.starts_with("enter passphrase") {
        let key = quoted(text.split_once(" for ").map_or("", |(_, rest)| rest.trim_start_matches("key ")))
            .map(str::to_owned);
        return Kind::KeyPassphrase { key };
    }
    // ssh password logins: "ada@example.com's password: " (older), or
    // "(ada@example.com) Password: " (keyboard-interactive).
    if let Some(login) = text.strip_suffix("'s password:") {
        return Kind::SshPassword { login: login.to_owned() };
    }
    if let Some(rest) = text.strip_prefix('(')
        && let Some((login, after)) = rest.split_once(')')
        && after.trim().eq_ignore_ascii_case("password:")
    {
        return Kind::SshPassword { login: login.to_owned() };
    }
    Kind::Other
}

/// The text between the first pair of single or double quotes.
fn quoted(text: &str) -> Option<&str> {
    let start = text.find(['\'', '"'])?;
    let quote = text[start..].chars().next()?;
    let rest = &text[start + 1..];
    rest.find(quote).map(|end| &rest[..end])
}

/// The host of a URL (`https://ada@example.com:8443/x` gives
/// `example.com:8443`), or the text itself when it is not one.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    rest.rsplit_once('@').map_or(rest, |(_, host)| host)
}

/// The user name a URL carries (`https://ada@example.com`), if any.
fn user_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let (user, _) = authority.rsplit_once('@')?;
    let user = user.split(':').next().unwrap_or(user);
    (!user.is_empty()).then(|| percent_decode(user))
}

/// `%40` and the like in a URL's user name (git writes `ada%40corp`).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text.get(i + 1..i + 3).and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(prompt: &str) -> Kind {
        Question::parse(prompt, "", None).kind
    }

    #[test]
    fn git_credential_prompts() {
        assert_eq!(kind("Username for 'https://github.com': "), Kind::Username { url: "https://github.com".into() });
        assert_eq!(
            kind("Password for 'https://ada%40corp@git.example.com:8443': "),
            Kind::Password { url: "https://ada%40corp@git.example.com:8443".into(), user: Some("ada@corp".into()) }
        );
        let q = Question::parse("Password for 'https://github.com': ", "", None);
        assert_eq!(q.kind, Kind::Password { url: "https://github.com".into(), user: None });
        assert_eq!(q.title(), "Sign in to github.com");
        assert_eq!(q.cancelled(), "Signing in to github.com was cancelled.");
    }

    #[test]
    fn ssh_prompts() {
        assert_eq!(
            kind("Enter passphrase for key '/home/ada/.ssh/id ed25519': "),
            Kind::KeyPassphrase { key: Some("/home/ada/.ssh/id ed25519".into()) }
        );
        assert_eq!(
            kind("Enter passphrase for \"C:/Users/Ada/.ssh/clé\": "),
            Kind::KeyPassphrase { key: Some("C:/Users/Ada/.ssh/clé".into()) }
        );
        assert_eq!(kind("Enter passphrase: "), Kind::KeyPassphrase { key: None });
        assert_eq!(kind("git@example.com's password: "), Kind::SshPassword { login: "git@example.com".into() });
        assert_eq!(kind("(ada@example.com) Password: "), Kind::SshPassword { login: "ada@example.com".into() });

        let host_key = "The authenticity of host 'github.com (140.82.121.4)' can't be established.\n\
                        ED25519 key fingerprint is SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU.\n\
                        This key is not known by any other names.\n\
                        Are you sure you want to continue connecting (yes/no/[fingerprint])? ";
        let q = Question::parse(host_key, "", None);
        assert_eq!(q.kind, Kind::HostKey { host: Some("github.com".into()) });
        assert_eq!(q.title(), "Trust github.com?");
        assert!(q.prompt.ends_with("[fingerprint])?"), "{}", q.prompt);
    }

    #[test]
    fn hints_and_signing_win_over_the_text() {
        assert_eq!(Question::parse("Allow use of key id_ed25519?", "confirm", None).kind, Kind::Confirm);
        assert_eq!(Question::parse("Confirm user presence for key ED25519-SK SHA256:x", "none", None).kind, Kind::Notice);
        let signing = Signing { key: "436C50D526FA7874".into(), user: Some("Ada <ada@example.com>".into()), retry: false };
        let q = Question::parse("Passphrase", "", Some(signing.clone()));
        assert_eq!(q.kind, Kind::Signing(signing));
        assert!(q.cancelled().contains("Ada <ada@example.com>"));
        assert_eq!(kind("Something else entirely?"), Kind::Other);
    }

    #[test]
    fn hosts_and_users_of_urls() {
        assert_eq!(host_of("https://ada@example.com:8443/group/app.git"), "example.com:8443");
        assert_eq!(host_of("https://github.com"), "github.com");
        assert_eq!(host_of("plain words"), "plain words");
        assert_eq!(user_of("https://ada:secret@example.com"), Some("ada".into()));
        assert_eq!(user_of("https://example.com/a@b"), None);
        assert_eq!(quoted("for 'it's'"), Some("it"));
        assert_eq!(quoted("no quotes"), None);
    }
}
