//! Asking the user what git, ssh and gpg would ask on a terminal: an HTTPS
//! remote's user name and password, an SSH key's passphrase, whether to
//! trust a new host, the passphrase of the OpenPGP key or X.509
//! certificate commits are signed with.
//!
//! GitKrill's git commands have no terminal, so every one of them gets
//! `GIT_ASKPASS` and `SSH_ASKPASS` (with `SSH_ASKPASS_REQUIRE=force`)
//! naming GitKrill's own executable, and `gpg.program` and
//! `gpg.x509.program` naming it too
//! ([`Bridge::env`], [`Bridge::config`]; `cli::command` adds them). Started
//! that way, the executable is a *helper* ([`helper::dispatch`], run by
//! `main` before anything else): it sends the question to the running app
//! over a loopback TCP connection, waits for the answer and prints it,
//! which is all git and ssh want from an askpass program. The GPG wrapper
//! ([`gpg`]) runs the real gpg (or gpgsm) and asks only when gpg-agent does
//! not hold the passphrase already.
//!
//! The connection is the app's alone: it listens on 127.0.0.1 only, and a
//! question counts only with the run's token, 32 random bytes that only
//! the environment of GitKrill's own git commands carries. A helper that
//! cannot reach the app fails at once, so git fails as it did before.
//!
//! The UI (`ui::prompt`) shows the oldest open question ([`Bridge::first`])
//! and answers it ([`Bridge::answer`]). A question whose helper went away
//! (ssh stops waiting for a security key once it was touched) closes by
//! itself. When the user cancels, the command fails with
//! [`Question::cancelled`] in place of git's words about a terminal (see
//! [`take_cancelled`]).
//!
//! A credential helper that answers (Git Credential Manager, the macOS
//! keychain, `store`) is asked first by git, so it keeps working as
//! before; git asks GitKrill only when none knows the answer, and hands
//! what the user typed back to the helpers once it worked, so they store it.

pub mod gpg;
pub mod helper;
mod kind;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

pub use kind::{Kind, Question, Signing, host_of};

/// The port the app listens on.
pub const PORT_VAR: &str = "GITKRILL_PROMPT_PORT";
/// The run's token; the helper sends it with every question.
pub const TOKEN_VAR: &str = "GITKRILL_PROMPT_TOKEN";
/// Which of GitKrill's git commands asks, so a cancelled question can
/// explain that command's failure.
pub const OP_VAR: &str = "GITKRILL_PROMPT_OP";

/// How long a password typed together with its user name is kept for the
/// question git asks next.
const REMEMBER_FOR: Duration = Duration::from_secs(120);

/// A question, as sent by the helper.
#[derive(Serialize, Deserialize, Debug, Default)]
pub(crate) struct Request {
    pub token: String,
    pub op: u64,
    pub prompt: String,
    /// `SSH_ASKPASS_PROMPT`.
    #[serde(default)]
    pub hint: String,
    #[serde(default)]
    pub signing: Option<Signing>,
}

/// The app's answer: `None` when the user cancelled.
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Reply {
    pub answer: Option<String>,
}

/// A question waiting for the user.
#[derive(Clone, Debug)]
pub struct Asked {
    pub id: u64,
    /// The command that asks (0 when unknown).
    pub op: u64,
    pub question: Question,
}

struct Open {
    asked: Asked,
    reply: Sender<Option<String>>,
}

/// A password the user typed in the same dialog as the user name: git asks
/// for it right after it got the name.
struct Remembered {
    op: u64,
    host: String,
    user: String,
    password: String,
    at: Instant,
}

impl Drop for Remembered {
    fn drop(&mut self) {
        wipe(&mut self.password);
    }
}

#[derive(Default)]
struct State {
    next_id: u64,
    open: Vec<Open>,
    /// Why commands failed, by op, for questions the user cancelled.
    cancelled: Vec<(u64, String)>,
    remembered: Option<Remembered>,
}

/// The app's end of the connection: where helpers send questions, and the
/// questions waiting for an answer.
pub struct Bridge {
    port: u16,
    token: String,
    /// What git and ssh run to ask: GitKrill's executable.
    exe: PathBuf,
    /// Woken when a question arrives or goes away.
    ctx: Option<egui::Context>,
    state: Mutex<State>,
    /// How many questions are open, for a check each frame without a lock.
    waiting: AtomicUsize,
}

