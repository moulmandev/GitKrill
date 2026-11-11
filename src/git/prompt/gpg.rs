//! `gpg.program` and `gpg.x509.program` for GitKrill's git commands: the
//! program git would have run (gpg for OpenPGP keys, gpgsm for X.509
//! certificates) does the work, and when gpg-agent does not hold the
//! signing key's passphrase the user is asked in the app.
//!
//! git signs with `<program> --status-fd=2 -bsau <key>`, the data on
//! standard input, the signature on standard output and the program's
//! status lines on standard error. The wrapper first runs that with
//! `--pinentry-mode error`: when the agent has the passphrase cached it
//! signs and nothing is asked. When it would need a pinentry (gpg's "No
//! pinentry" error), the wrapper asks the app for the passphrase and runs
//! it again with `--pinentry-mode loopback --batch --passphrase-fd 0`, the
//! passphrase on standard input and the data in a temporary file; the
//! agent caches the passphrase as it does for its own pinentry. A wrong
//! passphrase is asked again, twice. This works with GnuPG 2.1 and later
//! (Git for Windows' own gpg and gpgsm, Gpg4win, GPG Suite, Linux
//! distributions) unless the agent was set up with
//! `no-allow-loopback-pinentry`.
//!
//! git runs both programs with the same arguments, so the wrapper tells
//! them apart the way git chose between them ([`Setup::format_of`]): it
//! signs in the format `gpg.format` names, and checks a signature with the
//! program of the format the signature is in. gpg reports its errors in
//! status lines (`FAILURE sign <code>`); gpgsm writes none for these, so
//! its messages are read instead, and it is run with them untranslated.
//!
//! Everything else (verifying, a program that is not GnuPG's, such as
//! smimesign, one that does not know `--pinentry-mode`) is passed through
//! untouched.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{Signing, wipe};
use crate::git::cli;
use crate::prefs::identity::SignFormat;

/// gpg's error codes (`gpg-error.h`), in the low 16 bits of a status
/// line's number.
const NO_PIN_ENTRY: u32 = 85;
const BAD_PASSPHRASE: u32 = 11;
const NO_PASSPHRASE: u32 = 89;

/// How many passphrases are tried before giving up.
const TRIES: usize = 3;

/// How an X.509 signature (gpgsm's) starts: git checks a signature that
/// starts so with `gpg.x509.program`.
const X509_SIGNATURE: &[u8] = b"-----BEGIN SIGNED MESSAGE-----";

