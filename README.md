# GitKrill

[![Build](https://github.com/moulmandev/GitKrill/actions/workflows/build.yml/badge.svg)](https://github.com/moulmandev/GitKrill/actions/workflows/build.yml)

A fast, native Git client with a commit graph, written in Rust. Small and
quick, like its namesake: it idles at zero CPU, uses about 100 MB of memory and
shows the first page of a 60,000-commit history in a few tens of milliseconds.

Each repository opens in its own tab, with a ref sidebar, the graph in the
middle, and a staging / commit-details panel on the right.

![Commit graph](docs/graph.png)

![The light theme](docs/light.png)

![Staging individual lines](docs/staging.png)

![Resolving a merge conflict](docs/conflict.png)

![Blame](docs/blame.png)

## Install

Download the build for your system from the
[latest release](https://github.com/moulmandev/GitKrill/releases/latest),
unpack it and run it. Git must be installed and on `PATH`.

Or build it yourself with a recent Rust toolchain:

```sh
cargo run --release            # start with the tabs from last time
cargo run --release -- <path>  # open the repository containing <path>
```

The binary ends up in `target/release/`.

## Builds for Windows, Linux and macOS

`.github/workflows/build.yml` tests and builds all three on GitHub Actions:

- every push: the packaged apps are attached to the workflow run as artifacts;
- pushing a tag such as `v0.2.0`: a GitHub release is published with
  `gitkrill-windows-x64.zip`, `gitkrill-linux-x64.tar.gz` and
  `gitkrill-macos-universal.zip` (Apple silicon and Intel) attached.

The macOS app is not signed with an Apple Developer ID, so the first launch
needs right-click > Open. On Linux the app needs OpenGL and either X11 or
Wayland; nothing else has to be installed.

## What works

**Graph**
- Lanes with rounded merge / fork lines; every branch line gets its own colour
- Branch, remote and tag labels, styled by kind: the checked-out branch is a
  solid pill with a tick, other local branches are muted, remotes and tags are
  outlined (a local branch and its in-sync remote share one label)
- Author pictures from Gravatar and GitHub, cached on disk, initials otherwise
- WIP row for uncommitted changes, stashes shown in place
- Loads history in pages as you scroll; arrow keys walk the graph
- Search by message, author or SHA (`Ctrl+F`, `Enter` / `Shift+Enter` to step)
- Optional Author / Date / SHA columns (gear icon), resizable columns
- Drag a branch onto another branch, a remote or a commit to merge, rebase,
  fast-forward, push or reset

**Working tree**
- Unstaged / staged lists as paths or a folder tree
- Stage, unstage and discard files, folders, hunks or individual lines
- Syntax-highlighted diffs with word-level emphasis of what changed
- Image previews for PNG, JPEG, GIF, BMP, WebP and ICO: before and after
  side by side, a swipe between them, or an onion skin, with sizes and
  dimensions; tiny icons are enlarged with sharp pixels
- Select and copy text in diffs and blame as in an editor: drag, double-click
  a word, triple-click a line, Shift+click to extend. Copies leave out line
  numbers and +/- markers and keep tabs as tabs; picking lines to stage is
  done in the line-number gutter
- Commit, amend, and "stage all and commit" in one click (`Ctrl+Enter`)
- Changes made outside the app (editor, terminal) appear on their own

**Conflicts**
- Merge, rebase, cherry-pick or revert in progress: Continue and Abort
- A resolver for conflicted files: current and incoming side by side, take
  either or both per conflict, edit the result by hand, save and mark resolved
- Binary and delete/modify conflicts: keep one side or delete

**History**
- Checkout (branch, remote branch, commit, tag), create / rename / delete branch
- Merge, rebase, cherry-pick, revert, reset (soft / mixed / hard)
- Interactive rebase: pick, reword, squash, fixup, drop and reorder
- File history (following renames) and blame, from any file's context menu
- Tags: create (lightweight or annotated), delete, push
- Stash, apply, pop, delete
- Undo / redo for commits, checkouts, merges, resets, rebases, branch create
  and delete

**Remotes**
- Fetch, pull (default / ff-only / rebase), push, force push with lease
- Automatic fetch every five minutes
- GitHub, GitHub Enterprise and GitLab (gitlab.com or your own server): open
  pull requests (merge requests on GitLab), issues and CI runs (Actions,
  pipelines) in the sidebar, each request with the CI status of its head;
  check out a pull or merge request as a local branch
- A remote on a server other than github.com or gitlab.com is offered as
  "Connect <server>…": say whether it runs GitLab or GitHub Enterprise and
  its lists appear; right-click the remote to disconnect it again
- Open a pull or merge request: drop a branch on the list, right-click it,
  or use the list's + button. Title and description are filled in from
  the commits, the branch is pushed first when the remote lacks it, and
  "Open on GitHub" (or GitLab) works without signing in

**Shell**
- Multiple repositories in tabs, restored on start; drop a folder to open it
- Dark and light themes, or follow the system setting (**View > Theme**)
- Clone and init
- Open a terminal in the repository
- Command palette (`Ctrl+P`): type a few letters of any action, branch, tag,
  repository or commit SHA and press Enter

Right-click almost anything (commits, labels, branches, tags, stashes, files,
pull requests) for the relevant actions. Double-click a branch to check it out.

## Network and privacy

Three features talk to the network on their own. Each has a switch in the
**View** menu.

| Feature | What is sent | Where |
| --- | --- | --- |
| Author pictures | SHA-256 of each author's email address, or the GitHub user id from a `noreply` address | gravatar.com, avatars.githubusercontent.com |
| Hosting sidebar | Requests for the repository's pull or merge requests, issues and CI runs, and the requests you open, with a token when one is found for that host (below) | api.github.com, gitlab.com, and only the servers you connected |
| Automatic fetch | `git fetch --all --prune`, as you would run it | your remotes |

A server other than github.com and gitlab.com is never sent anything until
you have said what it runs; the answer is kept in the settings file
(`hosting_servers`). Tokens are looked for per host and sent only to that
host's API over HTTPS. They are never stored, logged or shown.

| Service | Token sources, in order |
| --- | --- |
| github.com | `GITHUB_TOKEN`, `GH_TOKEN`, `gh auth token --hostname github.com`, git's credential helper for github.com |
| GitHub Enterprise | `GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN` (unless `GH_HOST` names another server), `gh auth token --hostname <server>`, git's credential helper for the server |
| gitlab.com | `GITLAB_TOKEN` (unless `GITLAB_HOST` names another server), `glab config get token --host gitlab.com`, git's credential helper for gitlab.com |
| Your GitLab | `GITLAB_TOKEN` only when `GITLAB_HOST` names the server, `glab config get token --host <server>`, git's credential helper for the server |

A github.com token never goes to an Enterprise server, nor the other way
round. `gh` and `glab` are run without the token variables, so they answer
with what they keep for that exact host.

## Shortcuts

| Key | Action |
| --- | --- |
| `Ctrl+P` | Command palette |
| `Ctrl+O` | Open repository |
| `Ctrl+W` | Close tab |
| `Ctrl+Tab` | Next tab |
| `Ctrl+F` | Search commits |
| `Ctrl+Enter` | Commit (from the message fields); start an interactive rebase |
| `Ctrl+A` / `Ctrl+C` | Select all / copy the text of a diff or blame |
| `Up` / `Down` | Previous / next commit |
| `F5` | Refresh |
| `Esc` | Close the command palette, the current view, then the diff, then the search bar |
| `Ctrl` `+` / `-` / `0` | Zoom |

## How it is built

- **UI**: [egui](https://github.com/emilk/egui) through eframe with the OpenGL
  backend. The UI only repaints on input or when a background thread has news,
  so it idles at zero CPU. The graph, file lists and diffs are virtualised:
  only visible rows are laid out.
- **Reads** (history, refs, status, diffs) go through libgit2 in-process, on a
  background thread per repository. Nothing on the UI thread touches the disk.
- **Writes** (commit, checkout, merge, push, ...) run your installed `git`, so
  hooks, commit signing, credential helpers, LFS and your config behave exactly
  as they do in a terminal.
- A file watcher (ignoring everything `.gitignore` ignores) triggers refreshes.

`docs/DEVELOPING.md` describes the architecture, the extension points and how
to check UI changes with scripted screenshots.

```
src/
  main.rs        entry point, window setup
  app.rs         tabs, menu bar, status bar, global shortcuts
  state.rs       per-tab state, settings, dialogs, toasts
  worker.rs      reader / operator threads and the file watcher
  theme.rs       colours, fonts, egui style
  avatars.rs     author pictures: fetch, disk cache, textures
  hosting/       pull / merge requests, issues and CI: github.rs (GitHub
                 and Enterprise), gitlab.rs, shared sign-in and network
  git/
    repo.rs      snapshot loading (refs, status, history walk)
    graph.rs     lane layout and line colours
    diff.rs      file diffs and per-hunk / per-line patches
    ops.rs       every mutating operation as git invocations, plus undo
    conflict.rs  three-way conflict regions and resolutions
    rebase.rs    interactive rebase plans
    blame.rs     file history and blame
    cli.rs       running git
  ui/
    graph.rs     commit graph (graph/drag.rs: dragging branches)
    sidebar.rs   refs sidebar        hosting.rs  hosting sections
    new_pull.rs  creating a pull or merge request
    panel.rs     staging / details   files.rs    changed-file lists
    diff.rs      diff view           highlight.rs  syntax colours
    conflict.rs  conflict resolver   rebase.rs   interactive rebase
    history.rs   file history, blame
    toolbar.rs   toolbar             dialogs.rs  modals and toasts
    palette.rs   command palette (Ctrl+P)
    widgets.rs   shared widgets
  shot.rs        scripted screenshots for development
  tests.rs       end-to-end backend tests against real temp repositories
tools/           demo repository and screenshot scripts
```

`cargo test` runs the test suite. Settings live in
`%APPDATA%\GitKrill\settings.json`.

## Known gaps

- Undo does not cover a rebase that stopped on conflicts and was continued, or
  drag actions that switch branches first (`git reflog` still has them)
- Pull request review state and merge request approvals
- Pull and merge requests from a fork of the repository
- Bitbucket, Gitea, Azure DevOps and other hosts; GitLab installed under a
  path (`example.com/gitlab`)
- Interactive rebase refuses ranges that contain merge commits
- File history and blame always start from HEAD
- No ancestor pane or syntax colours in the conflict resolver, and text in
  its two side panes cannot be selected (the result can, in its editor)
- Syntax grammars are syntect's bundled set: TypeScript is coloured as
  JavaScript, and Kotlin, Swift, Dart and PowerShell stay plain
- SSH keys with a passphrase need an agent; there is no passphrase prompt
- Submodule management, worktrees, Git LFS controls; SVG changes show as text

## License

MIT. See [LICENSE](LICENSE).
