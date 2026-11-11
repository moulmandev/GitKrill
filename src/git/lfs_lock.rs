//! Git LFS file locking: the locks the LFS server holds for a repository
//! (`git lfs locks`), and locking or unlocking a file (`git lfs lock`,
//! `unlock`). Every call talks to the server: run them in the background.
//!
//! Only offered in repositories that use LFS (`git::lfs::in_use`); a
//! server without the locking API answers with an error, which is what the
//! user is shown.

use std::path::Path;

use serde_json::Value;

use super::cli;
use super::ops::GitCmd;

/// One lock on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lock {
    pub id: String,
    /// Relative to the working tree root, with `/`.
    pub path: String,
    /// Who holds it, as the server names them.
    pub owner: String,
    /// When it was taken, as the server wrote it (RFC 3339).
    pub locked_at: String,
    /// Held by the user, as far as the server could say (`--verify`);
    /// `None` when it was not asked.
    pub ours: Option<bool>,
}

impl Lock {
    /// When it was taken, in the user's date format; the server's text
    /// when it cannot be read.
    pub fn when(&self) -> String {
        chrono::DateTime::parse_from_rfc3339(&self.locked_at)
            .map(|time| crate::state::format_time(time.timestamp()))
            .unwrap_or_else(|_| self.locked_at.clone())
    }
}

fn read_lock(value: &Value, ours: Option<bool>) -> Option<Lock> {
    let text = |v: &Value| v.as_str().map(str::to_owned).or_else(|| v.as_i64().map(|n| n.to_string()));
    Some(Lock {
        id: text(value.get("id")?)?,
        path: value.get("path")?.as_str()?.replace('\\', "/"),
        owner: value.pointer("/owner/name").and_then(Value::as_str).unwrap_or_default().to_owned(),
        locked_at: value.get("locked_at").and_then(Value::as_str).unwrap_or_default().to_owned(),
        ours,
    })
}

/// Reads `git lfs locks --json`: a list of locks, or with `--verify` an
/// object of `ours` and `theirs`.
pub fn parse(json: &str) -> Result<Vec<Lock>, String> {
    let value: Value = serde_json::from_str(json.trim()).map_err(|e| format!("Could not read git-lfs's answer: {e}"))?;
    let list = |value: Option<&Value>, ours: Option<bool>| -> Vec<Lock> {
        value.and_then(Value::as_array).into_iter().flatten().filter_map(|lock| read_lock(lock, ours)).collect()
    };
    let mut locks = match &value {
        Value::Array(_) => list(Some(&value), None),
        Value::Object(_) => {
            let mut locks = list(value.get("ours"), Some(true));
            locks.extend(list(value.get("theirs"), Some(false)));
            locks
        }
        Value::Null => Vec::new(),
        _ => return Err("Could not read git-lfs's answer.".to_owned()),
    };
    locks.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(locks)
}

/// The locks of the repository at `workdir`. Asks the server which are the
/// user's (`--verify`, which needs it to know who is asking), and when it
/// cannot tell, lists them without that. Never asks for credentials in a
/// window: it runs on its own, not on the user's request.
pub fn list(workdir: &Path) -> Result<Vec<Lock>, String> {
    let options = || cli::Options { background: true, ..Default::default() };
    let run = |extra: &[&str]| {
        let mut args = vec!["lfs".to_owned(), "locks".to_owned(), "--json".to_owned()];
        args.extend(extra.iter().map(|a| (*a).to_owned()));
        cli::run(workdir, &args, options())
    };
    match run(&["--verify"]) {
        // git-lfs lists no locks rather than fail when the remote is a
        // folder, which has no LFS server: say that instead.
        Ok(out) if out.contains("file:// URL") || out.contains("standalone transfer agent") => {
            Err(NO_SERVER.to_owned())
        }
        Ok(out) => parse(json_part(&out)),
        Err(verify) => match run(&[]) {
            Ok(out) => parse(json_part(&out)),
            // The plain listing's refusal says more than the verification's.
            Err(e) => Err(if e.trim().is_empty() { verify } else { e }),
        },
    }
}

/// Why a repository whose remote is a folder has no locks.
pub const NO_SERVER: &str = "The remote is a folder or a file:// URL: there is no LFS server to keep locks.";

/// `cli::run` puts standard error after the output; the JSON is the first line.
fn json_part(out: &str) -> &str {
    out.lines().find(|line| line.trim_start().starts_with(['[', '{'])).unwrap_or("[]")
}

/// Locks `path` on the server so nobody else can push changes to it.
pub fn lock(path: &str) -> GitCmd {
    GitCmd::new(format!("Lock {}", file_name(path)), &["lfs", "lock", "--", path]).done(format!("Locked {path}"))
}

