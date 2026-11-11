//! What the commit panel's options do to a commit: `Co-authored-by:`
//! trailers for the people it is made with, a `Signed-off-by:` line (git's
//! `--signoff`) and skipping the commit hooks (`--no-verify`).
//!
//! Neither trailer is ever added twice. Co-authors already named in the
//! message (by email) are not added again. Git adds a sign-off only when the
//! last trailer is not already the same one, so the co-authors go in before
//! a trailing run of `Signed-off-by:` lines, and when it is known who git
//! signs off as ([`ident_args`]), a message that already has that sign-off
//! anywhere gets no `--signoff` at all.

use super::ops::GitCmd;

pub const CO_AUTHOR: &str = "Co-authored-by";
pub const SIGN_OFF: &str = "Signed-off-by";

/// Someone a commit can be co-authored with.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Person {
    pub name: String,
    pub email: String,
}

impl Person {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Person { name: name.into(), email: email.into() }
    }

    /// Reads `Name <email>`, as typed or as git prints an identity. Both
    /// parts are needed; the email needs an `@` and no spaces.
    pub fn parse(text: &str) -> Option<Person> {
        let text = text.trim();
        let open = text.rfind('<')?;
        let rest = text[open + 1..].strip_suffix('>')?;
        let name = text[..open].trim();
        let email = rest.trim();
        let plausible = !email.is_empty()
            && email.contains('@')
            && !email.contains(char::is_whitespace)
            && !email.contains(['<', '>']);
        (!name.is_empty() && !name.contains(['<', '>', '\n']) && plausible).then(|| Person::new(name, email))
    }

    /// `Name <email>`.
    pub fn ident(&self) -> String {
        format!("{} <{}>", self.name, self.email)
    }

    /// Whether this is the same person as `other`: emails are compared
    /// without regard to case, names not at all.
    pub fn same(&self, other: &Person) -> bool {
        self.email.eq_ignore_ascii_case(&other.email)
    }
}

/// Splits a `Key: value` trailer line, if it is one. Keys are letters,
/// digits and dashes, as git requires.
fn trailer(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    let key_ok = !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    key_ok.then(|| (key, value.trim()))
}

/// The lines of the trailer block at the end of `lines`, as a range: the
/// last paragraph, if it is not the first one (that is the summary) and
/// every line in it is a trailer or a continuation of one.
fn trailer_block(lines: &[&str]) -> Option<std::ops::Range<usize>> {
    let end = lines.iter().rposition(|line| !line.trim().is_empty())? + 1;
    let start = lines[..end].iter().rposition(|line| line.trim().is_empty()).map_or(0, |blank| blank + 1);
    if start == 0 {
        return None;
    }
    let block = &lines[start..end];
    let first_is_trailer = trailer(block[0]).is_some();
    let all = block.iter().all(|line| trailer(line).is_some() || line.starts_with([' ', '\t']));
    (first_is_trailer && all).then_some(start..end)
}

/// Whether `message` names `person` as a co-author already.
fn has_co_author(message: &str, person: &Person) -> bool {
    message.lines().filter_map(trailer).any(|(key, value)| {
        key.eq_ignore_ascii_case(CO_AUTHOR)
            && Person::parse(value).is_some_and(|named| named.same(person))
    })
}

/// Whether `message` carries the sign-off of `ident` (`Name <email>`).
pub fn has_sign_off(message: &str, ident: &str) -> bool {
    message
        .lines()
        .filter_map(trailer)
        .any(|(key, value)| key.eq_ignore_ascii_case(SIGN_OFF) && value == ident.trim())
}

