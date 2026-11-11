//! Searching the commit graph: the query language of the search bar, and the
//! `git log` run that answers what only git can (the files a commit changed,
//! the text its diff added or removed).
//!
//! A query is words plus filters written `key:value`. Words are looked for,
//! as one phrase and ignoring case, in the message, the author and the SHA.
//! Filters narrow that down; every one given must match:
//!
//! | Filter | Matches a commit that |
//! | --- | --- |
//! | `path:` (`file:`) | changed a file whose path contains the value, or matches it when it has `*`, `?` or `[` |
//! | `code:` | added or removed the text (`git log -S`) |
//! | `regex:` | changed a line matching the regular expression (`git log -G`) |
//! | `author:` | has an author name or e-mail containing the value |
//! | `after:` (`since:`) | was authored at or after the date |
//! | `before:` (`until:`) | was authored before the date |
//!
//! Values with spaces go in double quotes (`path:"my file.txt"`); dates may
//! also run on unquoted (`after:2 weeks ago`). Several `path:` (or
//! `author:`) filters match any of them. `code:` and `regex:` ignore case unless the value has a
//! capital letter. Only the commits loaded in the graph are searched.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};

use super::{CommitInfo, Oid, RowKind};

/// What one search asks for. Every part given must match.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// Words that are not filters, lowercased: one phrase, looked for in the
    /// message, the author and (as a prefix) the SHA.
    pub text: String,
    /// `author:`, lowercased: the author matches any of them.
    pub authors: Vec<String>,
    /// `path:`: the commit changed a file matching any of them.
    pub paths: Vec<String>,
    /// `code:`: text the commit added or removed.
    pub code: Option<String>,
    /// `regex:`: a regular expression matching a line the commit changed.
    pub regex: Option<String>,
    /// `after:`: authored at or after this time (Unix seconds).
    pub after: Option<i64>,
    /// `before:`: authored before this time.
    pub before: Option<i64>,
}

