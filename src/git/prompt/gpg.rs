//! `gpg.program` for GitKrill's git commands: the gpg git would have run
//! does the work, and when gpg-agent does not hold the signing key's
//! passphrase the user is asked in the app.
//!
//! git signs with `<gpg.program> --status-fd=2 -bsau <key>`, the data on
//! standard input, the signature on standard output and gpg's status lines
//! on standard error. The wrapper first runs that with `--pinentry-mode
//! error`: when the agent has the passphrase cached it signs and nothing is
//! asked. When it would need a pinentry (`FAILURE sign` with gpg's "No
//! pinentry" code), the wrapper asks the app for the passphrase and runs it
//! again with `--pinentry-mode loopback --batch --passphrase-fd 0`, the
//! passphrase on standard input and the data in a temporary file; the
//! agent caches the passphrase as it does for its own pinentry. A wrong
//! passphrase is asked again, twice. This works with GnuPG 2.1 and later
//! (Git for Windows' own gpg, Gpg4win, GPG Suite, Linux distributions)
//! unless the agent was set up with `no-allow-loopback-pinentry`.
//!
//! Everything else (verifying, a `gpg.program` that is not GnuPG, a gpg
//! that does not know `--pinentry-mode`) is passed through untouched.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{Signing, wipe};
use crate::git::cli;

/// gpg's error codes (`gpg-error.h`), in the low 16 bits of a status
/// line's number.
const NO_PIN_ENTRY: u32 = 85;
const BAD_PASSPHRASE: u32 = 11;
const NO_PASSPHRASE: u32 = 89;

/// How many passphrases are tried before giving up.
const TRIES: usize = 3;

/// Runs gpg for git with `args`; returns the exit code to leave with.
pub fn wrap(args: Vec<OsString>) -> i32 {
    #[cfg(windows)]
    private_std_handles();
    let program = real_program();
    let key = signing_key(&args);
    let Some(key) = key.filter(|_| is_gnupg(&program)) else {
        return passthrough(&program, &args);
    };

    let mut data = Vec::new();
    if std::io::stdin().read_to_end(&mut data).is_err() {
        return passthrough(&program, &args);
    }
    let first = run(&program, &with(&["--pinentry-mode", "error"], &args, &[]), &data);
    if first.code == 0 || !first.needs(&[NO_PIN_ENTRY]) {
        if first.code != 0 && first.unknown_option() {
            // GnuPG 1, without an agent's pinentry modes.
            return run(&program, &args, &data).emit();
        }
        return first.emit();
    }

    let user = user_id(&program, &key);
    let file = match DataFile::new(&data) {
        Ok(file) => file,
        Err(e) => {
            let code = first.emit();
            eprintln!("GitKrill could not ask for the passphrase: {e}");
            return code;
        }
    };
    let mut retry = false;
    for _ in 0..TRIES {
        let signing = Signing { key: key.clone(), user: user.clone(), retry };
        let prompt = format!("Passphrase of the OpenPGP key {}", user.as_deref().unwrap_or(&key));
        match super::helper::ask(&prompt, "", Some(signing)) {
            Ok(Some(mut passphrase)) => {
                passphrase.push('\n');
                let loopback = ["--pinentry-mode", "loopback", "--batch", "--passphrase-fd", "0"];
                let tail = [OsString::from("-o"), OsString::from("-"), file.arg()];
                let ran = run(&program, &with(&loopback, &args, &tail), passphrase.as_bytes());
                wipe(&mut passphrase);
                if ran.code != 0 && ran.needs(&[BAD_PASSPHRASE, NO_PASSPHRASE]) {
                    retry = true;
                    continue;
                }
                return ran.emit();
            }
            Ok(None) => {
                eprintln!("gpg: signing failed: no passphrase was given in GitKrill");
                return 2;
            }
            Err(why) => {
                let code = first.emit();
                eprintln!("GitKrill could not ask for the passphrase: {why}");
                return code;
            }
        }
    }
    eprintln!("gpg: signing failed: Bad passphrase ({TRIES} tries)");
    2
}

/// `before`, then `args`, then `after`.
fn with(before: &[&str], args: &[OsString], after: &[OsString]) -> Vec<OsString> {
    before.iter().map(OsString::from).chain(args.iter().cloned()).chain(after.iter().cloned()).collect()
}