/// `message` with a `Co-authored-by:` trailer for each of `people` it does
/// not name yet. They join the trailer block at the end of the message
/// (before a trailing run of sign-offs, which stays last), or start one.
pub fn with_co_authors(message: &str, people: &[Person]) -> String {
    let mut missing: Vec<&Person> = Vec::new();
    for person in people {
        if !has_co_author(message, person) && !missing.iter().any(|p| p.same(person)) {
            missing.push(person);
        }
    }
    if missing.is_empty() {
        return message.to_owned();
    }
    let added: Vec<String> = missing.iter().map(|p| format!("{CO_AUTHOR}: {}", p.ident())).collect();
    let message = message.trim_end();
    let lines: Vec<&str> = message.lines().collect();
    match trailer_block(&lines) {
        Some(block) => {
            // Before the sign-offs at the very end, if any.
            let mut at = block.end;
            while at > block.start
                && trailer(lines[at - 1]).is_some_and(|(key, _)| key.eq_ignore_ascii_case(SIGN_OFF))
            {
                at -= 1;
            }
            let mut out: Vec<&str> = lines[..at].to_vec();
            out.extend(added.iter().map(String::as_str));
            out.extend_from_slice(&lines[at..]);
            out.join("\n")
        }
        None if message.is_empty() => added.join("\n"),
        None => format!("{message}\n\n{}", added.join("\n")),
    }
}

/// The commit panel's options for one commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitOptions {
    /// `--signoff`.
    pub sign_off: bool,
    /// `--no-verify`: no pre-commit or commit-msg hook.
    pub no_verify: bool,
    pub co_authors: Vec<Person>,
}

impl CommitOptions {
    pub fn is_plain(&self) -> bool {
        !self.sign_off && !self.no_verify && self.co_authors.is_empty()
    }

    /// The message as it is committed: with the co-authors' trailers.
    pub fn message(&self, message: &str) -> String {
        with_co_authors(message, &self.co_authors)
    }

    /// Adds `--signoff` and `--no-verify` to the `git commit` steps of
    /// `cmd`. `message` is the one committed ([`CommitOptions::message`]);
    /// `ident` is who git signs off as, when known: a message carrying that
    /// sign-off already gets no second one.
    pub fn apply(&self, mut cmd: GitCmd, message: &str, ident: Option<&str>) -> GitCmd {
        let sign = self.sign_off && !ident.is_some_and(|ident| has_sign_off(message, ident));
        let flags: Vec<&str> = [(sign, "--signoff"), (self.no_verify, "--no-verify")]
            .into_iter()
            .filter_map(|(on, flag)| on.then_some(flag))
            .collect();
        for step in &mut cmd.steps {
            if step.args.first().is_some_and(|arg| arg == "commit") {
                for (i, flag) in flags.iter().enumerate() {
                    step.args.insert(1 + i, (*flag).to_owned());
                }
            }
        }
        cmd
    }
}

/// The `git var` arguments that answer who the next commit's committer is:
/// `Name <email> <time> <zone>`, read with [`ident_of`].
pub fn ident_args() -> Vec<String> {
    vec!["var".to_owned(), "GIT_COMMITTER_IDENT".to_owned()]
}

/// `Name <email>` out of what `git var GIT_COMMITTER_IDENT` printed.
pub fn ident_of(output: &str) -> Option<String> {
    let end = output.rfind('>')?;
    Some(output[..=end].trim().to_owned())
}

/// The people who wrote commits in the repository, those with the most
/// commits first, as `git shortlog` counts them (`.mailmap` applied).
/// Blocks: run it in a job.
pub fn authors(workdir: &std::path::Path) -> Result<Vec<(Person, usize)>, String> {
    // A cap keeps a huge history from taking seconds; who wrote the latest
    // 20,000 commits is plenty to pick co-authors from.
    let args = ["shortlog", "-s", "-n", "-e", "--all", "--max-count=20000"].map(str::to_owned);
    let output = super::cli::run(workdir, &args, Default::default())?;
    Ok(parse_shortlog(&output))
}