/// The filters, for the menu that lists them and for parsing.
pub const FILTERS: [(&str, &str); 6] = [
    ("path:", "Changed a file whose path contains this"),
    ("code:", "Added or removed this text (git log -S)"),
    ("regex:", "Changed a line matching this regular expression (git log -G)"),
    ("author:", "Author name or e-mail contains this"),
    ("after:", "On or after a date: 2026-01-31, 2 weeks ago, yesterday"),
    ("before:", "Before a date"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Path,
    Code,
    Regex,
    Author,
    After,
    Before,
}

fn key(name: &str) -> Option<Key> {
    Some(match name.to_ascii_lowercase().as_str() {
        "path" | "file" => Key::Path,
        "code" => Key::Code,
        "regex" => Key::Regex,
        "author" => Key::Author,
        "after" | "since" => Key::After,
        "before" | "until" => Key::Before,
        _ => return None,
    })
}

impl Query {
    /// Reads what was typed in the search bar. `now` is what relative dates
    /// count back from. An error names the filter that cannot be used.
    pub fn parse(input: &str, now: DateTime<Local>) -> Result<Query, String> {
        let tokens = tokenize(input);
        let mut query = Query::default();
        let mut words: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];
            i += 1;
            let found = token.text.split_once(':').filter(|_| !token.quoted).and_then(|(name, value)| {
                // A value in quotes right after the colon: `path:"a b"`.
                Some((key(name)?, value))
            });
            let Some((key, value)) = found else {
                words.push(&token.text);
                continue;
            };
            let mut value = value.to_owned();
            if value.is_empty() && tokens.get(i).is_some_and(|next| next.quoted && next.glued) {
                value = tokens[i].text.clone();
                i += 1;
            }
            if matches!(key, Key::After | Key::Before) {
                // Dates may run on over the next words: `after:2 weeks ago`.
                let mut longest = None;
                let mut joined = value.clone();
                for extra in 0..=2 {
                    if extra > 0 {
                        let Some(next) = tokens.get(i + extra - 1).filter(|t| !t.quoted) else { break };
                        joined = format!("{joined} {}", next.text);
                    }
                    if let Some(time) = parse_date(&joined, now) {
                        longest = Some((extra, time));
                    }
                }
                if value.is_empty() {
                    continue;
                }
                let Some((extra, time)) = longest else {
                    let name = if key == Key::After { "after" } else { "before" };
                    return Err(format!("{name}: cannot read \u{201c}{value}\u{201d} as a date (try 2026-01-31 or 2 weeks ago)"));
                };
                i += extra;
                match key {
                    Key::After => query.after = Some(query.after.map_or(time, |t| t.max(time))),
                    _ => query.before = Some(query.before.map_or(time, |t| t.min(time))),
                }
                continue;
            }
            if value.is_empty() {
                // Still being typed.
                continue;
            }
            match key {
                Key::Path => query.paths.push(value),
                Key::Author => query.authors.push(value.to_lowercase()),
                Key::Code => query.code = Some(value),
                Key::Regex => query.regex = Some(value),
                Key::After | Key::Before => unreachable!(),
            }
        }
        if query.code.is_some() && query.regex.is_some() {
            return Err("Use code: or regex:, not both (git cannot search for both at once)".to_owned());
        }
        query.text = words.join(" ").to_lowercase();
        Ok(query)
    }

    /// Whether nothing at all is asked for.
    pub fn is_empty(&self) -> bool {
        *self == Query::default()
    }

    /// Whether git has to answer part of it (see [`log_args`]).
    pub fn needs_git(&self) -> bool {
        !self.paths.is_empty() || self.code.is_some() || self.regex.is_some()
    }

    /// Whether `commit` matches the parts of the query that the loaded
    /// history answers: words, authors and dates. The WIP row never does.
    pub fn matches(&self, commit: &CommitInfo) -> bool {
        if commit.kind == RowKind::Wip {
            return false;
        }
        if self.after.is_some_and(|after| commit.time < after) || self.before.is_some_and(|before| commit.time >= before) {
            return false;
        }
        if !self.authors.is_empty() {
            let author = commit.author.to_lowercase();
            let email = commit.email.to_lowercase();
            if !self.authors.iter().any(|a| author.contains(a) || email.contains(a)) {
                return false;
            }
        }
        let text = &self.text;
        if text.is_empty() {
            return true;
        }
        let hexish = text.len() >= 4 && text.bytes().all(|b| b.is_ascii_hexdigit());
        commit.summary.to_lowercase().contains(text)
            || commit.author.to_lowercase().contains(text)
            || commit.body_preview.to_lowercase().contains(text)
            || (hexish && commit.oid.to_string().starts_with(text.as_str()))
    }
}

struct Token {
    text: String,
    /// Written in double quotes.
    quoted: bool,
    /// Follows the previous token with no space between (`path:"a b"`).
    glued: bool,
}

/// Splits on whitespace; double quotes keep spaces in.
fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    let mut glued = false;
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            glued = false;
            continue;
        }
        if c == '"' {
            chars.next();
            let text: String = chars.by_ref().take_while(|c| *c != '"').collect();
            tokens.push(Token { text, quoted: true, glued });
            glued = true;
            continue;
        }
        let mut text = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() || c == '"' {
                break;
            }
            text.push(c);
            chars.next();
        }
        tokens.push(Token { text, quoted: false, glued });
        glued = true;
    }
    tokens
}