/// Gives up the lock on `path`. `force` takes away someone else's (the
/// server may still refuse it to those without the right).
pub fn unlock(path: &str, force: bool) -> GitCmd {
    let mut args = vec!["lfs", "unlock"];
    if force {
        args.push("--force");
    }
    args.extend(["--", path]);
    let label = if force { format!("Force unlock {}", file_name(path)) } else { format!("Unlock {}", file_name(path)) };
    GitCmd::new(label, &args).done(format!("Unlocked {path}"))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::tests::{Fixture, git};

    #[test]
    fn both_listings_are_read() {
        let plain = r#"[{"id":"2","path":"art/b c.psd","owner":{"name":"Ann"},"locked_at":"2026-03-01T10:00:00Z"},
                       {"id":1,"path":"a.bin","owner":{"name":"Bo"},"locked_at":"bad"}]"#;
        let locks = parse(&plain.replace('\n', " ")).unwrap();
        assert_eq!(locks.len(), 2);
        assert_eq!((locks[0].path.as_str(), locks[0].id.as_str(), locks[0].ours), ("a.bin", "1", None));
        assert_eq!(locks[0].when(), "bad", "unreadable times stay as written");
        assert_eq!(locks[1].owner, "Ann");
        let verified = r#"{"ours":[{"id":"1","path":"mine.bin","owner":{"name":"Me"}}],"theirs":[{"id":"2","path":"theirs.bin","owner":{"name":"You"}}],"next_cursor":""}"#;
        let locks = parse(verified).unwrap();
        assert_eq!(locks.iter().map(|l| (l.path.as_str(), l.ours)).collect::<Vec<_>>(), [("mine.bin", Some(true)), ("theirs.bin", Some(false))]);
        assert_eq!(parse("[]").unwrap(), Vec::new());
        assert_eq!(parse("null").unwrap(), Vec::new());
        assert!(parse("not json").is_err());
        assert_eq!(json_part("[]\nsome warning"), "[]");
    }

    #[test]
    fn commands_end_options_before_the_path() {
        assert_eq!(lock("-odd name.psd").steps[0].args, ["lfs", "lock", "--", "-odd name.psd"]);
        assert_eq!(unlock("a.psd", false).steps[0].args, ["lfs", "unlock", "--", "a.psd"]);
        assert_eq!(unlock("a.psd", true).steps[0].args, ["lfs", "unlock", "--force", "--", "a.psd"]);
        assert_eq!(unlock("art/a.psd", true).label, "Force unlock a.psd");
    }

    // ---- a stand-in LFS server ------------------------------------------

    #[derive(Default)]
    struct Server {
        locks: Vec<(String, String, String)>,
        next: u32,
        /// Answer 404 to everything, as a server without locking does.
        no_locking: bool,
    }

    const ME: &str = "Test User";

    fn lock_json(id: &str, path: &str, owner: &str) -> String {
        format!(r#"{{"id":"{id}","path":"{path}","owner":{{"name":"{owner}"}},"locked_at":"2026-01-02T03:04:05Z"}}"#)
    }

    /// Answers one request of git-lfs's locking API.
    fn answer(server: &Mutex<Server>, method: &str, target: &str, body: &str) -> (u16, String) {
        let mut server = server.lock().unwrap();
        if server.no_locking {
            return (404, r#"{"message":"Not Found"}"#.to_owned());
        }
        let path_of = |body: &str| -> String {
            let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            value.get("path").and_then(Value::as_str).unwrap_or_default().to_owned()
        };
        let route = target.split('?').next().unwrap_or(target);
        match (method, route.trim_end_matches('/')) {
            ("POST", "/locks/verify") => {
                let pick = |ours: bool| -> Vec<String> {
                    server.locks.iter().filter(|(_, _, o)| (o == ME) == ours).map(|(id, p, o)| lock_json(id, p, o)).collect()
                };
                (200, format!(r#"{{"ours":[{}],"theirs":[{}]}}"#, pick(true).join(","), pick(false).join(",")))
            }
            ("POST", "/locks") => {
                let path = path_of(body);
                if let Some((id, p, o)) = server.locks.iter().find(|(_, p, _)| *p == path) {
                    return (409, format!(r#"{{"lock":{},"message":"already created lock"}}"#, lock_json(id, p, o)));
                }
                server.next += 1;
                let id = format!("id{}", server.next);
                server.locks.push((id.clone(), path.clone(), ME.to_owned()));
                (201, format!(r#"{{"lock":{}}}"#, lock_json(&id, &path, ME)))
            }
            ("GET", "/locks") => {
                // git-lfs looks a path up before unlocking it.
                let wanted = target.split_once("path=").map(|(_, rest)| decode(rest.split('&').next().unwrap_or_default()));
                let found: Vec<String> = server
                    .locks
                    .iter()
                    .filter(|(_, p, _)| wanted.as_ref().is_none_or(|w| w == p))
                    .map(|(id, p, o)| lock_json(id, p, o))
                    .collect();
                (200, format!(r#"{{"locks":[{}]}}"#, found.join(",")))
            }
            ("POST", route) if route.ends_with("/unlock") => {
                let id = route.trim_start_matches("/locks/").trim_end_matches("/unlock").to_owned();
                let force = body.contains("\"force\":true");
                let Some(at) = server.locks.iter().position(|(i, _, _)| *i == id) else {
                    return (404, r#"{"message":"unknown lock"}"#.to_owned());
                };
                let (id, path, owner) = server.locks[at].clone();
                if owner != ME && !force {
                    return (403, format!(r#"{{"message":"{path} is locked by {owner}"}}"#));
                }
                server.locks.remove(at);
                (200, format!(r#"{{"lock":{}}}"#, lock_json(&id, &path, &owner)))
            }
            _ => (404, r#"{"message":"Not Found"}"#.to_owned()),
        }
    }

    fn decode(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' if i + 2 < bytes.len() => {
                    out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap_or(b'?'));
                    i += 3;
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Serves the locking API on a loopback port until the test ends;
    /// returns the URL to give `lfs.url`.
    fn serve(server: Arc<Mutex<Server>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let server = server.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut stream = stream;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut parts = line.split_whitespace();
                        let (method, target) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned());
                        let mut length = 0;
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                                return;
                            }
                            let header = header.trim();
                            if header.is_empty() {
                                break;
                            }
                            if let Some((name, value)) = header.split_once(':')
                                && name.eq_ignore_ascii_case("content-length")
                            {
                                length = value.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0; length];
                        if reader.read_exact(&mut body).is_err() {
                            return;
                        }
                        let (status, json) = answer(&server, &method, &target, &String::from_utf8_lossy(&body));
                        let reply = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/vnd.git-lfs+json\r\nContent-Length: {}\r\n\r\n{json}",
                            json.len()
                        );
                        if stream.write_all(reply.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        url
    }

    /// A repository using LFS whose `lfs.url` is a stand-in server, or
    /// `None` (the test says so and passes) without git-lfs.
    fn lfs_repo(name: &str, server: Arc<Mutex<Server>>) -> Option<Fixture> {
        let fx = Fixture::new(name);
        if let Err(e) = super::super::lfs::version(&fx.dir) {
            println!("skipped: git-lfs is not installed ({})", e.trim());
            return None;
        }
        fx.run(super::super::lfs::install());
        fx.run(super::super::lfs::track("*.psd"));
        fx.write("art/cover art.psd", "picture");
        fx.write("other.psd", "picture");
        fx.commit_all("art");
        git(&fx.dir, &["config", "lfs.url", &serve(server)]);
        // Never a credential helper: the stand-in asks for no sign-in.
        git(&fx.dir, &["config", "credential.helper", ""]);
        git(&fx.dir, &["config", "lfs.locksverify", "false"]);
        Some(fx)
    }

    #[test]
    fn files_are_locked_listed_and_unlocked() {
        let server = Arc::new(Mutex::new(Server::default()));
        let Some(fx) = lfs_repo("lfs-lock", server.clone()) else { return };
        assert_eq!(list(&fx.dir), Ok(Vec::new()));

        fx.run(lock("art/cover art.psd"));
        server.lock().unwrap().locks.push(("theirs".to_owned(), "other.psd".to_owned(), "Someone Else".to_owned()));
        let locks = list(&fx.dir).unwrap();
        let seen: Vec<_> = locks.iter().map(|l| (l.path.as_str(), l.owner.as_str(), l.ours)).collect();
        assert_eq!(seen, [("art/cover art.psd", ME, Some(true)), ("other.psd", "Someone Else", Some(false))]);
        assert_eq!(locks[0].when(), crate::state::format_time(1_767_323_045));

        // Locked already: git-lfs passes on the server's word.
        let again = fx.run_as(lock("art/cover art.psd"), crate::worker::Origin::User);
        assert!(again.result.is_err());

        // Someone else's lock needs force.
        let refused = fx.run_as(unlock("other.psd", false), crate::worker::Origin::User);
        let message = refused.result.unwrap_err();
        assert!(message.contains("Someone Else") || message.contains("403"), "{message}");
        fx.run(unlock("other.psd", true));
        fx.run(unlock("art/cover art.psd", false));
        assert_eq!(list(&fx.dir), Ok(Vec::new()));
    }

    #[test]
    fn a_server_without_locking_says_so_when_locking() {
        let server = Arc::new(Mutex::new(Server { no_locking: true, ..Server::default() }));
        let Some(fx) = lfs_repo("lfs-lock-unsupported", server) else { return };
        // git-lfs takes a 404 for "no locks" when listing...
        assert_eq!(list(&fx.dir), Ok(Vec::new()));
        // ...and passes the server's word on when locking.
        let refused = fx.run_as(lock("other.psd"), crate::worker::Origin::User);
        assert!(refused.result.unwrap_err().contains("Not Found"));
    }

    #[test]
    fn a_folder_remote_has_no_locks() {
        let server = Arc::new(Mutex::new(Server::default()));
        let Some(fx) = lfs_repo("lfs-lock-folder", server) else { return };
        git(&fx.dir, &["config", "--unset", "lfs.url"]);
        let bare = fx.dir.join("remote.git");
        git(&fx.dir, &["init", "-q", "--bare", bare.to_str().unwrap()]);
        git(&fx.dir, &["remote", "add", "origin", &bare.to_string_lossy().replace('\\', "/")]);
        assert_eq!(list(&fx.dir), Err(NO_SERVER.to_owned()));
    }
}