/// The key a signing call signs with (`-bsau <key>`, `-u <key>`,
/// `--local-user <key>`), empty for gpg's default key; `None` when the
/// call does not sign.
pub fn signing_key(args: &[OsString]) -> Option<String> {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
    let (mut signs, mut key) = (false, None);
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            break;
        } else if let Some(long) = arg.strip_prefix("--") {
            match long.split_once('=') {
                Some(("local-user", value)) => key = Some(value.to_owned()),
                _ if long == "local-user" => {
                    key = args.get(i + 1).cloned();
                    i += 1;
                }
                _ if matches!(long, "sign" | "detach-sign" | "clearsign" | "clear-sign") => signs = true,
                _ => {}
            }
        } else if let Some(cluster) = arg.strip_prefix('-').filter(|cluster| !cluster.is_empty()) {
            // Short options run together: -bsau <key>, or -u<key>.
            for (at, letter) in cluster.char_indices() {
                match letter {
                    's' => signs = true,
                    'u' => {
                        let rest = &cluster[at + 1..];
                        if rest.is_empty() {
                            key = args.get(i + 1).cloned();
                            i += 1;
                        } else {
                            key = Some(rest.to_owned());
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
        i += 1;
    }
    signs.then(|| key.unwrap_or_default())
}

/// Whether `program` is GnuPG's gpg (`gpg`, `gpg2`, `gpg.exe`, a full
/// path to one), which knows the pinentry modes.
fn is_gnupg(program: &OsStr) -> bool {
    Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|name| name.starts_with("gpg") && !name.starts_with("gpgsm"))
}

/// The OpenPGP program git would run without GitKrill's `-c
/// gpg.program=...`: `gpg.openpgp.program` or `gpg.program` from the
/// configuration files (the later one wins, as in git), else `gpg`.
fn real_program() -> OsString {
    let mut cmd = Command::new(cli::program());
    cmd.args(["config", "--get-regexp", r"^gpg\.(openpgp\.)?program$"])
        // The `-c` options of the command that runs us, ours among them.
        .env_remove("GIT_CONFIG_PARAMETERS")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    cli::hide_window(&mut cmd);
    let configured = cmd
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .and_then(|out| out.lines().filter_map(|line| line.split_once(' ')).map(|(_, value)| value.trim().to_owned()).next_back())
        .filter(|value| !value.is_empty());
    let program = configured.map(OsString::from).unwrap_or_else(|| OsString::from("gpg"));
    // Never ourselves, whatever a configuration file says.
    let me = std::env::current_exe().ok().and_then(|exe| exe.canonicalize().ok());
    let it = Path::new(&program).canonicalize().ok();
    if me.is_some() && me == it { OsString::from("gpg") } else { program }
}

/// The user id of `key`, for the dialog: "Ada Lovelace <ada@example.com>".
fn user_id(program: &OsStr, key: &str) -> Option<String> {
    let mut args = vec!["--batch", "--with-colons", "--list-secret-keys"];
    if !key.is_empty() {
        args.extend(["--", key]);
    }
    let args: Vec<OsString> = args.into_iter().map(OsString::from).collect();
    let ran = run(program, &args, b"");
    let out = String::from_utf8_lossy(&ran.stdout);
    crate::prefs::identity::parse_secret_keys(&out, 0)
        .into_iter()
        .map(|key| key.user_id)
        .find(|user| !user.is_empty())
}

/// Runs gpg with this process's standard input, output and error.
fn passthrough(program: &OsStr, args: &[OsString]) -> i32 {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cli::hide_window(&mut cmd);
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(2),
        Err(e) => {
            eprintln!("GitKrill could not run {}: {e}", program.to_string_lossy());
            2
        }
    }
}

/// What a gpg run printed, and how it ended.
struct Ran {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Ran {
    /// Whether gpg failed with one of `codes` (its status lines say).
    fn needs(&self, codes: &[u32]) -> bool {
        failure_codes(&String::from_utf8_lossy(&self.stderr)).any(|code| codes.contains(&code))
    }

    fn unknown_option(&self) -> bool {
        let err = String::from_utf8_lossy(&self.stderr);
        err.contains("invalid option") && err.contains("pinentry-mode")
    }

    /// Hands what gpg printed to git, and returns its exit code.
    fn emit(&self) -> i32 {
        let _ = std::io::stdout().write_all(&self.stdout).and_then(|()| std::io::stdout().flush());
        let _ = std::io::stderr().write_all(&self.stderr).and_then(|()| std::io::stderr().flush());
        self.code
    }
}

/// The error codes of `[GNUPG:] FAILURE <what> <code>` and `[GNUPG:] ERROR
/// <what> <code>` status lines, without their source.
fn failure_codes(status: &str) -> impl Iterator<Item = u32> + '_ {
    status.lines().filter_map(|line| {
        let rest = line.trim().strip_prefix("[GNUPG:] ")?;
        let rest = rest.strip_prefix("FAILURE ").or_else(|| rest.strip_prefix("ERROR "))?;
        let code: u32 = rest.split_whitespace().nth(1)?.parse().ok()?;
        Some(code & 0xFFFF)
    })
}

/// Runs `program` with `input` on its standard input and collects what it
/// prints. A gpg-agent that gpg starts may keep the pipes open long after
/// gpg exited, so they are read on threads and given only a moment once
/// gpg is done.
fn run(program: &OsStr, args: &[OsString], input: &[u8]) -> Ran {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cli::hide_window(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let stderr = format!("GitKrill could not run {}: {e}\n", program.to_string_lossy()).into_bytes();
            return Ran { code: 2, stdout: Vec::new(), stderr };
        }
    };
    if let Some(mut pipe) = child.stdin.take() {
        let mut input = input.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
            input.fill(0);
        });
    }
    let stdout = Collected::start(child.stdout.take());
    let stderr = Collected::start(child.stderr.take());
    let code = child.wait().ok().and_then(|status| status.code()).unwrap_or(2);
    Ran { code, stdout: stdout.finish(), stderr: stderr.finish() }
}