/// Reads a date as typed after `after:` or `before:`: `2026-01-31`, with a
/// time (`2026-01-31 14:30`), `2026-01`, `2026`, `today`, `yesterday`,
/// `now`, `2 weeks ago` (also `2.weeks.ago`, `2w`), `last month`. A day
/// stands for its start, in local time.
pub fn parse_date(text: &str, now: DateTime<Local>) -> Option<i64> {
    let text = text.trim().to_lowercase().replace(['.', '_'], " ");
    let words: Vec<&str> = text.split_whitespace().collect();
    let midnight = |date: NaiveDate| local(date.and_time(NaiveTime::MIN));
    let today = now.date_naive();
    // `2w`: an amount and a unit in one word.
    let compact = |word: &str| {
        let digits = word.find(|c: char| !c.is_ascii_digit()).filter(|d| *d > 0)?;
        back(now, word[..digits].parse().ok()?, &word[digits..])
    };
    match words.as_slice() {
        [] => None,
        ["now"] => Some(now.timestamp()),
        ["today"] => midnight(today),
        ["yesterday"] => midnight(today.pred_opt()?),
        ["last", unit] => back(now, 1, unit),
        [amount, unit, "ago"] => back(now, amount.parse().ok()?, unit),
        [word, "ago"] => compact(word),
        [first, second] => match first.parse() {
            Ok(amount) => back(now, amount, second),
            Err(_) => {
                let date = NaiveDate::parse_from_str(&first.replace('/', "-"), "%Y-%m-%d").ok()?;
                let time = NaiveTime::parse_from_str(second, "%H:%M:%S")
                    .or_else(|_| NaiveTime::parse_from_str(second, "%H:%M"))
                    .ok()?;
                local(date.and_time(time))
            }
        },
        [word] => compact(word).or_else(|| absolute(word).and_then(local)),
        _ => None,
    }
}

/// `now`, `amount` units back.
fn back(now: DateTime<Local>, amount: u32, unit: &str) -> Option<i64> {
    let unit = unit.strip_suffix('s').filter(|u| u.len() > 1).unwrap_or(unit);
    let seconds = |n: i64| Some(now.timestamp() - n * amount as i64);
    match unit {
        "s" | "sec" | "second" => seconds(1),
        "min" | "minute" => seconds(60),
        "h" | "hour" => seconds(3600),
        "d" | "day" => seconds(86_400),
        "w" | "week" => seconds(7 * 86_400),
        "mo" | "month" => Some(now.checked_sub_months(Months::new(amount))?.timestamp()),
        "y" | "year" => Some(now.checked_sub_months(Months::new(amount.checked_mul(12)?))?.timestamp()),
        _ => None,
    }
}

/// `2026-01-31`, `2026-01-31T14:30`, `2026/01/31`, `2026-01` or `2026`.
fn absolute(word: &str) -> Option<NaiveDateTime> {
    let word = word.replace('/', "-");
    if let Ok(date) = NaiveDate::parse_from_str(&word, "%Y-%m-%d") {
        return Some(date.and_time(NaiveTime::MIN));
    }
    for format in ["%Y-%m-%dt%H:%M:%S", "%Y-%m-%dt%H:%M"] {
        if let Ok(time) = NaiveDateTime::parse_from_str(&word, format) {
            return Some(time);
        }
    }
    if let Some((year, month)) = word.split_once('-')
        && year.len() == 4
        && let (Ok(year), Ok(month)) = (year.parse(), month.parse())
    {
        return NaiveDate::from_ymd_opt(year, month, 1).map(|d| d.and_time(NaiveTime::MIN));
    }
    if word.len() == 4 && let Ok(year) = word.parse::<i32>() && year >= 1970 {
        return NaiveDate::from_ymd_opt(year, 1, 1).map(|d| d.and_time(NaiveTime::MIN));
    }
    None
}

fn local(time: NaiveDateTime) -> Option<i64> {
    Local.from_local_datetime(&time).earliest().map(|t| t.timestamp())
}

/// The `git log` arguments that find, among commits given on standard input,
/// those matching the query's `path:`, `code:` and `regex:` filters. `None`
/// when it has none.
pub fn log_args(query: &Query) -> Option<Vec<String>> {
    if !query.needs_git() {
        return None;
    }
    let mut args: Vec<String> =
        ["log", "--no-walk=unsorted", "--stdin", "--format=%H", "--no-color"].map(str::to_owned).into();
    let pickaxe = match (&query.code, &query.regex) {
        (Some(code), _) => Some(("-S", code)),
        (None, Some(regex)) => Some(("-G", regex)),
        (None, None) => None,
    };
    if let Some((flag, value)) = pickaxe {
        // Smart case: a capital letter means the case matters.
        if !value.chars().any(char::is_uppercase) {
            args.push("--regexp-ignore-case".to_owned());
        }
        args.push(format!("{flag}{value}"));
    }
    args.push("--".to_owned());
    args.extend(query.paths.iter().map(|path| pathspec(path)));
    Some(args)
}