/// Reads `git shortlog -sne`: `   12\tName <email>` per line.
pub fn parse_shortlog(output: &str) -> Vec<(Person, usize)> {
    let mut people: Vec<(Person, usize)> = Vec::new();
    for line in output.lines() {
        let Some((count, who)) = line.trim_start().split_once('\t') else { continue };
        let (Ok(count), Some(person)) = (count.trim().parse::<usize>(), Person::parse(who)) else { continue };
        // The same email under two names: count it once, by its first name.
        match people.iter_mut().find(|(p, _)| p.same(&person)) {
            Some((_, total)) => *total += count,
            None => people.push((person, count)),
        }
    }
    people.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    people
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::ops;
    use crate::tests::{Fixture, git};

    fn alice() -> Person {
        Person::new("Alice Ångström", "alice@example.com")
    }

    fn bob() -> Person {
        Person::new("Bob", "bob@example.com")
    }

    #[test]
    fn people_are_read_from_name_and_email() {
        assert_eq!(Person::parse(" Alice Ångström <alice@example.com> "), Some(alice()));
        assert_eq!(Person::parse("Bob <bob@example.com>").map(|p| p.ident()), Some("Bob <bob@example.com>".to_owned()));
        for bad in ["Bob", "<bob@example.com>", "Bob <bob>", "Bob <bob @example.com>", "Bob <bob@example.com", ""] {
            assert_eq!(Person::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn co_authors_start_a_trailer_block_or_join_one() {
        assert_eq!(
            with_co_authors("Fix the parser", &[alice()]),
            "Fix the parser\n\nCo-authored-by: Alice Ångström <alice@example.com>"
        );
        // A summary that looks like a trailer is still the summary.
        assert_eq!(with_co_authors("Fix: the parser", &[bob()]), "Fix: the parser\n\nCo-authored-by: Bob <bob@example.com>");
        assert_eq!(
            with_co_authors("Fix\n\nBody text.\n\nReviewed-by: Carol <c@example.com>\n", &[bob()]),
            "Fix\n\nBody text.\n\nReviewed-by: Carol <c@example.com>\nCo-authored-by: Bob <bob@example.com>"
        );
        // A body paragraph that is not all trailers is not a trailer block.
        assert_eq!(
            with_co_authors("Fix\n\nNote: this is prose\nand more of it.", &[bob()]),
            "Fix\n\nNote: this is prose\nand more of it.\n\nCo-authored-by: Bob <bob@example.com>"
        );
        // The sign-off stays last.
        assert_eq!(
            with_co_authors("Fix\n\nSigned-off-by: Test User <test@example.com>", &[alice(), bob()]),
            "Fix\n\nCo-authored-by: Alice Ångström <alice@example.com>\nCo-authored-by: Bob <bob@example.com>\n\
             Signed-off-by: Test User <test@example.com>"
        );
    }

    #[test]
    fn co_authors_already_named_are_not_added_again() {
        let message = "Fix\n\nco-authored-by: Alice A. <ALICE@example.com>";
        assert_eq!(with_co_authors(message, &[alice()]), message);
        assert_eq!(
            with_co_authors(message, &[alice(), bob(), bob()]),
            "Fix\n\nco-authored-by: Alice A. <ALICE@example.com>\nCo-authored-by: Bob <bob@example.com>"
        );
        assert_eq!(with_co_authors("Fix", &[]), "Fix");
    }

    #[test]
    fn options_become_commit_flags() {
        let options = CommitOptions { sign_off: true, no_verify: true, co_authors: vec![] };
        let cmd = options.apply(ops::commit_everything("Fix"), "Fix", None);
        assert_eq!(cmd.steps[0].args, ["add", "-A"]);
        assert_eq!(cmd.steps[1].args, ["commit", "--signoff", "--no-verify", "-F", "-"]);
        let amend = options.apply(ops::commit("Fix", true).strip_comments(), "Fix", None);
        assert_eq!(amend.steps[0].args, ["commit", "--signoff", "--no-verify", "--cleanup=strip", "-F", "-", "--amend"]);

        // Signed off already: no second sign-off.
        let signed = "Fix\n\nSigned-off-by: Test User <test@example.com>\nCo-authored-by: Bob <bob@example.com>";
        let cmd = options.apply(ops::commit(signed, false), signed, Some("Test User <test@example.com>"));
        assert_eq!(cmd.steps[0].args, ["commit", "--no-verify", "-F", "-"]);
        // Someone else's sign-off is not ours.
        let cmd = options.apply(ops::commit(signed, false), signed, Some("Other <other@example.com>"));
        assert_eq!(cmd.steps[0].args, ["commit", "--signoff", "--no-verify", "-F", "-"]);
        assert!(CommitOptions::default().is_plain());
    }

    #[test]
    fn shortlog_lines_become_people_most_commits_first() {
        let output = "    12\tAlice Ångström <alice@example.com>\n    40\tBob <bob@example.com>\n     3\tA. Ångström <Alice@Example.com>\n     1\tbroken line\n";
        let people = parse_shortlog(output);
        assert_eq!(people, vec![(bob(), 40), (alice(), 15)]);
        assert_eq!(ident_of("Test User <test@example.com> 1794403273 +0100").as_deref(), Some("Test User <test@example.com>"));
    }

    /// The options as the commit panel uses them, against real git: the
    /// sign-off and the co-authors land once each, also when amending a
    /// commit that has them, and `--no-verify` skips a failing hook.
    #[test]
    fn trailers_land_once_in_real_commits() {
        let fx = Fixture::new("trailers");
        fx.write("a file.txt", "a\n");
        fx.commit_all("first");
        let ident = ident_of(&git(&fx.dir, &["var", "GIT_COMMITTER_IDENT"])).unwrap();
        assert_eq!(ident, "Test User <test@example.com>");

        let options = CommitOptions { sign_off: true, no_verify: false, co_authors: vec![alice()] };
        let commit = |message: &str, amend: bool, ident: Option<&str>| {
            let message = options.message(message);
            let cmd = if amend { ops::commit(&message, true) } else { ops::commit_everything(&message) };
            fx.run(options.apply(cmd, &message, ident));
            git(&fx.dir, &["log", "-1", "--format=%B"])
        };
        fx.write("ü.txt", "u\n");
        let made = commit("Add ü", false, Some(&ident));
        assert_eq!(
            made,
            "Add ü\n\nCo-authored-by: Alice Ångström <alice@example.com>\nSigned-off-by: Test User <test@example.com>"
        );
        // Amending with the message as it is: nothing doubles, whether git
        // or GitKrill decides about the sign-off.
        assert_eq!(commit(&made, true, Some(&ident)), made);
        assert_eq!(commit(&made, true, None), made);

        // A pre-commit hook that refuses everything, then skipped. The
        // repository's own hooks folder, whatever the global config says.
        git(&fx.dir, &["config", "core.hooksPath", ".git/hooks"]);
        let hooks = fx.dir.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\necho no commits today >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(hooks.join("pre-commit"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        fx.write("b.txt", "b\n");
        let refused = fx.run_as(ops::commit_everything("Add b"), crate::worker::Origin::User);
        assert!(refused.result.is_err(), "the hook refuses: {:?}", refused.result);
        let skip = CommitOptions { no_verify: true, ..CommitOptions::default() };
        fx.run(skip.apply(ops::commit_everything("Add b"), "Add b", None));
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%s"]), "Add b");
    }

    #[test]
    fn authors_come_from_the_whole_history() {
        let fx = Fixture::new("trailers-authors");
        fx.write("a.txt", "a\n");
        fx.commit_all("first");
        fx.write("b.txt", "b\n");
        git(&fx.dir, &["add", "-A"]);
        git(&fx.dir, &["commit", "-q", "-m", "second", "--author", "Alice Ångström <alice@example.com>"]);
        fx.write("c.txt", "c\n");
        fx.commit_all("third");
        let people = authors(&fx.dir).unwrap();
        assert_eq!(people, vec![(Person::new("Test User", "test@example.com"), 2), (alice(), 1)]);
    }
}
