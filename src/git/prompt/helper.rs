//! GitKrill's executable when git or ssh start it to ask something: as
//! `GIT_ASKPASS` / `SSH_ASKPASS` it gets the question as its only
//! argument and prints the answer; as `gpg.program` or `gpg.x509.program`
//! it gets gpg's or gpgsm's arguments (`super::gpg`). `main` calls
//! [`dispatch`] before anything else, so no window ever opens in these
//! modes.
//!
//! Both are recognised by the bridge's token in the environment (only
//! GitKrill's own git commands carry it): with it, a first argument that
//! starts with `-` is a gpg call (git passes gpg options first; prompts
//! never start with one), and a single argument that is not an existing
//! path is a question. `--askpass <prompt>` and `--gpg <arguments>` ask
//! for a mode outright.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use super::{OP_VAR, PORT_VAR, Reply, Request, Signing, TOKEN_VAR, wipe};

/// How long a helper waits for the app to take its question.
const CONNECT: Duration = Duration::from_secs(2);
/// How long it waits for the user: a dialog left open longer than this
/// fails the command rather than keeping it forever.
const ANSWER: Duration = Duration::from_secs(30 * 60);

/// GitKrill's own flags (`main`), which stay its own whatever started it.
const APP_FLAGS: &[&str] = &["--version", "-V", "--help", "-h", "--git", "--dump"];

/// Runs a helper mode when the command line asks for one, and returns its
/// exit code; `None` starts the app as usual.
pub fn dispatch() -> Option<i32> {
    dispatch_with(std::env::args_os().skip(1).collect())
}

/// [`dispatch`] for `args` (without the program name).
pub fn dispatch_with(args: Vec<OsString>) -> Option<i32> {
    let first = args.first().map(|arg| arg.to_string_lossy().into_owned());
    match first.as_deref() {
        Some("--askpass") => {
            let prompt: Vec<String> = args[1..].iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
            return Some(askpass(&prompt.join(" ")));
        }
        Some("--gpg") => return Some(super::gpg::wrap(args[1..].to_vec())),
        _ => {}
    }
    // Started by one of GitKrill's git commands, or by what it ran.
    std::env::var_os(TOKEN_VAR)?;
    match first.as_deref() {
        Some(arg) if arg.starts_with('-') && !APP_FLAGS.contains(&arg) => Some(super::gpg::wrap(args)),
        Some(prompt) if args.len() == 1 && !std::path::Path::new(prompt).exists() => Some(askpass(prompt)),
        _ => None,
    }
}

/// `GIT_ASKPASS` / `SSH_ASKPASS`: prints the answer to `prompt` (0), or
/// fails (1) when the user cancelled or the app could not be asked.
fn askpass(prompt: &str) -> i32 {
    // ssh says what kind of question it is.
    let hint = std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default();
    match ask(prompt, &hint, None) {
        Ok(Some(mut answer)) => {
            let mut out = std::io::stdout().lock();
            let written = out.write_all(answer.as_bytes()).and_then(|()| out.write_all(b"\n")).and_then(|()| out.flush());
            wipe(&mut answer);
            if written.is_ok() { 0 } else { 1 }
        }
        Ok(None) => {
            eprintln!("GitKrill: cancelled");
            1
        }
        Err(why) => {
            eprintln!("GitKrill could not ask: {why}");
            1
        }
    }
}

/// Sends a question to the app the environment names and waits for the
/// answer: `Ok(None)` when the user cancelled, `Err` when the app could not
/// be reached (it then fails within a couple of seconds, never waits).
pub fn ask(prompt: &str, hint: &str, signing: Option<Signing>) -> Result<Option<String>, String> {
    ask_with(&Target::from_env()?, prompt, hint, signing)
}

/// Where a question goes: the app's port and token, and the command that
/// asks.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub port: u16,
    pub token: String,
    pub op: u64,
}