/// A pathspec matching the paths that contain `typed`, ignoring (ASCII)
/// case; with a wildcard in it, the paths it matches.
fn pathspec(typed: &str) -> String {
    let path = typed.replace('\\', "/");
    let path = path.trim_start_matches("./").trim_start_matches('/');
    if path.contains(['*', '?', '[']) {
        format!(":(icase){path}")
    } else {
        format!(":(icase)*{path}*")
    }
}

/// A `git log` running on a thread of its own, collecting the commits it
/// prints. Dropping it stops git.
pub struct Scan {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    found: Mutex<HashSet<Oid>>,
    /// Moves on each time `found` grows or the run ends.
    generation: AtomicUsize,
    done: AtomicBool,
    error: Mutex<Option<String>>,
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Scan {
    /// Starts `git <args>` in `workdir` with `commits` on its standard input,
    /// one SHA a line. `notify` is called when results arrive (from the
    /// scan's thread), at most every few tens of milliseconds.
    pub fn start(workdir: &Path, args: Vec<String>, commits: Vec<Oid>, notify: impl Fn() + Send + 'static) -> Scan {
        let inner = Arc::new(Inner::default());
        let scan = Scan { inner: inner.clone() };
        let workdir = workdir.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("commit-search".to_owned())
            .spawn(move || run(&inner, &workdir, &args, commits, &notify));
        if let Err(e) = spawned {
            *lock(&scan.inner.error) = Some(format!("Could not start the search: {e}"));
            scan.inner.done.store(true, Ordering::Release);
        }
        scan
    }

    /// The commits found so far.
    pub fn found(&self) -> MutexGuard<'_, HashSet<Oid>> {
        lock(&self.inner.found)
    }

    /// Changes whenever [`Scan::found`] or [`Scan::is_done`] does.
    pub fn generation(&self) -> usize {
        self.inner.generation.load(Ordering::Acquire)
    }

    pub fn is_done(&self) -> bool {
        self.inner.done.load(Ordering::Acquire)
    }

    /// Why git failed (a regular expression it cannot read), once done.
    pub fn error(&self) -> Option<String> {
        lock(&self.inner.error).clone()
    }