/// A pipe read on a thread of its own.
struct Collected {
    bytes: Arc<Mutex<Vec<u8>>>,
    done: std::sync::mpsc::Receiver<()>,
}

impl Collected {
    fn start(pipe: Option<impl Read + Send + 'static>) -> Collected {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (tx, done) = channel();
        let filling = bytes.clone();
        std::thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = pipe.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    filling.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]);
                }
            }
            let _ = tx.send(());
        });
        Collected { bytes, done }
    }

    fn finish(self) -> Vec<u8> {
        let _ = self.done.recv_timeout(Duration::from_secs(2));
        std::mem::take(&mut *self.bytes.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// The data to sign, in a file of its own while the passphrase takes
/// standard input; removed when dropped.
struct DataFile(PathBuf);

impl DataFile {
    fn new(data: &[u8]) -> std::io::Result<DataFile> {
        let mut random = [0u8; 8];
        let _ = getrandom::fill(&mut random);
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("gitkrill-sign-{}-{name}", std::process::id()));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
        file.write_all(data)?;
        Ok(DataFile(path))
    }

    /// The path as gpg gets it: forward slashes, which Git for Windows'
    /// gpg and a native one read alike.
    fn arg(&self) -> OsString {
        if cfg!(windows) {
            OsString::from(self.0.to_string_lossy().replace('\\', "/"))
        } else {
            self.0.clone().into_os_string()
        }
    }
}

impl Drop for DataFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Makes this process's standard handles (git's pipes) private: a
/// gpg-agent that gpg starts would otherwise inherit them, and git would
/// wait for them to close for as long as the agent runs. The handles gpg
/// itself gets are copies made for it.
#[cfg(windows)]
fn private_std_handles() {
    use windows::Win32::Foundation::{HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: plain calls on this process's own handles.
        unsafe {
            if let Ok(handle) = GetStdHandle(which) {
                let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn finds_the_key_of_signing_calls_only() {
        assert_eq!(signing_key(&args(&["--status-fd=2", "-bsau", "436C50D526FA7874"])), Some("436C50D526FA7874".into()));
        assert_eq!(signing_key(&args(&["-u", "ada@example.com", "--detach-sign"])), Some("ada@example.com".into()));
        assert_eq!(signing_key(&args(&["--local-user=ABCD", "-s"])), Some("ABCD".into()));
        assert_eq!(signing_key(&args(&["-uABCD", "-bs"])), Some("ABCD".into()));
        assert_eq!(signing_key(&args(&["--sign"])), Some(String::new()));
        // Verifying.
        assert_eq!(signing_key(&args(&["--keyid-format=long", "--status-fd=1", "--verify", "/tmp/sig", "-"])), None);
        assert_eq!(signing_key(&args(&["-u", "ABCD"])), None);
    }

    #[test]
    fn reads_failure_codes_from_status_lines() {
        let status = "[GNUPG:] KEY_CONSIDERED 00B8 2\n[GNUPG:] BEGIN_SIGNING H10\ngpg: signing failed: No pinentry\n\
                      [GNUPG:] FAILURE sign 67108949\n";
        assert_eq!(failure_codes(status).collect::<Vec<_>>(), vec![NO_PIN_ENTRY]);
        assert_eq!(failure_codes("[GNUPG:] FAILURE sign 67108875\r\n").collect::<Vec<_>>(), vec![BAD_PASSPHRASE]);
        assert_eq!(failure_codes("[GNUPG:] SIG_CREATED D 22 10 00 1794403615 00B8").count(), 0);
    }

    #[test]
    fn only_gnupg_gets_pinentry_options() {
        for program in ["gpg", "gpg2", "C:/Program Files/Git/usr/bin/gpg.exe", "/usr/local/bin/GPG2"] {
            assert!(is_gnupg(OsStr::new(program)), "{program}");
        }
        for program in ["gpgsm", "keybase", "/usr/bin/ssh-keygen"] {
            assert!(!is_gnupg(OsStr::new(program)), "{program}");
        }
    }
}
