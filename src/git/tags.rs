//! Tags on remotes: pushing every tag at once, deleting one on a remote,
//! and deleting one here and on remotes in one go.

use super::cli;
use super::internal::Internal;
use super::ops::{self, GitCmd, RefMove, Step, UndoKind};

/// `git push <remote> --tags`.
pub fn push_all(remote: &str) -> GitCmd {
    GitCmd::new(format!("Push all tags to {remote}"), &["push", remote, "--tags"])
        .done(format!("Pushed all tags to {remote}"))
}

/// `git push <remote> --delete refs/tags/<tag>`.
pub fn delete_remote(remote: &str, tag: &str) -> GitCmd {
    GitCmd::new(format!("Delete tag {tag} from {remote}"), &["push", remote, "--delete", &format!("refs/tags/{tag}")])
        .done(format!("Deleted tag {tag} from {remote}"))
}

/// Deletes the tag here, and on each of `remotes` first: a remote that
/// refuses (no access, offline) stops it while the tag is still here to try
/// again, and one that does not have the tag is fine. Deleting it here only
/// is undoable: the tag comes back where it was, its message with it.
pub fn delete(tag: &str, remotes: &[String]) -> GitCmd {
    if remotes.is_empty() {
        return ops::delete_tag(tag)
            .done(format!("Deleted tag {tag}"))
            .undoable(UndoKind::Refs(vec![RefMove::new(format!("refs/tags/{tag}"))]));
    }
    let mut cmd = GitCmd::empty(format!("Delete tag {tag} here and on {}", remotes.join(", ")))
        .done(format!("Deleted tag {tag} here and on {}", remotes.join(", ")));
    for remote in remotes {
        cmd.steps.push(delete_on(remote, tag));
    }
    cmd.steps.extend(ops::delete_tag(tag).steps);
    cmd
}

/// One remote's part of [`delete`].
fn delete_on(remote: &str, tag: &str) -> Step {
    let args: Vec<String> = ["push", remote, "--delete", &format!("refs/tags/{tag}")].map(str::to_owned).to_vec();
    let (remote, tag) = (remote.to_owned(), tag.to_owned());
    Step::internal(Internal::new(format!("delete {tag} on {remote}"), move |workdir| {
        match cli::run(workdir, &args, cli::Options::default()) {
            Err(e) if e.contains("remote ref does not exist") => Ok(format!("{remote} did not have {tag}")),
            other => other,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    /// A repository with a commit, tags `v1` (lightweight) and `v2`
    /// (annotated), and a bare remote `origin` that has neither.
    fn with_remote(name: &str) -> (Fixture, std::path::PathBuf) {
        let fx = Fixture::new(name);
        fx.write("a.txt", "one\n");
        fx.commit_all("base");
        git(&fx.dir, &["tag", "v1"]);
        git(&fx.dir, &["tag", "-a", "v2", "-m", "Second release"]);
        let bare = fx.dir.with_extension("remote.git");
        let _ = std::fs::remove_dir_all(&bare);
        git(&fx.dir, &["init", "-q", "--bare", &bare.display().to_string()]);
        git(&fx.dir, &["remote", "add", "origin", &bare.display().to_string()]);
        git(&fx.dir, &["push", "-q", "origin", "main"]);
        (fx, bare)
    }

    fn remote_tags(bare: &std::path::Path) -> String {
        git(bare, &["tag", "--list"])
    }

    #[test]
    fn every_tag_is_pushed_at_once() {
        let (fx, bare) = with_remote("tags-push-all");
        assert_eq!(remote_tags(&bare), "");
        fx.run(push_all("origin"));
        assert_eq!(remote_tags(&bare), "v1\nv2");
        // Nothing new: git says so and succeeds.
        fx.run(push_all("origin"));
    }

    #[test]
    fn a_tag_is_deleted_on_the_remote_only() {
        let (fx, bare) = with_remote("tags-delete-remote");
        fx.run(push_all("origin"));
        let done = fx.run(delete_remote("origin", "v1"));
        assert_eq!(remote_tags(&bare), "v2");
        assert!(done.snap.tags.iter().any(|t| t.name == "v1"), "still here");
    }

    #[test]
    fn deleting_here_and_on_the_remote_tolerates_a_remote_without_it() {
        let (fx, bare) = with_remote("tags-delete-both");
        git(&fx.dir, &["push", "-q", "origin", "v2"]);
        let done = fx.run(delete("v2", &["origin".to_owned()]));
        assert_eq!(remote_tags(&bare), "");
        assert!(!done.snap.tags.iter().any(|t| t.name == "v2"));
        assert!(done.undo.is_none(), "the remote's copy cannot be brought back");
        // v1 was never pushed: still deleted here.
        let done = fx.run(delete("v1", &["origin".to_owned()]));
        assert!(done.snap.tags.is_empty());
    }

    #[test]
    fn a_remote_that_refuses_keeps_the_tag_here() {
        let (fx, _) = with_remote("tags-delete-refused");
        git(&fx.dir, &["remote", "add", "gone", &fx.dir.join("no such remote").display().to_string()]);
        let done = fx.run_as(delete("v1", &["gone".to_owned()]), Origin::User);
        assert!(done.result.is_err());
        assert!(done.snap.tags.iter().any(|t| t.name == "v1"));
    }

    #[test]
    fn deleting_an_annotated_tag_here_is_undone_with_its_message() {
        let (fx, _) = with_remote("tags-delete-undo");
        let done = fx.run(delete("v2", &[]));
        assert!(!done.snap.tags.iter().any(|t| t.name == "v2"));
        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(git(&fx.dir, &["tag", "-l", "--format=%(contents:subject)", "v2"]), "Second release");
    }
}