    /// Stops git. What was found so far stays.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        if let Some(child) = lock(&self.inner.child).as_mut() {
            let _ = child.kill();
        }
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn run(inner: &Inner, workdir: &Path, args: &[String], commits: Vec<Oid>, notify: &dyn Fn()) {
    let finish = |error: Option<String>| {
        if let Some(error) = error {
            *lock(&inner.error) = Some(error);
        }
        inner.done.store(true, Ordering::Release);
        inner.generation.fetch_add(1, Ordering::AcqRel);
        notify();
    };
    let mut cmd = super::cli::command(workdir);
    // The pathspecs use magic (`:(icase)`) and wildcards on purpose.
    for var in ["GIT_LITERAL_PATHSPECS", "GIT_GLOB_PATHSPECS", "GIT_NOGLOB_PATHSPECS"] {
        cmd.env_remove(var);
    }
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return finish(Some(super::cli::NOT_FOUND.to_owned())),
        Err(e) => return finish(Some(format!("Failed to start git: {e}"))),
    };
    let (Some(mut stdin), Some(stdout), Some(mut stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        return finish(Some("Could not talk to git".to_owned()));
    };
    *lock(&inner.child) = Some(child);
    if inner.cancelled.load(Ordering::Acquire) {
        // Cancelled before the child was there to be stopped.
        if let Some(child) = lock(&inner.child).as_mut() {
            let _ = child.kill();
        }
    }

    // Git reads all of its input before it answers; a thread of its own
    // keeps a long list from filling the pipe while nothing reads stdout.
    let writer = std::thread::spawn(move || {
        let mut text = String::with_capacity(commits.len() * 41);
        for oid in commits {
            text.push_str(&oid.to_string());
            text.push('\n');
        }
        // Git may stop early (a bad regex): what it says on stderr matters.
        let _ = stdin.write_all(text.as_bytes());
    });
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let mut last_notice = Instant::now();
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if inner.cancelled.load(Ordering::Acquire) {
            break;
        }
        if let Ok(oid) = Oid::from_str(line.trim()) {
            lock(&inner.found).insert(oid);
            if last_notice.elapsed() >= Duration::from_millis(40) {
                inner.generation.fetch_add(1, Ordering::AcqRel);
                notify();
                last_notice = Instant::now();
            }
        }
    }
    let _ = writer.join();
    let errors = errors.join().unwrap_or_default();
    let status = lock(&inner.child).take().map(|mut child| child.wait());
    if inner.cancelled.load(Ordering::Acquire) {
        return finish(None);
    }
    let failed = match status {
        Some(Ok(status)) => !status.success(),
        _ => true,
    };
    let error = failed.then(|| {
        let message = errors.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("git log failed");
        message.strip_prefix("fatal: ").unwrap_or(message).to_owned()
    });
    finish(error);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};

    fn now() -> DateTime<Local> {
        Local.from_local_datetime(&NaiveDate::from_ymd_opt(2026, 3, 15).unwrap().and_hms_opt(12, 0, 0).unwrap()).unwrap()
    }

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        local(NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, min, 0).unwrap()).unwrap()
    }

    fn parse(input: &str) -> Query {
        Query::parse(input, now()).unwrap()
    }

    #[test]
    fn words_stay_one_phrase_and_filters_come_out() {
        let query = parse("Fix crash path:src/ui author:Ann  in graph");
        assert_eq!(query.text, "fix crash in graph");
        assert_eq!(query.paths, ["src/ui"]);
        assert_eq!(query.authors, ["ann"]);
        assert!(query.needs_git());
        assert!(parse("fix").text == "fix" && !parse("fix").needs_git());
        // An unknown key is just a word; so is a URL.
        assert_eq!(parse("see http://x.org").text, "see http://x.org");
        assert_eq!(parse("TODO: later").text, "todo: later");
    }

    #[test]
    fn quoted_values_keep_their_spaces() {
        let query = parse(r#"path:"my file.txt" code:"fn main()" "two words""#);
        assert_eq!(query.paths, ["my file.txt"]);
        assert_eq!(query.code.as_deref(), Some("fn main()"));
        assert_eq!(query.text, "two words");
        assert_eq!(parse(r#"file:"unterminated name"#).paths, ["unterminated name"]);
    }

    #[test]
    fn a_filter_being_typed_is_left_out() {
        assert!(parse("path:").is_empty());
        assert!(parse("after:").is_empty());
        assert_eq!(parse("code: fix").text, "fix");
    }

    #[test]
    fn dates_absolute_and_relative() {
        let now = now();
        assert_eq!(parse_date("2026-01-31", now), Some(at(2026, 1, 31, 0, 0)));
        assert_eq!(parse_date("2026/01/31", now), Some(at(2026, 1, 31, 0, 0)));
        assert_eq!(parse_date("2026-01-31 14:30", now), Some(at(2026, 1, 31, 14, 30)));
        assert_eq!(parse_date("2026-01-31T14:30", now), Some(at(2026, 1, 31, 14, 30)));
        assert_eq!(parse_date("2026-02", now), Some(at(2026, 2, 1, 0, 0)));
        assert_eq!(parse_date("2025", now), Some(at(2025, 1, 1, 0, 0)));
        assert_eq!(parse_date("today", now), Some(at(2026, 3, 15, 0, 0)));
        assert_eq!(parse_date("Yesterday", now), Some(at(2026, 3, 14, 0, 0)));
        let week = 7 * 86_400;
        for text in ["2 weeks ago", "2 weeks", "2.weeks.ago", "2w", "2w ago", "2 WEEKS AGO"] {
            assert_eq!(parse_date(text, now), Some(now.timestamp() - 2 * week), "{text}");
        }
        assert_eq!(parse_date("3 days ago", now), Some(now.timestamp() - 3 * 86_400));
        assert_eq!(parse_date("1 day ago", now), Some(now.timestamp() - 86_400));
        assert_eq!(parse_date("5 hours ago", now), Some(now.timestamp() - 5 * 3600));
        assert_eq!(parse_date("last month", now), Some(at(2026, 2, 15, 12, 0)));
        assert_eq!(parse_date("1 year ago", now), Some(at(2025, 3, 15, 12, 0)));
        assert_eq!(parse_date("6mo", now), Some(at(2025, 9, 15, 12, 0)));
        for bad in ["", "soon", "2 fortnights ago", "2026-13-01", "12", "-3 days", "2026-02-30"] {
            assert_eq!(parse_date(bad, now), None, "{bad}");
        }
    }

    #[test]
    fn dates_run_on_over_the_next_words() {
        let query = parse("after:2 weeks ago fix before:yesterday");
        assert_eq!(query.after, Some(now().timestamp() - 14 * 86_400));
        assert_eq!(query.before, Some(at(2026, 3, 14, 0, 0)));
        assert_eq!(query.text, "fix");
        let query = parse("since:2026-01-31 crash");
        assert_eq!(query.after, Some(at(2026, 1, 31, 0, 0)));
        assert_eq!(query.text, "crash");
        assert_eq!(parse(r#"until:"2026-01-31 14:30""#).before, Some(at(2026, 1, 31, 14, 30)));
        // Two of the same narrow down.
        assert_eq!(parse("after:2026-01-01 after:2026-02-01").after, Some(at(2026, 2, 1, 0, 0)));
    }

    #[test]
    fn errors_name_the_filter() {
        let error = Query::parse("after:someday", now()).unwrap_err();
        assert!(error.starts_with("after:") && error.contains("someday"), "{error}");
        assert!(Query::parse("code:a regex:b", now()).is_err());
    }

    #[test]
    fn local_filters() {
        let commit = |summary: &str, author: &str, time: i64| CommitInfo {
            oid: Oid::from_str("abcdef0123456789abcdef0123456789abcdef01").unwrap(),
            parents: Vec::new(),
            summary: summary.to_owned(),
            body_preview: "Body text".to_owned(),
            author: author.to_owned(),
            email: "ann@example.com".to_owned(),
            time,
            kind: RowKind::Commit,
        };
        let c = commit("Fix the crash", "Ann Smith", at(2026, 2, 1, 10, 0));
        assert!(parse("crash").matches(&c));
        assert!(parse("body").matches(&c));
        assert!(parse("abcdef01").matches(&c));
        assert!(!parse("bcdef0").matches(&c), "a SHA matches from its start");
        assert!(parse("author:smith crash").matches(&c));
        assert!(parse("author:EXAMPLE.com").matches(&c));
        assert!(!parse("author:bob").matches(&c));
        assert!(parse("after:2026-02-01").matches(&c));
        assert!(!parse("after:2026-02-02").matches(&c));
        assert!(parse("before:2026-02-02").matches(&c));
        assert!(!parse("before:2026-02-01").matches(&c));
        assert!(parse("path:x").matches(&c), "git answers path: on its own");
        let wip = CommitInfo { kind: RowKind::Wip, ..commit("// WIP", "", 0) };
        assert!(!parse("").matches(&wip));
    }

    #[test]
    fn git_arguments() {
        assert_eq!(log_args(&parse("fix author:ann")), None);
        let args = log_args(&parse(r#"path:"src\ui" path:*.rs code:Foo"#)).unwrap();
        assert!(args.starts_with(&["log".to_owned(), "--no-walk=unsorted".to_owned(), "--stdin".to_owned()]));
        assert!(args.contains(&"-SFoo".to_owned()));
        assert!(!args.contains(&"--regexp-ignore-case".to_owned()), "a capital letter makes case matter");
        let paths = &args[args.iter().position(|a| a == "--").unwrap() + 1..];
        assert_eq!(paths, [":(icase)*src/ui*", ":(icase)*.rs"]);
        let args = log_args(&parse("regex:fo+")).unwrap();
        assert!(args.contains(&"-Gfo+".to_owned()) && args.contains(&"--regexp-ignore-case".to_owned()));
        assert_eq!(args.last().map(String::as_str), Some("--"));
    }

    /// Commits of a small history, newest first, and what each one changed.
    fn history(fixture: &Fixture) -> Vec<(Oid, String)> {
        fixture.write("a b.txt", "one\n");
        fixture.commit_all("add a b");
        fixture.write("ünï code.rs", "fn needle() {}\n");
        fixture.commit_all("add unicode file");
        fixture.write("a b.txt", "one\ntwo\n");
        fixture.commit_all("grow a b");
        fixture.write("ünï code.rs", "fn other() {}\n");
        fixture.commit_all("drop the needle");
        fixture.write("deep/dir/Thing.TXT", "Needle in caps\n");
        fixture.commit_all("deep file");
        git(&fixture.dir, &["log", "--format=%H %s"])
            .lines()
            .map(|line| {
                let (sha, subject) = line.split_once(' ').unwrap();
                (Oid::from_str(sha).unwrap(), subject.to_owned())
            })
            .collect()
    }

    /// Runs the git part of `query` over `commits` and names what it found.
    fn scan(fixture: &Fixture, commits: &[(Oid, String)], query: &str) -> Result<Vec<String>, String> {
        let args = log_args(&parse(query)).expect("the query needs git");
        let scan = Scan::start(&fixture.dir, args, commits.iter().map(|c| c.0).collect(), || {});
        let start = Instant::now();
        while !scan.is_done() {
            assert!(start.elapsed() < crate::tests::TIMEOUT, "the search finishes");
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(error) = scan.error() {
            return Err(error);
        }
        let found = scan.found();
        Ok(commits.iter().filter(|c| found.contains(&c.0)).map(|c| c.1.clone()).collect())
    }

    #[test]
    fn finds_commits_by_changed_file_and_by_code() {
        let fixture = Fixture::new("search-git");
        let commits = history(&fixture);
        let names = |query| scan(&fixture, &commits, query).unwrap();
        assert_eq!(names("path:a b.txt"), ["grow a b", "add a b"]);
        assert_eq!(names("path:ünï"), ["drop the needle", "add unicode file"]);
        assert_eq!(names("path:DEEP/DIR"), ["deep file"], "paths ignore case");
        assert_eq!(names("path:*.rs"), ["drop the needle", "add unicode file"]);
        assert_eq!(names("path:a path:thing"), ["deep file", "grow a b", "add a b"], "any of several paths");
        assert_eq!(names("path:nothing-here"), Vec::<String>::new());
        assert_eq!(names("code:needle"), ["deep file", "drop the needle", "add unicode file"]);
        assert_eq!(names("code:Needle"), ["deep file"], "a capital letter makes case matter");
        assert_eq!(names("code:needle path:.rs"), ["drop the needle", "add unicode file"]);
        assert_eq!(names(r#"regex:"fn (needle|other)""#), ["drop the needle", "add unicode file"]);
        let error = scan(&fixture, &commits, r#"regex:"fn (""#).unwrap_err();
        assert!(error.contains("regex"), "{error}");
        // Only the commits given are looked at.
        assert_eq!(scan(&fixture, &commits[..2], "path:ünï").unwrap(), ["drop the needle"]);
    }

    #[test]
    fn stopping_a_scan_leaves_it_done() {
        let fixture = Fixture::new("search-cancel");
        let commits = history(&fixture);
        let args = log_args(&parse("code:needle")).unwrap();
        let scan = Scan::start(&fixture.dir, args, commits.iter().map(|c| c.0).collect(), || {});
        scan.cancel();
        let start = Instant::now();
        while !scan.is_done() {
            assert!(start.elapsed() < crate::tests::TIMEOUT, "a stopped search ends");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(scan.error(), None, "stopping is not a failure");
    }
}