impl Target {
    fn from_env() -> Result<Target, String> {
        let port: u16 = std::env::var(PORT_VAR)
            .ok()
            .and_then(|port| port.parse().ok())
            .ok_or("GitKrill is not running (no port in the environment)")?;
        let token = std::env::var(TOKEN_VAR).map_err(|_| "no token in the environment")?;
        let op = std::env::var(OP_VAR).ok().and_then(|op| op.parse().ok()).unwrap_or(0);
        Ok(Target { port, token, op })
    }
}

/// [`ask`] at `target`.
pub(crate) fn ask_with(target: &Target, prompt: &str, hint: &str, signing: Option<Signing>) -> Result<Option<String>, String> {
    let port = target.port;
    let mut stream = TcpStream::connect_timeout(&super::address(port), CONNECT)
        .map_err(|e| format!("GitKrill did not answer on port {port}: {e}"))?;
    let request =
        Request { token: target.token.clone(), op: target.op, prompt: prompt.to_owned(), hint: hint.to_owned(), signing };
    let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    line.push('\n');
    stream.set_write_timeout(Some(CONNECT)).map_err(|e| e.to_string())?;
    stream.write_all(line.as_bytes()).map_err(|e| format!("could not send the question: {e}"))?;
    stream.flush().map_err(|e| e.to_string())?;

    stream.set_read_timeout(Some(ANSWER)).map_err(|e| e.to_string())?;
    let mut answer = String::new();
    BufReader::new((&stream).take(1024 * 1024))
        .read_line(&mut answer)
        .map_err(|e| format!("no answer from GitKrill: {e}"))?;
    if answer.is_empty() {
        // The app quit, or the token was not its own.
        return Err("GitKrill closed the connection without answering".to_owned());
    }
    let reply: Result<Reply, _> = serde_json::from_str(answer.trim_end());
    wipe(&mut answer);
    reply.map(|reply| reply.answer).map_err(|e| format!("an answer GitKrill could not read: {e}"))
}

/// In tests, the test binary stands in for GitKrill's executable: started
/// with `GITKRILL_TEST_HELPER` set (only the tests' own git commands have
/// it), it runs [`dispatch_with`] and exits before the test harness starts.
/// This runs from the platform's list of initialisers, before `main`.
#[cfg(test)]
mod test_hook {
    use std::ffi::OsString;

    fn run(args: Vec<OsString>) {
        if std::env::var_os("GITKRILL_TEST_HELPER").is_none() {
            return;
        }
        let code = super::dispatch_with(args).unwrap_or(99);
        std::process::exit(code);
    }

    // glibc and dyld pass the arguments to initialisers; std may not have
    // read them yet at this point.
    #[cfg(unix)]
    extern "C" fn init(argc: i32, argv: *const *const std::ffi::c_char, _env: *const *const std::ffi::c_char) {
        use std::os::unix::ffi::OsStrExt;
        if argv.is_null() {
            return;
        }
        let args = (1..argc.max(1) as usize)
            .map(|i| {
                // SAFETY: the loader passes `argc` valid C strings.
                let arg = unsafe { std::ffi::CStr::from_ptr(*argv.add(i)) };
                std::ffi::OsStr::from_bytes(arg.to_bytes()).to_os_string()
            })
            .collect();
        run(args);
    }

    #[cfg(windows)]
    extern "C" fn init() {
        run(std::env::args_os().skip(1).collect());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    #[used]
    #[unsafe(link_section = ".init_array")]
    static INIT: extern "C" fn(i32, *const *const std::ffi::c_char, *const *const std::ffi::c_char) = init;

    #[cfg(target_vendor = "apple")]
    #[used]
    #[unsafe(link_section = "__DATA,__mod_init_func")]
    static INIT: extern "C" fn(i32, *const *const std::ffi::c_char, *const *const std::ffi::c_char) = init;

    #[cfg(windows)]
    #[used]
    #[unsafe(link_section = ".CRT$XCU")]
    static INIT: extern "C" fn() = init;
}