impl Bridge {
    /// Listens on a free loopback port. Each connection is served on a
    /// thread of its own while its question is open; questions that
    /// arrive repaint `ctx`.
    pub fn start(ctx: Option<egui::Context>, exe: PathBuf) -> std::io::Result<Arc<Bridge>> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let bridge = Arc::new(Bridge {
            port,
            token: new_token()?,
            exe,
            ctx,
            state: Mutex::new(State::default()),
            waiting: AtomicUsize::new(0),
        });
        let serving = bridge.clone();
        std::thread::Builder::new().name("prompt-bridge".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let bridge = serving.clone();
                let _ = std::thread::Builder::new()
                    .name("prompt-question".into())
                    .spawn(move || bridge.serve(stream));
            }
        })?;
        Ok(bridge)
    }

    /// The variables a git command needs to ask through this bridge, as
    /// command `op`. On top of these, [`Bridge::config`] goes on its
    /// command line.
    pub fn env(&self, op: u64) -> Vec<(String, String)> {
        let exe = self.exe.to_string_lossy().into_owned();
        let mut env = vec![
            ("GIT_ASKPASS".to_owned(), exe.clone()),
            ("SSH_ASKPASS".to_owned(), exe),
            // OpenSSH 8.4 and later: use it even with a terminal around and
            // without a display.
            ("SSH_ASKPASS_REQUIRE".to_owned(), "force".to_owned()),
            (PORT_VAR.to_owned(), self.port.to_string()),
            (TOKEN_VAR.to_owned(), self.token.clone()),
            (OP_VAR.to_owned(), op.to_string()),
        ];
        // Older OpenSSH (before 8.4, on older Linux distributions) uses
        // SSH_ASKPASS only when DISPLAY is set.
        let no_display = std::env::var_os("DISPLAY").is_none_or(|display| display.is_empty());
        if cfg!(all(unix, not(target_os = "macos"))) && no_display {
            env.push(("DISPLAY".to_owned(), ":0".to_owned()));
        }
        env
    }

    /// `-c gpg.program=<GitKrill> -c gpg.x509.program=<GitKrill>`, before a
    /// git command's own arguments: OpenPGP and X.509 signing go through
    /// the wrapper, which runs the gpg or gpgsm git would have run
    /// ([`gpg`]).
    pub fn config(&self) -> [String; 4] {
        let exe = self.exe.to_string_lossy();
        ["-c".to_owned(), format!("gpg.program={exe}"), "-c".to_owned(), format!("gpg.x509.program={exe}")]
    }

    /// Where a helper of command `op` sends its questions.
    #[cfg(test)]
    pub(crate) fn target(&self, op: u64) -> helper::Target {
        helper::Target { port: self.port, token: self.token.clone(), op }
    }

    /// Whether a question is open (no lock taken).
    pub fn waiting(&self) -> bool {
        self.waiting.load(Ordering::Relaxed) > 0
    }

    /// The oldest open question.
    pub fn first(&self) -> Option<Asked> {
        if !self.waiting() {
            return None;
        }
        self.lock().open.first().map(|open| open.asked.clone())
    }

    /// Every open question, oldest first.
    #[cfg(test)]
    pub fn questions(&self) -> Vec<Asked> {
        self.lock().open.iter().map(|open| open.asked.clone()).collect()
    }

    /// Answers question `id`; `None` cancels it, and the command that
    /// asked then fails with [`Question::cancelled`].
    pub fn answer(&self, id: u64, answer: Option<String>) {
        let mut state = self.lock();
        let Some(at) = state.open.iter().position(|open| open.asked.id == id) else { return };
        let open = state.open.remove(at);
        self.waiting.store(state.open.len(), Ordering::Relaxed);
        if answer.is_none() {
            let why = open.asked.question.cancelled();
            state.cancelled.retain(|(op, _)| *op != open.asked.op);
            state.cancelled.push((open.asked.op, why));
            // Nothing more to ask for this command.
            state.remembered = None;
        }
        drop(state);
        let _ = open.reply.send(answer);
    }

    /// Answers a user-name question with `user`, and keeps `password` for
    /// the password question git asks next for the same host.
    pub fn answer_login(&self, id: u64, user: String, password: String) {
        {
            let mut state = self.lock();
            let host = state.open.iter().find(|open| open.asked.id == id).and_then(|open| match &open.asked.question.kind {
                Kind::Username { url } => Some((open.asked.op, host_of(url).to_owned())),
                _ => None,
            });
            if let Some((op, host)) = host
                && !password.is_empty()
            {
                state.remembered = Some(Remembered { op, host, user: user.clone(), password, at: Instant::now() });
            }
        }
        self.answer(id, Some(user));
    }

    /// Why command `op` failed, when the user cancelled one of its
    /// questions (once).
    pub fn take_cancelled(&self, op: u64) -> Option<String> {
        let mut state = self.lock();
        let at = state.cancelled.iter().position(|(cancelled, _)| *cancelled == op)?;
        Some(state.cancelled.remove(at).1)
    }

    /// Forgets what is kept for command `op` once it ended.
    pub fn finish(&self, op: u64) {
        let mut state = self.lock();
        state.cancelled.retain(|(cancelled, _)| *cancelled != op);
        if state.remembered.as_ref().is_some_and(|kept| kept.op == op) {
            state.remembered = None;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn repaint(&self) {
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
    }

    /// One helper's connection: its question, then the answer.
    fn serve(&self, stream: TcpStream) {
        let Some(request) = read_request(&stream) else { return };
        if !same(request.token.as_bytes(), self.token.as_bytes()) {
            return;
        }
        let question = Question::parse(&request.prompt, &request.hint, request.signing);

        // The password typed in the same dialog as the user name.
        if let Some(password) = self.remembered_password(request.op, &question) {
            reply(&stream, Some(password));
            return;
        }

        let (tx, rx) = channel();
        let id = {
            let mut state = self.lock();
            state.next_id += 1;
            let id = state.next_id;
            state.open.push(Open { asked: Asked { id, op: request.op, question }, reply: tx });
            self.waiting.store(state.open.len(), Ordering::Relaxed);
            id
        };
        self.repaint();

        loop {
            match rx.recv_timeout(Duration::from_millis(300)) {
                Ok(answer) => {
                    reply(&stream, answer);
                    return;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    reply(&stream, None);
                    return;
                }
                // ssh kills the helper of a notice once it is done with
                // it; a helper can also be stopped with its command.
                Err(RecvTimeoutError::Timeout) if gone(&stream) => {
                    let mut state = self.lock();
                    state.open.retain(|open| open.asked.id != id);
                    self.waiting.store(state.open.len(), Ordering::Relaxed);
                    drop(state);
                    self.repaint();
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn remembered_password(&self, op: u64, question: &Question) -> Option<String> {
        let Kind::Password { url, user } = &question.kind else { return None };
        let mut state = self.lock();
        let kept = state.remembered.take()?;
        let fits = kept.op == op
            && kept.at.elapsed() < REMEMBER_FOR
            && kept.host == host_of(url)
            && user.as_deref().is_none_or(|user| user == kept.user);
        // Used once or not at all.
        fits.then(|| kept.password.clone())
    }
}

/// Reads the helper's one line, at most 64 KiB, within a few seconds.
fn read_request(stream: &TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut line = String::new();
    BufReader::new(stream.take(64 * 1024)).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim_end()).ok()
}

fn reply(mut stream: &TcpStream, answer: Option<String>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut reply = Reply { answer };
    if let Ok(mut line) = serde_json::to_string(&reply) {
        line.push('\n');
        let _ = stream.write_all(line.as_bytes());
        let _ = stream.flush();
        wipe(&mut line);
    }
    if let Some(answer) = reply.answer.as_mut() {
        wipe(answer);
    }
}

/// Whether the helper closed its end.
fn gone(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let gone = match stream.peek(&mut [0u8; 1]) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
    };
    let _ = stream.set_nonblocking(false);
    gone
}

/// Compares in a time that does not depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn new_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Overwrites a secret before its memory is freed.
pub fn wipe(text: &mut String) {
    // SAFETY: zero bytes are valid UTF-8.
    unsafe { text.as_bytes_mut().fill(0) };
    text.clear();
}

/// Numbers commands, so a question can say which one asks.
static NEXT_OP: AtomicU64 = AtomicU64::new(1);

/// A number for the next git command.
pub fn next_op() -> u64 {
    NEXT_OP.fetch_add(1, Ordering::Relaxed)
}

// ---- the app's bridge ------------------------------------------------------

static APP: OnceLock<Option<Arc<Bridge>>> = OnceLock::new();

/// Starts the app's bridge, once: from then on every git command
/// [`apply`] is called on can ask the user. Without it (tests, `--dump`)
/// git commands cannot ask anything, as before.
pub fn install(ctx: &egui::Context) {
    APP.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let bridge = Bridge::start(Some(ctx.clone()), exe).ok()?;
        // Scripted runs say where to send questions, so a script can ask
        // through `gitkrill --askpass` with these in its environment.
        if std::env::var_os("GITKRILL_SHOT").is_some() {
            println!("prompt bridge: {PORT_VAR}={} {TOKEN_VAR}={}", bridge.port, bridge.token);
        }
        Some(bridge)
    });
}

/// The app's bridge, once [`install`]ed.
pub fn app() -> Option<&'static Arc<Bridge>> {
    APP.get().and_then(Option::as_ref)
}

/// Lets `cmd`, a git command, ask the user (as command `op`, 0 when no
/// failure is explained): the environment of [`Bridge::env`] and the
/// options of [`Bridge::config`], which go before the command's own
/// arguments. Does nothing before [`install`].
pub fn apply(cmd: &mut Command, op: u64) {
    if let Some(bridge) = app() {
        cmd.args(bridge.config());
        cmd.envs(bridge.env(op));
    }
}

/// For commands nobody is waiting for (the automatic fetch): nothing may
/// ask, so git and ssh fail as they would have without GitKrill.
pub fn silence(cmd: &mut Command) {
    cmd.env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env_remove(TOKEN_VAR)
        .env_remove(PORT_VAR);
}

/// Why command `op` failed, when the user cancelled one of its questions.
pub fn take_cancelled(op: u64) -> Option<String> {
    app()?.take_cancelled(op)
}

/// Forgets what was kept for command `op`.
pub fn finish(op: u64) {
    if let Some(bridge) = app() {
        bridge.finish(op);
    }
}

/// The address helpers connect to.
pub(crate) fn address(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

#[cfg(test)]
pub(crate) mod tests;