/// Runs gpg or gpgsm for git with `args`; returns the exit code to leave
/// with.
pub fn wrap(args: Vec<OsString>) -> i32 {
    #[cfg(windows)]
    private_std_handles();
    let setup = Setup::read();
    let format = setup.format_of(&args);
    let tool = Tool { program: setup.program(format), format };
    let key = signing_key(&args);
    let Some(key) = key.filter(|_| is_gnupg(&tool.program, format)) else {
        return passthrough(&tool.program, &args);
    };

    let mut data = Vec::new();
    if std::io::stdin().read_to_end(&mut data).is_err() {
        return passthrough(&tool.program, &args);
    }
    let first = tool.run(&with(&["--pinentry-mode", "error"], &args, &[]), &data);
    if first.code == 0 || !first.needs(&[NO_PIN_ENTRY]) {
        if first.code != 0 && first.unknown_option() {
            // GnuPG 1, without an agent's pinentry modes.
            return tool.run(&args, &data).emit();
        }
        return first.emit();
    }

    let user = tool.user_id(&key);
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
        let signing = Signing { key: key.clone(), user: user.clone(), retry, format };
        let prompt = format!("Passphrase of the {} key {}", format.label(), user.as_deref().unwrap_or(&key));
        match super::helper::ask(&prompt, "", Some(signing)) {
            Ok(Some(mut passphrase)) => {
                passphrase.push('\n');
                let loopback = ["--pinentry-mode", "loopback", "--batch", "--passphrase-fd", "0"];
                let tail = [OsString::from("-o"), OsString::from("-"), file.arg()];
                let ran = tool.run(&with(&loopback, &args, &tail), passphrase.as_bytes());
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

/// Whether `program` is GnuPG's own for `format`, which knows the pinentry
/// modes: gpg (`gpg`, `gpg2`, `gpg.exe`, a full path to one) for OpenPGP,
/// gpgsm for X.509.
fn is_gnupg(program: &OsStr, format: SignFormat) -> bool {
    let Some(name) = Path::new(program).file_name().map(|name| name.to_string_lossy().to_ascii_lowercase()) else {
        return false;
    };
    match format {
        SignFormat::X509 => name.starts_with("gpgsm"),
        SignFormat::OpenPgp | SignFormat::Ssh => name.starts_with("gpg") && !name.starts_with("gpgsm"),
    }
}

/// What git's configuration says about signing, as the git that started
/// the wrapper reads it (the `-c` options of its command included), and
/// without the programs that are GitKrill itself: those are GitKrill's own
/// `-c gpg.program=...` and `-c gpg.x509.program=...`, which come after the
/// user's settings.
#[derive(Debug, Default, PartialEq)]
struct Setup {
    /// `gpg.format`.
    format: Option<String>,
    /// `gpg.program` or `gpg.openpgp.program`, whichever comes last.
    openpgp: Option<String>,
    /// `gpg.x509.program`.
    x509: Option<String>,
}

impl Setup {
    fn read() -> Setup {
        let mut cmd = Command::new(cli::program());
        cmd.args(["config", "--get-regexp", r"^gpg\.(format|program|openpgp\.program|x509\.program)$"])
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        cli::hide_window(&mut cmd);
        let listing = cmd.output().map(|out| String::from_utf8_lossy(&out.stdout).into_owned()).unwrap_or_default();
        let me = std::env::current_exe().ok().and_then(|exe| exe.canonicalize().ok());
        Setup::parse(&listing, |value| me.is_some() && Path::new(value).canonicalize().ok() == me)
    }

    /// Reads `git config --get-regexp` output, oldest scope first: a later
    /// value wins, as in git, and an empty one stands for git's default.
    /// Programs that `is_me` are left out.
    fn parse(listing: &str, is_me: impl Fn(&str) -> bool) -> Setup {
        let mut setup = Setup::default();
        for (key, value) in listing.lines().filter_map(|line| line.split_once(' ')) {
            let value = value.trim();
            let slot = match key {
                "gpg.format" => &mut setup.format,
                "gpg.program" | "gpg.openpgp.program" => &mut setup.openpgp,
                "gpg.x509.program" => &mut setup.x509,
                _ => continue,
            };
            if key != "gpg.format" && is_me(value) {
                continue;
            }
            *slot = (!value.is_empty()).then(|| value.to_owned());
        }
        setup
    }

    /// Which program git started the wrapper as. git signs in the format
    /// `gpg.format` names; it checks a signature (`--verify <file>`) with
    /// the program of the format the signature is in. SSH has a program
    /// of its own, which is never the wrapper.
    fn format_of(&self, args: &[OsString]) -> SignFormat {
        let signature = verified_file(args).and_then(|file| {
            let mut start = Vec::new();
            std::fs::File::open(file).and_then(|f| f.take(X509_SIGNATURE.len() as u64).read_to_end(&mut start)).ok()?;
            Some(start)
        });
        let x509 = match signature {
            Some(start) => start.starts_with(X509_SIGNATURE),
            None => SignFormat::parse(self.format.as_deref()) == SignFormat::X509,
        };
        if x509 { SignFormat::X509 } else { SignFormat::OpenPgp }
    }

    /// The program git would have run for `format` without GitKrill's `-c`
    /// options: the one configured, else git's default.
    fn program(&self, format: SignFormat) -> OsString {
        let configured = if format == SignFormat::X509 { &self.x509 } else { &self.openpgp };
        OsString::from(configured.as_deref().unwrap_or(format.default_program()))
    }
}

/// The signature file of a verifying call (`--verify <file> -`).
fn verified_file(args: &[OsString]) -> Option<&OsString> {
    let at = args.iter().position(|arg| arg == "--verify")?;
    args.get(at + 1)
}

/// The program a call goes to.
struct Tool {
    program: OsString,
    format: SignFormat,
}

impl Tool {
    /// Runs the program with `args` and `input` (see [`run`]). gpgsm's
    /// messages are what tells why it failed, so they are asked for
    /// untranslated: `LC_MESSAGES` decides unless `LC_ALL` is set, and
    /// gettext does not look at `LANGUAGE` for the C locale.
    fn run(&self, args: &[OsString], input: &[u8]) -> Ran {
        let mut cmd = Command::new(&self.program);
        cmd.args(args);
        if self.format == SignFormat::X509 {
            let all = std::env::var_os("LC_ALL").is_some_and(|value| !value.is_empty());
            cmd.env(if all { "LC_ALL" } else { "LC_MESSAGES" }, "C").env_remove("LANGUAGE");
        }
        run(cmd, input)
    }

    /// Who `key` belongs to, for the dialog: "Ada Lovelace
    /// <ada@example.com>", from the key's user ids, or from an X.509
    /// certificate's subject name and email address.
    fn user_id(&self, key: &str) -> Option<String> {
        let mut args = vec!["--batch", "--with-colons", "--list-secret-keys"];
        if !key.is_empty() {
            args.extend(["--", key]);
        }
        let args: Vec<OsString> = args.into_iter().map(OsString::from).collect();
        let ran = self.run(&args, b"");
        let out = String::from_utf8_lossy(&ran.stdout);
        if self.format == SignFormat::X509 {
            return certificate_owner(&out);
        }
        crate::prefs::identity::parse_secret_keys(&out, 0)
            .into_iter()
            .map(|key| key.user_id)
            .find(|user| !user.is_empty())
    }
}

/// The owner of the first secret certificate in `gpgsm --with-colons
/// --list-secret-keys` output: its subject's common name with the email
/// address gpgsm lists after it (`Ada Lovelace <ada@example.com>`), else
/// whichever of the two it has, else the subject as it is.
fn certificate_owner(listing: &str) -> Option<String> {
    let mut ids: Vec<String> = Vec::new();
    let mut inside = false;
    for line in listing.lines() {
        let fields: Vec<&str> = line.trim_end_matches('\r').split(':').collect();
        match fields[0] {
            "crs" if inside => break,
            "crs" => inside = true,
            "crt" if inside => break,
            "uid" if inside => ids.push(crate::prefs::identity::unescape_colons(fields.get(9).copied().unwrap_or(""))),
            _ => {}
        }
    }
    let subject = ids.first().filter(|id| !id.starts_with('<'))?.clone();
    let email = ids.iter().find(|id| id.starts_with('<') && id.ends_with('>'));
    let name = common_name(&subject);
    match (name, email) {
        (Some(name), Some(email)) => Some(format!("{name} {email}")),
        (Some(name), None) => Some(name),
        (None, Some(email)) => Some(email.trim_start_matches('<').trim_end_matches('>').to_owned()),
        (None, None) => Some(subject),
    }
}

/// The `CN` of a distinguished name as gpgsm writes it (`CN=Lovelace\,
/// Ada,O=Example`): the parts are split at commas no backslash escapes.
fn common_name(subject: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut chars = subject.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => part.extend(chars.next()),
            ',' => parts.push(std::mem::take(&mut part)),
            c => part.push(c),
        }
    }
    parts.push(part);
    parts
        .into_iter()
        .find_map(|part| part.split_once('=').filter(|(attr, _)| attr.trim().eq_ignore_ascii_case("CN")).map(|(_, v)| v.trim().to_owned()))
        .filter(|name| !name.is_empty())
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
    /// Whether gpg failed with one of `codes`: its status lines say, or
    /// (gpgsm) its messages.
    fn needs(&self, codes: &[u32]) -> bool {
        let stderr = String::from_utf8_lossy(&self.stderr);
        failure_codes(&stderr).chain(message_codes(&stderr)).any(|code| codes.contains(&code))
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

/// The error codes gpgsm's untranslated messages name ("gpgsm: error
/// creating signature: No pinentry <GPG Agent>"): it writes no status line
/// when signing fails.
fn message_codes(stderr: &str) -> impl Iterator<Item = u32> + '_ {
    const TEXTS: [(u32, &str); 3] =
        [(NO_PIN_ENTRY, ": No pinentry"), (BAD_PASSPHRASE, ": Bad passphrase"), (NO_PASSPHRASE, ": No passphrase given")];
    stderr
        .lines()
        .filter(|line| !line.starts_with("[GNUPG:]"))
        .flat_map(|line| TEXTS.iter().filter(move |(_, text)| line.contains(text)).map(|(code, _)| *code))
}

/// Runs `cmd` with `input` on its standard input and collects what it
/// prints. A gpg-agent that gpg starts may keep the pipes open long after
/// gpg exited, so they are read on threads and given only a moment once
/// gpg is done.
fn run(mut cmd: Command, input: &[u8]) -> Ran {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cli::hide_window(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let stderr = format!("GitKrill could not run {}: {e}\n", cmd.get_program().to_string_lossy()).into_bytes();
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
            assert!(is_gnupg(OsStr::new(program), SignFormat::OpenPgp), "{program}");
            assert!(!is_gnupg(OsStr::new(program), SignFormat::X509), "{program}");
        }
        for program in ["gpgsm", "keybase", "/usr/bin/ssh-keygen"] {
            assert!(!is_gnupg(OsStr::new(program), SignFormat::OpenPgp), "{program}");
        }
        for program in ["gpgsm", "C:/Program Files (x86)/GnuPG/bin/gpgsm.exe"] {
            assert!(is_gnupg(OsStr::new(program), SignFormat::X509), "{program}");
        }
        for program in ["smimesign", "/opt/homebrew/bin/smimesign", "gpg"] {
            assert!(!is_gnupg(OsStr::new(program), SignFormat::X509), "{program}");
        }
    }

    #[test]
    fn reads_the_settings_without_gitkrill_itself() {
        let me = "C:/Program Files/GitKrill/gitkrill.exe";
        let listing = "gpg.format x509\ngpg.program /usr/local/bin/gpg2\ngpg.x509.program /opt/gnupg/bin/gpgsm\n\
                       gpg.program C:/Program Files/GitKrill/gitkrill.exe\ngpg.x509.program C:/Program Files/GitKrill/gitkrill.exe\n";
        let setup = Setup::parse(listing, |value| value == me);
        assert_eq!(
            setup,
            Setup { format: Some("x509".into()), openpgp: Some("/usr/local/bin/gpg2".into()), x509: Some("/opt/gnupg/bin/gpgsm".into()) }
        );
        assert_eq!(setup.program(SignFormat::X509), OsString::from("/opt/gnupg/bin/gpgsm"));
        assert_eq!(setup.program(SignFormat::OpenPgp), OsString::from("/usr/local/bin/gpg2"));

        // Only GitKrill's own: git's defaults. A later gpg.openpgp.program
        // wins over gpg.program, and an empty value means the default.
        let setup = Setup::parse(&format!("gpg.program {me}\ngpg.x509.program {me}\n"), |value| value == me);
        assert_eq!(setup, Setup::default());
        assert_eq!(setup.program(SignFormat::X509), OsString::from("gpgsm"));
        assert_eq!(setup.program(SignFormat::OpenPgp), OsString::from("gpg"));
        let setup = Setup::parse("gpg.program gpg2\ngpg.openpgp.program /bin/gpg\n", |_| false);
        assert_eq!(setup.program(SignFormat::OpenPgp), OsString::from("/bin/gpg"));
        let setup = Setup::parse("gpg.x509.program smimesign\ngpg.x509.program\ngpg.x509.program \n", |_| false);
        assert_eq!(setup.program(SignFormat::X509), OsString::from("gpgsm"));
    }

    #[test]
    fn calls_go_to_the_program_git_chose() {
        let x509 = Setup { format: Some("x509".into()), ..Setup::default() };
        let openpgp = Setup::default();
        let signing = args(&["--status-fd=2", "-bsau", "AC52F5E21AF70FE6"]);
        assert_eq!(x509.format_of(&signing), SignFormat::X509);
        assert_eq!(openpgp.format_of(&signing), SignFormat::OpenPgp);
        let ssh = Setup { format: Some("ssh".into()), ..Setup::default() };
        assert_eq!(ssh.format_of(&signing), SignFormat::OpenPgp);

        // Checking a signature: its first line decides, whatever gpg.format says.
        let dir = std::env::temp_dir().join(format!("gitkrill-gpg-format-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let smime = dir.join("smime.sig");
        std::fs::write(&smime, "-----BEGIN SIGNED MESSAGE-----\nMIAGCSqGSIb3DQEHAqCAMIACAQEx\n").unwrap();
        let pgp = dir.join("pgp.sig");
        std::fs::write(&pgp, "-----BEGIN PGP SIGNATURE-----\n\niHUEABYKAB0WIQ\n").unwrap();
        let verify = |file: &Path, keyid: bool| {
            let mut list = if keyid { vec![OsString::from("--keyid-format=long")] } else { Vec::new() };
            list.extend(args(&["--status-fd=1", "--verify"]));
            list.push(file.as_os_str().to_owned());
            list.push(OsString::from("-"));
            list
        };
        assert_eq!(openpgp.format_of(&verify(&smime, false)), SignFormat::X509);
        assert_eq!(x509.format_of(&verify(&pgp, true)), SignFormat::OpenPgp);
        // A file that cannot be read: gpg.format.
        assert_eq!(x509.format_of(&verify(&dir.join("missing"), false)), SignFormat::X509);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_gpgsm_failures_from_its_messages() {
        let pinentry = "gpgsm: CRLs not checked due to --disable-crl-checks option\n\
                        gpgsm: error creating signature: No pinentry <GPG Agent>\n";
        let ran = |stderr: &str| Ran { code: 2, stdout: Vec::new(), stderr: stderr.as_bytes().to_vec() };
        assert!(ran(pinentry).needs(&[NO_PIN_ENTRY]));
        assert!(!ran(pinentry).needs(&[BAD_PASSPHRASE, NO_PASSPHRASE]));
        assert!(ran("gpgsm: error creating signature: Bad passphrase <GPG Agent>\r\n").needs(&[BAD_PASSPHRASE]));
        assert!(ran("gpgsm: error creating signature: No passphrase given <Pinentry>\n").needs(&[NO_PASSPHRASE]));
        // Not trusted: something to show, not to ask about.
        let untrusted = "gpgsm: can't sign using 'AC52F5E21AF70FE6': Not trusted\n[GNUPG:] INV_SGNR 10 AC52F5E21AF70FE6\n";
        assert!(!ran(untrusted).needs(&[NO_PIN_ENTRY, BAD_PASSPHRASE, NO_PASSPHRASE]));
        // gpg's status lines still count.
        assert!(ran("[GNUPG:] FAILURE sign 67108949\n").needs(&[NO_PIN_ENTRY]));
    }

    #[test]
    fn names_the_owner_of_a_certificate() {
        let listing = "/home/ada/.gnupg/pubring.kbx\n------------------------\n\
            crs:u:2048:1:AC52F5E21AF70FE6:20261111T204546:20630405T170000:14E94818C26F0773::CN=Ada Test,O=Example::sS::::::23:\n\
            fpr:::::::::3CA1124B703B6720377E6028AC52F5E21AF70FE6:::3CA1124B703B6720377E6028AC52F5E21AF70FE6:\n\
            grp:::::::::E531E5F59B0D1B90C9B81BAC2DBF6A9239DCC7FF:\n\
            uid:u::::::::CN=Ada Test,O=Example:::\n\
            uid:u::::::::<ada@example.com>::\n\
            crs:u:2048:1:0000000000000001:::01::CN=Someone Else::sS::::::23:\n\
            uid:u::::::::CN=Someone Else:::\n";
        assert_eq!(certificate_owner(listing).as_deref(), Some("Ada Test <ada@example.com>"));
        let escaped = "crs:u:2048:1:01:::01::x::sS:\nuid:u::::::::CN=Lovelace\\, Ada,OU=R\\x3aD,O=Example:::\n";
        assert_eq!(certificate_owner(escaped).as_deref(), Some("Lovelace, Ada"));
        let no_name = "crs:u:2048:1:01:::01::x::sS:\nuid:u::::::::O=Example,C=GB:::\nuid:u::::::::<it@example.com>::\n";
        assert_eq!(certificate_owner(no_name).as_deref(), Some("it@example.com"));
        let bare = "crs:u:2048:1:01:::01::x::sS:\nuid:u::::::::O=Example,C=GB:::\n";
        assert_eq!(certificate_owner(bare).as_deref(), Some("O=Example,C=GB"));
        assert_eq!(certificate_owner(""), None);
    }
}
