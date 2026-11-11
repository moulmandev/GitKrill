//! The bridge in process, then real git, ssh-keygen and gpg asking
//! through it, with the test binary standing in for GitKrill's executable
//! (see `helper::test_hook`). Nothing here can wait on a prompt: questions
//! are answered by the tests themselves, and every wait has a deadline.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::helper::{Target, ask_with};
use super::*;
use crate::git::cli;

const DEADLINE: Duration = Duration::from_secs(60);

fn bridge() -> Arc<Bridge> {
    let exe = std::env::current_exe().expect("the test binary");
    Bridge::start(None, exe).expect("listens")
}

/// The next open question of command `op`, waiting for it a while.
fn next_question(bridge: &Bridge, op: u64) -> Asked {
    let started = Instant::now();
    loop {
        if let Some(asked) = bridge.questions().into_iter().find(|asked| asked.op == op) {
            return asked;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "no question from command {op}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Asks on a thread of its own: the answer comes from the test.
fn ask_later(target: Target, prompt: &str) -> std::sync::mpsc::Receiver<Result<Option<String>, String>> {
    let (tx, rx) = channel();
    let prompt = prompt.to_owned();
    std::thread::spawn(move || {
        let _ = tx.send(ask_with(&target, &prompt, "", None));
    });
    rx
}

#[test]
fn a_user_name_and_password_in_one_dialog() {
    let bridge = bridge();
    let op = next_op();
    let answer = ask_later(bridge.target(op), "Username for 'https://example.com': ");
    let asked = next_question(&bridge, op);
    assert_eq!(asked.question.kind, Kind::Username { url: "https://example.com".into() });
    bridge.answer_login(asked.id, "ada".into(), "s3cret".into());
    assert_eq!(answer.recv_timeout(DEADLINE).unwrap(), Ok(Some("ada".into())));

    // git asks for the password next: answered without a dialog.
    let target = bridge.target(op);
    assert_eq!(ask_with(&target, "Password for 'https://ada@example.com': ", "", None), Ok(Some("s3cret".into())));
    assert!(!bridge.waiting());

    // Only once, and only for that command: the next one is asked.
    let answer = ask_later(bridge.target(op), "Password for 'https://ada@example.com': ");
    let asked = next_question(&bridge, op);
    bridge.answer(asked.id, Some("typed".into()));
    assert_eq!(answer.recv_timeout(DEADLINE).unwrap(), Ok(Some("typed".into())));
}

#[test]
fn cancelling_explains_the_failure() {
    let bridge = bridge();
    let op = next_op();
    let answer = ask_later(bridge.target(op), "Enter passphrase for key '/home/ada/.ssh/id_ed25519': ");
    let asked = next_question(&bridge, op);
    bridge.answer(asked.id, None);
    assert_eq!(answer.recv_timeout(DEADLINE).unwrap(), Ok(None));
    assert_eq!(
        bridge.take_cancelled(op).as_deref(),
        Some("Unlocking the SSH key /home/ada/.ssh/id_ed25519 was cancelled.")
    );
    assert_eq!(bridge.take_cancelled(op), None);
}

#[test]
fn strangers_and_missing_apps_get_no_answer_quickly() {
    let bridge = bridge();
    let op = next_op();
    let mut wrong = bridge.target(op);
    wrong.token = "0".repeat(64);
    let started = Instant::now();
    assert!(ask_with(&wrong, "Username for 'https://example.com': ", "", None).is_err());
    assert!(bridge.questions().is_empty());

    // Nothing listening: refused at once, well within the helper's limit.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let gone = Target { port, token: "x".into(), op };
    assert!(ask_with(&gone, "Password: ", "", None).is_err());
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
}

#[test]
fn a_question_closes_when_its_helper_goes_away() {
    let bridge = bridge();
    let op = next_op();
    let target = bridge.target(op);
    let mut stream = TcpStream::connect(address(target.port)).unwrap();
    let request = Request { token: target.token, op, prompt: "Confirm user presence for key".into(), hint: "none".into(), signing: None };
    writeln!(stream, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    let asked = next_question(&bridge, op);
    assert_eq!(asked.question.kind, Kind::Notice);
    drop(stream);
    let started = Instant::now();
    while bridge.questions().iter().any(|asked| asked.op == op) {
        assert!(started.elapsed() < Duration::from_secs(10), "the question stayed open");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn background_commands_cannot_ask_and_nothing_is_added_before_install() {
    let mut cmd = std::process::Command::new("git");
    apply(&mut cmd, 1);
    assert_eq!(cmd.get_args().count(), 0, "no bridge in tests unless one is started");
    silence(&mut cmd);
    let envs: Vec<(String, Option<String>)> = cmd
        .get_envs()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
        .collect();
    let get = |name: &str| envs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    assert_eq!(get("GIT_ASKPASS"), Some(Some(String::new())));
    assert_eq!(get("SSH_ASKPASS_REQUIRE"), Some(Some("never".into())));
    assert_eq!(get(TOKEN_VAR), Some(None));

    let bridge = bridge();
    let env = bridge.env(7);
    assert!(env.iter().any(|(k, v)| k == OP_VAR && v == "7"));
    assert!(env.iter().any(|(k, v)| k == "SSH_ASKPASS_REQUIRE" && v == "force"));
    assert!(bridge.config()[1].starts_with("gpg.program="));
}

#[test]
fn the_helper_fails_at_once_without_an_app() {
    // No token or port in this process's environment: nothing to ask.
    let started = Instant::now();
    assert_eq!(helper::dispatch_with(vec!["--askpass".into(), "Password:".into()]), Some(1));
    assert!(started.elapsed() < Duration::from_secs(5));
    // A path opens the app as before.
    assert_eq!(helper::dispatch_with(vec![".".into()]), None);
}

// ---- real programs ---------------------------------------------------------

/// A scratch folder of this test's own.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("prompt-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The environment of a git command that asks through `bridge` as `op`,
/// cut off from the machine's git configuration (no credential manager
/// may open a window) and with the test binary as the helper.
fn isolated_env(bridge: &Bridge, op: u64, dir: &Path) -> Vec<(String, String)> {
    let global = dir.join("gitconfig");
    if !global.exists() {
        std::fs::write(&global, "[user]\n\tname = Test\n\temail = test@example.com\n").unwrap();
    }
    let mut env = bridge.env(op);
    env.extend([
        ("GITKRILL_TEST_HELPER".to_owned(), "1".to_owned()),
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        ("GIT_CONFIG_GLOBAL".to_owned(), global.to_string_lossy().into_owned()),
        ("GCM_INTERACTIVE".to_owned(), "Never".to_owned()),
    ]);
    env
}

/// Runs git on a thread, so a test fails instead of hanging.
fn git_with(dir: &Path, args: &[String], env: Vec<(String, String)>) -> Result<String, String> {
    let (tx, rx) = channel();
    let (dir, args) = (dir.to_path_buf(), args.to_vec());
    std::thread::spawn(move || {
        let _ = tx.send(cli::run(&dir, &args, cli::Options { env: &env, ..Default::default() }));
    });
    rx.recv_timeout(DEADLINE).expect("git finished in time")
}

/// Answers the questions of command `op` with `answer` until stopped,
/// recording what was asked.
struct Answerer {
    asked: Arc<Mutex<Vec<Question>>>,
    stop: Arc<AtomicBool>,
}

impl Answerer {
    fn start(bridge: Arc<Bridge>, op: u64, answer: impl Fn(&Question, usize) -> Option<(String, Option<String>)> + Send + 'static) -> Answerer {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, stopped) = (asked.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                if let Some(open) = bridge.questions().into_iter().find(|asked| asked.op == op) {
                    let count = {
                        let mut seen = seen.lock().unwrap();
                        seen.push(open.question.clone());
                        seen.len()
                    };
                    match answer(&open.question, count) {
                        Some((first, Some(password))) => bridge.answer_login(open.id, first, password),
                        Some((text, None)) => bridge.answer(open.id, Some(text)),
                        None => bridge.answer(open.id, None),
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Answerer { asked, stop }
    }

    fn asked(&self) -> Vec<Question> {
        self.asked.lock().unwrap().clone()
    }
}

impl Drop for Answerer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A web server that wants HTTP basic sign-in and then serves one ref the
/// "dumb" way, recording the `Authorization` headers it got.
struct Server {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(user: &'static str, password: &'static str) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let expected = format!("Basic {}", base64(&format!("{user}:{password}")));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let mut reader = BufReader::new(&stream);
                let mut auth = String::new();
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Authorization: ").or_else(|| line.strip_prefix("authorization: ")) {
                        auth = value.trim().to_owned();
                    }
                    line.clear();
                }
                if !auth.is_empty() {
                    record.lock().unwrap().push(auth.clone());
                }
                let response = if auth == expected {
                    let body = format!("{}\trefs/heads/main\n", "1".repeat(40));
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                };
                let mut stream = stream;
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Server { port, seen }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/repo.git", self.port)
    }
}

fn base64(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn git_asks_for_a_password_and_the_credential_helper_stores_it() {
    assert_eq!(base64("ada:s3cret"), "YWRhOnMzY3JldA==");
    let dir = scratch("https");
    let server = Server::start("ada", "s3cret");
    let bridge = bridge();
    let op = next_op();
    let store = dir.join("credentials store");
    let answerer = Answerer::start(bridge.clone(), op, |question, _| match question.kind {
        Kind::Username { .. } => Some(("ada".into(), Some("s3cret".into()))),
        _ => Some(("not this".into(), None)),
    });
    let helper = format!("store --file=\"{}\"", store.to_string_lossy().replace('\\', "/"));
    let args = strings(&["-c", "credential.helper=", "-c", &format!("credential.helper={helper}"), "ls-remote", &server.url()]);
    let out = git_with(&dir, &args, isolated_env(&bridge, op, &dir)).expect("ls-remote signs in");
    assert!(out.contains("refs/heads/main"), "{out}");

    // One dialog: the password typed with the user name answered git's
    // second question.
    let asked = answerer.asked();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(matches!(&asked[0].kind, Kind::Username { url } if url.starts_with("http://127.0.0.1")), "{asked:?}");
    assert!(server.seen.lock().unwrap().contains(&format!("Basic {}", base64("ada:s3cret"))));
    // git handed what was typed to the credential helper.
    let stored = std::fs::read_to_string(&store).expect("the helper stored the credential");
    assert!(stored.contains("ada:s3cret@127.0.0.1"), "{stored}");

    // Next time the helper answers, and nobody is asked.
    drop(answerer);
    let op = next_op();
    let answerer = Answerer::start(bridge.clone(), op, |_, _| None);
    let out = git_with(&dir, &args, isolated_env(&bridge, op, &dir)).expect("the stored credential works");
    assert!(out.contains("refs/heads/main"), "{out}");
    assert!(answerer.asked().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cancelled_sign_in_fails_cleanly() {
    let dir = scratch("cancel");
    let server = Server::start("ada", "s3cret");
    let bridge = bridge();
    let op = next_op();
    let answerer = Answerer::start(bridge.clone(), op, |_, _| None);
    let args = strings(&["-c", "credential.helper=", "ls-remote", &server.url()]);
    assert!(git_with(&dir, &args, isolated_env(&bridge, op, &dir)).is_err());
    assert_eq!(answerer.asked().len(), 1);
    assert!(bridge.take_cancelled(op).is_some_and(|why| why.contains("was cancelled")));
    assert!(server.seen.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ssh_asks_for_a_key_passphrase() {
    let Some(keygen) = crate::prefs::identity::find_program("ssh-keygen") else {
        eprintln!("ssh-keygen not found; skipped");
        return;
    };
    let dir = scratch("ssh");
    let key = dir.join("id test");
    let made = std::process::Command::new(&keygen)
        .args(["-q", "-t", "ed25519", "-N", "correct horse", "-C", "test", "-f"])
        .arg(&key)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));

    let bridge = bridge();
    for (answer, works) in [("wrong", false), ("correct horse", true)] {
        let op = next_op();
        let answerer = Answerer::start(bridge.clone(), op, move |_, _| Some((answer.to_owned(), None)));
        let mut cmd = std::process::Command::new(&keygen);
        cmd.arg("-y").arg("-f").arg(&key).envs(isolated_env(&bridge, op, &dir)).stdin(std::process::Stdio::null());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(cmd.output());
        });
        let out = rx.recv_timeout(DEADLINE).expect("ssh-keygen finished in time").unwrap();
        let public = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.success(), works, "{public} {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(public.starts_with("ssh-ed25519 "), works, "{public}");
        let asked = answerer.asked();
        assert!(!asked.is_empty());
        assert!(matches!(&asked[0].kind, Kind::KeyPassphrase { .. }), "{asked:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A GnuPG home of the test's own, with its agent started first (detached,
/// so it holds none of the test's pipes) and stopped when dropped.
struct Gnupg {
    gpg: PathBuf,
    /// The home as gpg reads it (`/c/...` for Git for Windows' gpg).
    home: String,
    dir: PathBuf,
}

impl Gnupg {
    fn new(name: &str) -> Option<Gnupg> {
        let gpg = crate::prefs::identity::find_program("gpg")?;
        let dir = scratch(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let home = home_for(&gpg, &dir);
        let gnupg = Gnupg { gpg, home, dir };
        gnupg.gpgconf(&["--launch", "gpg-agent"]);
        Some(gnupg)
    }

    fn command(&self, program: &Path) -> std::process::Command {
        let mut cmd = std::process::Command::new(program);
        cmd.env("GNUPGHOME", &self.home).stdin(std::process::Stdio::null());
        cmd
    }

    fn gpgconf(&self, args: &[&str]) {
        let gpgconf = self.gpg.with_file_name(if cfg!(windows) { "gpgconf.exe" } else { "gpgconf" });
        let program = if gpgconf.is_file() { gpgconf } else { PathBuf::from("gpgconf") };
        let _ = self
            .command(&program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    /// A signing key with `passphrase`; its fingerprint.
    fn key(&self, passphrase: &str) -> Option<String> {
        let made = self
            .command(&self.gpg)
            .args(["--batch", "--pinentry-mode", "loopback", "--passphrase", passphrase])
            .args(["--quick-gen-key", "Ada Test <ada@example.com>", "ed25519", "sign", "never"])
            .output()
            .ok()?;
        if !made.status.success() {
            eprintln!("gpg could not make a key: {}", String::from_utf8_lossy(&made.stderr));
            return None;
        }
        // Forget the passphrase the agent may keep from making the key.
        self.gpgconf(&["--reload", "gpg-agent"]);
        let listed = self.command(&self.gpg).args(["--batch", "--with-colons", "--list-secret-keys"]).output().ok()?;
        crate::prefs::identity::parse_secret_keys(&String::from_utf8_lossy(&listed.stdout), 0)
            .into_iter()
            .next()
            .map(|key| key.fingerprint)
    }
}

impl Drop for Gnupg {
    fn drop(&mut self) {
        self.gpgconf(&["--kill", "gpg-agent"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `GNUPGHOME` for `gpg`: Git for Windows' own gpg reads only the
/// `/c/Users/...` form.
fn home_for(gpg: &Path, dir: &Path) -> String {
    let text = dir.to_string_lossy().into_owned();
    let msys = cfg!(windows) && gpg.to_string_lossy().to_ascii_lowercase().replace('\\', "/").contains("/usr/bin/");
    match text.split_once(":\\") {
        Some((drive, rest)) if msys => format!("/{}/{}", drive.to_ascii_lowercase(), rest.replace('\\', "/")),
        _ => text,
    }
}

#[test]
fn signing_asks_for_the_passphrase_the_agent_does_not_hold() {
    let Some(gnupg) = Gnupg::new("gpg") else {
        eprintln!("gpg not found; skipped");
        return;
    };
    let Some(key) = gnupg.key("tr0ub4dor") else { return };
    let dir = gnupg.dir.join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    crate::tests::git(&dir, &["init", "-q", "-b", "main"]);
    let bridge = bridge();
    let commit = |message: &str, op: u64| {
        let mut env = isolated_env(&bridge, op, &gnupg.dir);
        env.push(("GNUPGHOME".into(), gnupg.home.clone()));
        let mut args = bridge.config().to_vec();
        args.extend(strings(&["-c", &format!("user.signingkey={key}"), "commit", "-q", "-S", "--allow-empty", "-m", message]));
        git_with(&dir, &args, env)
    };

    // A wrong passphrase is asked again, then the right one signs.
    let op = next_op();
    let answerer = Answerer::start(bridge.clone(), op, |_, count| Some((if count == 1 { "wrong" } else { "tr0ub4dor" }.into(), None)));
    commit("signed", op).expect("the commit is signed");
    let asked = answerer.asked();
    assert_eq!(asked.len(), 2, "{asked:?}");
    match (&asked[0].kind, &asked[1].kind) {
        (Kind::Signing(first), Kind::Signing(second)) => {
            assert!(!first.retry && second.retry);
            assert_eq!(first.user.as_deref(), Some("Ada Test <ada@example.com>"));
        }
        other => panic!("{other:?}"),
    }
    let object = crate::tests::git(&dir, &["cat-file", "-p", "HEAD"]);
    assert!(object.contains("gpgsig -----BEGIN PGP SIGNATURE-----"), "{object}");
    drop(answerer);

    // The agent has it now: nothing is asked.
    let op = next_op();
    let answerer = Answerer::start(bridge.clone(), op, |_, _| None);
    commit("signed again", op).expect("signed from the agent's cache");
    assert!(answerer.asked().is_empty());
    drop(answerer);

    // Without it, cancelling fails the commit and says why.
    gnupg.gpgconf(&["--reload", "gpg-agent"]);
    let op = next_op();
    let answerer = Answerer::start(bridge.clone(), op, |_, _| None);
    assert!(commit("not signed", op).is_err());
    assert_eq!(answerer.asked().len(), 1);
    assert!(bridge.take_cancelled(op).is_some_and(|why| why.starts_with("Signing was cancelled")));
    assert_eq!(crate::tests::git(&dir, &["log", "--format=%s"]), "signed again\nsigned");

    // Verifying goes through the wrapper untouched.
    let op = next_op();
    let mut env = isolated_env(&bridge, op, &gnupg.dir);
    env.push(("GNUPGHOME".into(), gnupg.home.clone()));
    let mut args = bridge.config().to_vec();
    args.extend(strings(&["verify-commit", "HEAD"]));
    let verified = git_with(&dir, &args, env).expect("the signature verifies");
    assert!(verified.contains("Good signature"), "{verified}");
}

#[test]
fn reading_the_helper_protocol_by_hand() {
    // What a helper sends, as the app reads it: one JSON line.
    let line = r#"{"token":"t","op":3,"prompt":"Password: ","hint":"","signing":null}"#;
    let request: Request = serde_json::from_str(line).unwrap();
    assert_eq!((request.op, request.prompt.as_str()), (3, "Password: "));
    // Older or smaller helpers may leave the optional fields out.
    let request: Request = serde_json::from_str(r#"{"token":"t","op":0,"prompt":"x"}"#).unwrap();
    assert!(request.hint.is_empty() && request.signing.is_none());
    let mut bytes = Vec::new();
    let mut reader = BufReader::new(&b"{\"answer\":null}\n"[..]);
    reader.read_to_end(&mut bytes).unwrap();
    let reply: Reply = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(reply.answer, None);
}
