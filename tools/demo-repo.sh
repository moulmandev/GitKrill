#!/bin/bash
# Build a small repository that exercises most of the UI: several authors,
# feature branches, merges, a tag, a stash, a remote that is one commit
# behind, and a dirty working tree.
#
#   tools/demo-repo.sh <dir>
#
# <dir> is recreated from scratch; a bare "<dir>-origin.git" is created next
# to it to act as the remote.

set -eu
mkdir -p "$1"
# Absolute, so the remote created below sits next to the repository and not
# inside it when a relative path was given.
dir="$(cd "$1" && pwd)"
rm -rf "$dir" "$dir-origin.git"
mkdir -p "$dir"
cd "$dir"

git init -q -b main
git config user.name "Ada Lovelace"
git config user.email ada@example.com
git config commit.gpgsign false
git config core.autocrlf false

# commit <line> <file> <date> <author> <email> <message>
commit() {
    echo "$1" >> "$2"
    git add -A
    GIT_AUTHOR_NAME="$4" GIT_AUTHOR_EMAIL="$5" GIT_AUTHOR_DATE="$3" GIT_COMMITTER_DATE="$3" \
        git commit -q -m "$6"
}
ada() { commit "$1" "$2" "$3" "Ada Lovelace" ada@example.com "$4"; }
grace() { commit "$1" "$2" "$3" "Grace Hopper" grace@example.com "$4"; }
merge() {
    GIT_AUTHOR_DATE="$2" GIT_COMMITTER_DATE="$2" git merge -q --no-ff "$1" -m "Merge branch '$1'"
}

mkdir -p src/ui docs
printf 'fn main() {\n    println!("hello");\n}\n' > src/main.rs
ada "# Demo" README.md "2026-10-01T09:00:00" "first commit"
ada "mod ui;" src/main.rs "2026-10-02T10:00:00" "Import"

git checkout -q -b feature/login
grace "pub fn login() {}" src/ui/login.rs "2026-10-03T11:00:00" "Add login screen

Sober login form over the main window.
- Remembers the last user"
grace "pub fn logout() {}" src/ui/login.rs "2026-10-04T12:00:00" "Add logout"

git checkout -q main
ada "notes" docs/notes.md "2026-10-05T09:30:00" "Proper auth + manual mapping"

git checkout -q -b hotfix/crash
commit "fix" src/fix.rs "2026-10-06T08:00:00" "Linus T" linus@example.com "Fix crash on startup"

git checkout -q main
merge feature/login "2026-10-07T10:00:00"
git tag v0.1.0
ada "more" docs/notes.md "2026-10-08T10:00:00" "Loading screen, signed mod download from the new API

Sober loading screen over the game window while the loader prepares the game.
- Download the signed mod from the new API"
merge hotfix/crash "2026-10-09T10:00:00"

git checkout -q -b feature/graph
grace "graph" src/ui/graph.rs "2026-10-10T10:00:00" "Draw the commit graph"
grace "graph2" src/ui/graph.rs "2026-10-11T10:00:00" "Curved merge lines"

git checkout -q main
ada "v2" README.md "2026-10-12T10:00:00" "Update README"

# A remote that has everything except the newest commit on main.
git clone -q --bare . "$dir-origin.git"
git remote add origin "$dir-origin.git"
git fetch -q origin
git branch -q -u origin/main main
git update-ref refs/remotes/origin/main HEAD~1

# A stash, then a dirty tree: one modified, one deleted, one untracked, one staged.
echo "stashed work" >> README.md
git stash push -q -m "experiment with colours" -- README.md
printf 'fn main() {\n    println!("hello, world");\n    run();\n}\n\nfn run() {}\nmod ui;\n' > src/main.rs
echo "new file" > src/new_module.rs
echo "staged" >> docs/notes.md
git add docs/notes.md
rm src/fix.rs

echo "demo repository ready at $dir"
