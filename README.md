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

From the [latest release](https://github.com/moulmandev/GitKrill/releases/latest).

GitKrill needs Git to change repositories (it reads them without it). When
Git is missing:

- the Windows installer offers, ticked, to download Git for Windows and
  install it for your account only (no administrator rights; about 65 MB,
  checked against the SHA-256 GitHub publishes for it). Unattended installs
  do that only with `/INSTALLGIT=1`;
- GitKrill itself, any download, says so when it starts and keeps a
  **Git not found** warning in the status bar. On Windows it installs Git
  for Windows the same way in one click; on macOS it starts Apple's Command
  Line Tools install (or use `brew install git`); on Linux it shows the
  command for your distribution.

Git is found on `PATH` or where its installers put it, so a Git installed
while GitKrill runs, or one left off `PATH`, works too.
`gitkrill --git` prints which one is used.

| System | Download | Updates |
| --- | --- | --- |
| Windows | `GitKrill-Setup-x64.exe`: installs for you only, no administrator rights, with a Start menu entry | Automatic: a new version is downloaded in the background and installed when you close GitKrill, or straight away with **Restart to update** in the status bar |
| Windows | `gitkrill-portable-windows-x64.exe`: one file, runs from anywhere, nothing installed | Asks first; on yes it replaces itself and restarts |
| Linux | `gitkrill-portable-linux-x64` (or the same in `gitkrill-linux-x64.tar.gz`) | Asks first; on yes it replaces itself and restarts |
| macOS | `gitkrill-macos-universal.zip` (Apple silicon and Intel) | Asks first, then opens the download page |

Every update is checked against its SHA-256 (GitHub's record of the file and
the release's `SHA256SUMS.txt`) before it is used. **Help > Check for
Updates** looks right away; the automatic check can be switched off there.
"Skip this version" stops a version from being offered again. Both Windows
downloads keep their settings in the same place, so you can switch between
them.

The downloads are not signed with a paid certificate: on first launch Windows
SmartScreen may say it does not know the app (**More info > Run anyway**),
and macOS needs right-click > Open.

Or build it yourself with a recent Rust toolchain:

```sh
cargo run --release            # start with the tabs from last time
cargo run --release -- <path>  # open the repository containing <path>
```

The binary ends up in `target/release/`. Builds made from source never
update themselves.

## Builds for Windows, Linux and macOS

`.github/workflows/build.yml` tests and builds all three on GitHub Actions:

- every push: the packaged apps are attached to the workflow run as artifacts,
  and the Windows installer is tried out (silent install, run, uninstall);
- pushing a tag such as `v0.2.0` (it must match the version in `Cargo.toml`):
  a GitHub release is published with every download above and
  `SHA256SUMS.txt`. Running copies find it within six hours.

To release: bump `version` in `Cargo.toml`, commit, then
`git tag v0.3.0 && git push origin v0.3.0`.

On Linux the app needs OpenGL and either X11 or Wayland; nothing else has to
be installed.

## What works

**Graph**
- Lanes with rounded merge / fork lines; every branch line gets its own colour
- Branch, remote and tag labels, styled by kind: the checked-out branch is a
  solid pill with a tick, other local branches are muted, remotes and tags are
  outlined (a local branch and its in-sync remote share one label)
- Author pictures from Gravatar and GitHub, cached on disk, initials otherwise
- WIP row for uncommitted changes, stashes shown in place
- Loads history in pages as you scroll, or on request, or all at once
  (Preferences > Commit Graph); arrow keys walk the graph
- Search by message, author or SHA (`Ctrl+F`, `Enter` / `Shift+Enter` to step),
  and narrow it with filters, listed by the funnel button in the search bar:
  `path:` for the files a commit changed (part of the path, or a wildcard
  pattern), `code:` for text it added or removed (`git log -S`), `regex:`
  for changed lines matching an expression (`git log -G`), `author:`, and
  `after:` / `before:` with a date (`2026-01-31`, `2 weeks ago`,
  `yesterday`). Git answers file and code filters in the background while
  you type. The loaded commits are searched; the clock button loads older
  ones. "Search commits by changed file / code / date" in the command
  palette start one
- Every column can be hidden or shown (gear icon), resizable columns;
  commit descriptions after the summary always, on hover or never
- Hovering a branch, in the graph or the sidebar, dims the commits it does
  not contain; faded branch and tag labels on the hovered or selected commit
  create one there
- Drag a branch onto another branch, a remote or a commit to merge, rebase,
  fast-forward, push or reset
- Select several commits: `Ctrl`+click adds or removes one, `Shift`+click
  takes every commit in between (`Shift+Up` / `Shift+Down` from the
  keyboard), a plain click or `Esc` goes back to one. The WIP row and
  stashes are always selected on their own. The right panel lists them;
  click one to find it in the graph
- Compare two commits: select both, or use "Compare with HEAD" on a
  commit's right-click menu, or "Compare with main" (the checked-out
  branch) on a branch or tag, in the sidebar or on its graph label. The
  panel lists the files that differ, older commit to newer (Swap turns it
  round), and opens their diffs. Several commits in one unbroken line show
  their combined change instead
- Compare from where two commits forked: "From merge base" in the panel
  (or "Compare with main from the merge base" on a branch or tag) shows
  only what one side adds on its own since their merge base, as
  `git diff main...feature` does, and says which commit the base is
- Compare a commit, branch or tag with the working tree, uncommitted
  changes included ("Compare with working tree" on its right-click menu or
  in the command palette): the panel lists every file that differs from
  the files on disk, staged, unstaged and untracked alike, and follows
  edits as they are saved. Its diffs are read-only and have the same
  inline / split, hunks / whole file and whitespace switches
- Hide branches, whole folders of branches (say, the ones coding agents
  create for their worktrees), remotes or tags from the graph: click the eye on their
  sidebar row or right-click them, here or on their label in the graph.
  Hidden ones stay in the sidebar, dimmed, and their commits leave the graph
  unless something shown still reaches them. A hidden folder also hides
  branches created in it later, one branch inside it can still be shown, and
  the checked out branch is always shown. Remembered per repository
- Show one branch alone: "Show only this branch" on a local or remote
  branch's right-click menu (sidebar or graph label) hides every commit
  that is not in its history, the checked out branch's included. A bar
  above the graph names it, branches shown from the sidebar meanwhile join
  in, and "Show all branches" (or the command palette) puts back exactly
  what was hidden before

**Working tree**
- Unstaged / staged lists as paths or a folder tree
- Stage, unstage and discard files, folders, hunks or individual lines
- Syntax-highlighted diffs with word-level emphasis of what changed, in
  some two hundred languages (bat's grammars: TypeScript and TSX, Kotlin,
  Swift, Dart, PowerShell, Dockerfile, Vue, Svelte, TOML, Zig, Terraform
  and the like). Colours follow the whole file, in the inline and side by
  side layouts and the whole-file view alike: a hunk that starts inside a
  block comment or a multi-line string is coloured as one. In files over
  1 MB or 20,000 lines (up to 16 MB), each hunk shows its own colours first
  while the lines between the hunks are parsed in the background, for up to
  20 seconds, and takes the right ones once that gets to it
- Diffs inline or side by side (the old version on the left, the new one on
  the right, unchanged lines level), as the changed hunks or the whole file,
  and with or without whitespace changes (`git diff -w`): switches in the
  diff's title bar, the file history, the command palette and **Preferences >
  Editor**, remembered across restarts. Hunks and lines stage from either
  side and from the whole file; while whitespace is ignored only whole files
  do, since such a diff does not match the files byte for byte
- Diffs, blame and the conflict resolver in the font, size and tab width you
  pick (**Preferences > Editor**: any installed font, monospace ones listed
  first), with or without line numbers, syntax colours and word wrap
- Image previews for PNG, JPEG, GIF, BMP, WebP and ICO: before and after
  side by side, a swipe between them, or an onion skin, with sizes and
  dimensions; tiny icons are enlarged with sharp pixels. SVG changes are
  drawn the same way, with **Show Text** to see (and stage parts of) their
  text diff and **Show Picture** back, also in the command palette; an SVG
  that does not parse is shown as text
- Select and copy text in diffs, blame and the conflict resolver as in an
  editor: drag, double-click a word, triple-click a line, Shift+click to
  extend. Copies leave out line numbers and +/- markers and keep tabs as
  tabs; picking lines to stage is done in the line-number gutter
- Commit, amend, and "stage all and commit" in one click (`Ctrl+Enter`); the
  button's menu also commits and pushes, or commits and opens a pull request,
  and Preferences > Commit picks which of these the button does
- Commit options (the sliders button beside Amend): sign off (`--signoff`;
  Preferences > Commit > Sign off commits does it for every commit), skip the
  hooks (`--no-verify`) and co-authors (`Co-authored-by:` trailers), picked
  from the authors in the history as you type or typed as `Name <email>`.
  What is on shows as chips under the message, each with a × to remove it.
  They apply to amends and to commit and push or commit and open a pull
  request, never add a trailer the message already has, and go back to
  their defaults after each commit
- Commit templates: git's own `commit.template` fills an empty message (set
  it in Preferences > Commit), a counter shows what is left of 72 characters
  in the summary, and comment lines can be left out of the commit
- Git LFS (Preferences > LFS): set it up in a repository, add and remove
  tracked patterns (or "Track *.psd with LFS" from a file's menu), fetch and
  pull LFS objects
- Git LFS file locking: "Lock file" and "Unlock file" in a changed file's
  right-click menu, and "Force unlock" (after a confirmation) for someone
  else's lock. Locked files carry a lock in the file lists (blue when it is
  yours) and say who holds it and since when; Preferences > LFS (or "Show
  LFS locks" in the command palette) lists every lock with its unlock
  button. Offered in repositories that use LFS; a server without locking
  says so when a file is locked
- Submodules (SUBMODULES in the sidebar): each one's state, not
  initialized, at the recorded commit, checked out at another commit or
  modified inside. Right-click one to initialize and update it (submodules
  inside included), update it to the recorded commit, open it in a new tab,
  sync its URL or remove it; add one (URL, path, optional branch) with the
  section's + or the command palette. A submodule that moved shows in the
  changed-file lists with a package icon and in the diff as "Moved from
  abc1234 to def5678" with both commits' summaries and the commits in
  between; staging it records the new commit
- Worktrees (WORKTREES in the sidebar): every working tree of the
  repository with its branch, locked or missing. Right-click one to open it
  in a new tab or its folder, lock or unlock it, remove it (refused while it
  has changes, with Force remove to delete them), or prune missing ones;
  add one (a folder, a new or existing branch) with the + or the command
  palette, and it opens in a new tab. Branches checked out in another
  worktree carry a folder mark, and checking one out says where it is and
  offers to open that worktree
- Ignore a changed file from its right-click menu: the file itself, every
  file with its extension, or one of its folders (folder rows have "Ignore
  folder" too). The pattern can be edited before it goes in the root
  `.gitignore` or only in this clone (`.git/info/exclude`), and the dialog
  says how many changed files it hides. For a tracked file, "Stop tracking
  and ignore" also takes it out of the index; it stays on disk. Undo takes
  the line out again
- Changes made outside the app (editor, terminal) appear on their own

**Conflicts**
- Merge, rebase, cherry-pick or revert in progress: Continue and Abort
- A resolver for conflicted files: current and incoming side by side, take
  either or both per conflict (its box, or a click on its line numbers), edit
  the result by hand, save and mark resolved
- The common ancestor between the two sides when you want it (**Ancestor** in
  the resolver's toolbar or the command palette, remembered): what the file
  had before either side changed each conflict, and which commit that is (the
  merge base, or the parent of the commit being picked or replayed); a file
  both sides added says it has none
- Syntax colours in every pane of the resolver and in its editor, tabs at the
  tab size of Preferences > Editor, and word wrap (**Wrap** in the toolbar,
  the same switch as Preferences > Editor) that keeps the panes level line by
  line. Text in the sides and the ancestor can be selected and copied as in
  diffs
- Binary and delete/modify conflicts: keep one side or delete
- Or hand a conflicted file to an external merge tool (`git mergetool`):
  the file counts as resolved once the tool has saved it
- Files in another encoding (**Preferences > Encoding**, per repository:
  Windows-1252, ISO-8859-1, Shift_JIS, GBK, Big5, EUC-KR, Cyrillic and more)
  and UTF-16 files with a byte order mark are shown, diffed and resolved as
  text; a resolved file is written back in its own encoding, with the line
  endings set in Preferences > Editor
- UTF-16 files, LE or BE, with a byte order mark or without one (when the
  repository is set to UTF-16, or the file plainly is, in a Latin script or
  any other), are diffed and blamed as text, and their hunks and lines
  staged, unstaged and discarded like any other file's: GitKrill writes the
  result in the file's own encoding, byte order mark and line endings kept.
  So does a file turned from UTF-8 (or the repository's encoding) into
  UTF-16, or back: each version keeps its own encoding while changes are
  staged or discarded in part, and takes the other's once all of them are

**History**
- Checkout (branch, remote branch, commit, tag), create / rename / delete branch
- Delete a whole folder of branches at once (right-click the folder). A
  branch checked out in a worktree goes with that worktree, unless it has
  uncommitted changes or is locked; undo brings the branches back. On a
  remote, the folder's branches are deleted there in one push
- Merge, rebase, cherry-pick, revert, reset (soft / mixed / hard)
- Merge options, under "Merge options" in a branch's right-click menu (sidebar
  or graph label), in the drop menu and in the command palette: always a
  merge commit (`--no-ff`), fast-forward only, or a squash merge, which stages
  what the branch adds as one change and puts a prepared message in the
  commit panel, with a banner to commit it (Continue) or abort it. Each can
  be undone
- Fixup commits: right-click a commit of the checked-out branch for "Fixup
  this commit with staged changes", or "Squash into this commit", which also
  adds the message typed in the commit panel. "Autosquash", on a fixup
  commit's menu or in the command palette, folds every fixup of the branch
  into the commit it names (`git rebase -i --autosquash`, no editor), across
  merges too: a fixup goes to its commit inside a merged branch if that is
  where it is, and the merges are made again (`--rebase-merges`). Commits
  another branch has are never rewritten, and a fixup that is left says why
  in its menu; a conflict stops it like any rebase, and undo puts the branch
  back
- Interactive rebase: pick, reword, squash, fixup, drop and reorder, also
  over merge commits (`--rebase-merges`). A merge is a row of its own, with
  the commits it brought in listed under it, one group per merged branch
  (its caret, or the arrow keys, fold them away): each of them takes any
  action and moves within its group. A merge can be kept, reworded or
  dropped (with what it brought in), take later commits folded into it, or
  be squashed or fixed up into the commit below it, which then gets its
  changes and is no longer a merge (the merged-in commits leave the
  history). Merges whose parents change are made again with their own
  message; commits and merges that do not change keep their SHAs. A base on
  a branch that was merged in works too: the plan starts at the merge that
  brought it in
- Several selected commits at once, from their right-click menu, the panel
  or the command palette: cherry-pick them (oldest first) or revert them
  (newest first) in one git command, so a conflict stops at the commit that
  causes it and Continue or Abort finish or undo the lot; squash a run of
  them on the checked-out branch (the interactive rebase opens with them set
  to squash and their combined message ready to edit, and warns when the
  upstream already has some of them); copy their SHAs
- File history (following renames) and blame, from any file's context menu.
  Opened from a commit's files (or a comparison's) they start at that
  commit, say so in the title bar, and "Go to HEAD" looks at HEAD instead
- Browse every file of a commit: "All files" beside the changed ones above
  a commit's file list, as paths or a folder tree with a filter, quick on
  trees of tens of thousands of files. Click a file to read it as it was
  then, syntax-coloured, selectable and copyable
- Older versions of a file: right-click a file of a commit (changed or
  browsed) or of a comparison to open that version with its default
  program or the editor from Preferences > External Tools, save it
  somewhere ("Save this version as…"), or restore it in the working tree
  (`git restore --source`). Restoring asks first when the file has
  uncommitted changes, and can be undone. The same buttons sit in the title
  bar of a file shown from a commit, and in the command palette
- Bisect: "Start bisect: this commit is bad" (or good) on a commit's
  right-click menu, then answer from the banner above the graph (Good, Bad,
  Skip, Stop) or mark any commit from its menu. The banner names the commit
  being tested and roughly how many steps are left; good, bad and skipped
  commits are labelled in the graph, the one to test is selected, and once
  git names the first bad commit it is selected too. A bisect started in a
  terminal gets the same banner and buttons, its own terms
  (`--term-new=broken`) and paths included
- Tags: create (lightweight or annotated), delete, push. Push all of them
  at once from the TAGS header's right-click menu or the command palette
  (choosing the remote when there are several), delete one on a remote from
  its right-click menu ("Delete from origin…"), and delete one here and, if
  ticked, on the remotes too
- Patch files: "Save as patch…" on a commit's right-click menu (`git
  format-patch`); several selected commits save one file each in a folder,
  numbered oldest first, or all in one file. "Save changes as a patch…" on
  the WIP row's menu (all, staged or unstaged changes, untracked files as new
  files) and on the right-click of the Unstaged and Staged headers
- **File > Apply Patch…** (or "Apply patch…" in the command palette) checks
  the file first and says whether it applies cleanly, needs a three-way merge
  (naming the files that will have conflicts) or does not apply (naming each
  file and why). Mails from `git format-patch` can be committed with their
  authors and messages (`git am`); any patch can go into the working tree,
  staged or not, and what does not apply is saved beside its file as a
  `.rej`. Applying is undoable; a `git am` stopped on a conflict waits with
  Continue, Skip and Abort, and is undone as a whole once finished
- "Export as archive…" on a commit's, branch's or tag's right-click menu
  (the command palette exports what is checked out): a zip, tar.gz or tar of
  its files, by the extension of the name you give, in a folder named after
  the repository and the commit
- Stash, apply, pop, delete. The stash dialog (right-click the WIP row, or
  "Stash…" in the command palette) takes a message and can leave untracked
  files out, keep the staged changes, or stash only the staged changes
  (Git 2.35 or later). "Stash this file" and "Stash folder" in the
  changed-file lists stash just that
- Gitflow without the git-flow program: set it up in **Preferences > Gitflow**
  (the same `gitflow.*` settings git-flow uses), then start and finish
  features, releases and hotfixes from Gitflow in the branch menu, a branch's
  right-click menu or the command palette. Finishing merges without
  fast-forward, tags releases and hotfixes, and can be undone; one that stops
  on a conflict carries on from there when run again
- Undo / redo for commits, checkouts, merges, resets, rebases, branch create
  and delete, adding, editing and removing remotes, deleting a tag and
  applying a patch to the working tree. A rebase, merge, cherry-pick,
  revert or `git am` that stopped on conflicts is undone as a whole once it
  is finished: by Continue (or the commit that concludes a merge) in the
  app, or in a terminal or another tool, even while GitKrill was closed.
  What only finishing leaves behind (the rebase's last reflog entry, the
  merge commit, the picked commits) tells it from an abort or a `--quit`,
  which leave nothing to undo. A drag action that checks out another branch
  first is undone in one step, back on the branch you were on
- Reflog (**View > Reflog**, "Show reflog" in the toolbar's branch menu or on
  a branch's right-click menu, or the command palette): a panel under the
  graph lists where HEAD, or a branch, pointed after every commit,
  checkout, reset, rebase and merge, newest first. Click an entry to find
  its commit in the graph; a commit nothing reaches any more (amended or
  rebased away) is marked unreachable and shows in the details panel.
  Right-click an entry to check it out, create a branch there (to keep a
  lost commit), reset the current branch to it or cherry-pick it

**Remotes**
- Fetch, pull (default / ff-only / rebase), push, force push with lease
- Sign-in prompts: when a push, pull, fetch or clone over HTTPS needs a
  user name and password that no credential helper knows, GitKrill asks
  for both in one dialog, and git hands them to your credential helper (if
  you have one) to keep. Over SSH it asks for a key's passphrase, a
  server's password, or whether to trust a host it has not seen (with the
  fingerprint). A credential helper that answers by itself (Git Credential
  Manager, the macOS keychain) works as before. Cancel makes the operation
  fail with a message saying so; the automatic fetch never asks
- Add a remote with the + on the sidebar's REMOTE header (or the command
  palette): a name (`origin` for the first), a fetch URL, an optional push
  URL, and a first fetch. Right-click a remote to fetch or prune it alone,
  rename it or change its URLs (its remote-tracking branches and the
  branches tracking it follow a rename), or remove it. Removing can be
  undone: the remote comes back with its settings and the upstreams of its
  branches, and its remote-tracking branches with the next fetch
- Automatic fetch, every five minutes unless you pick another interval
  (0 to 60, **File > Preferences > General**), pruning remote branches
  deleted on the remote unless Auto-Prune is off
- Optionally keep submodules at the commits HEAD records after checkout,
  pull, merge, rebase, reset, cherry-pick and revert
- GitHub, GitHub Enterprise, GitLab (gitlab.com or your own server),
  Bitbucket Cloud, your own Bitbucket Data Center, Azure DevOps
  (dev.azure.com and the older visualstudio.com addresses), Codeberg, and
  your own Gitea or Forgejo: open pull requests (merge requests on GitLab),
  issues (work items on Azure DevOps; Bitbucket Cloud removed its issue
  tracker in August 2026, and Bitbucket Data Center leaves issues to Jira)
  and CI runs (Actions, pipelines, Azure Pipelines builds, and on Bitbucket
  Data Center the builds CI servers reported for the open pull requests and
  the ten most recently changed branches) in the sidebar, each request with
  the CI status of its head; check out a pull or merge request as a local
  branch, from a fork too (on Bitbucket and Azure DevOps, which keep no ref
  for it, only from a branch in the repository itself)
- SourceHut (git.sr.ht): the tickets of the tracker named like the
  repository on todo.sr.ht and the repository's builds on builds.sr.ht,
  each section shown when there are any. SourceHut has no pull requests:
  patches go to the project's mailing list with `git send-email`, which the
  sidebar says, linking SourceHut's guide. Its API answers only with a
  sign-in
- What the reviewers say, at the right of each pull request: approved,
  changes requested, or review required (GitHub's review decision, and the
  reviews themselves where no rule requires any; without a sign-in, each
  pull request's reviews, a few per refresh within GitHub's 60 requests an
  hour); on GitLab the approvals, `1/2` while some are missing; on Azure
  DevOps the reviewers' votes, with required reviewers counted the same
  way; on Bitbucket who approved or asked for changes (needs work on
  Bitbucket Data Center); on Gitea, Forgejo
  and Codeberg each reviewer's latest approval or request for changes
  (reviews that count for the branch's rules, dismissed ones left out).
  Reviews are asked again only for pull requests that changed. The
  tooltip says how many approved
- Pull requests from forks have a fork mark, and their tooltip names the
  fork and its branch (`grace:main`)
- In a fork (the service says so, or a remote is called `upstream`), a
  Fork / Parent switch above the lists shows the parent's pull requests,
  issues and CI instead of the fork's own, remembered per repository (also
  in the command palette). There, pull requests from your fork come first,
  their fork mark in the accent colour, and check out from the remote that
  points at the parent, or from its HTTPS address when none does
- A remote on a server other than the public services is offered as
  "Connect <server>…": say whether it runs GitLab, GitHub Enterprise,
  Gitea, Forgejo or Bitbucket Data Center and its lists appear; right-click
  the remote to disconnect it again. A GitLab installed under a path
  (`example.com/gitlab`) is added with that path in Preferences >
  Integrations > GitLab Self-Managed, a Bitbucket Data Center likewise (or
  at once from an HTTPS remote, whose `/scm/` shows the path in front of
  it); their HTTPS and SSH remotes are then recognised. Bitbucket Data
  Center remotes are `https://<server>/scm/<project>/<repo>.git` and
  `ssh://git@<server>:7999/<project>/<repo>.git`, personal repositories
  under `~<user>`
- Open a pull or merge request: drop a branch on the list, right-click it,
  or use the list's + button. Title and description are filled in from
  the commits, the branch is pushed first when the remote lacks it, and
  "Open on GitHub" (or the service it is on) works without signing in
- In a fork, the pull request goes into the parent repository by default
  (Repository in the dialog), from `owner:branch`, on GitHub, GitLab,
  Gitea, Forgejo, Codeberg, Bitbucket, Bitbucket Data Center and Azure
  DevOps; it shows in the parent's list at once. A branch on another
  remote of the same server, such as your fork of the repository shown,
  can be proposed too

**Profiles, SSH and signing** (File > Preferences)
- Profiles: an author name and email, an SSH key and signing settings each;
  switch from the menu at the right of the tab strip or the command palette
  ("Switch to profile ..."). Your global `.gitconfig` follows the profile
  in use, or, with that option off, only GitKrill's own git commands do
- SSH: your system's agent, or a key of the profile's own; generate an
  ed25519 key pair and copy its public key
- Commit and tag signing with OpenPGP, SSH or X.509 keys: pick a secret key
  from GnuPG, generate a new 4096-bit one, sign by default
- Keys with a passphrase: when gpg-agent does not hold the passphrase of
  an OpenPGP key or of an X.509 certificate's key in gpgsm (or a card's
  PIN), GitKrill asks for it in the app and the agent keeps it for its
  usual time; a wrong one is asked again. SSH signing keys, and SSH keys
  not in ssh-agent, ask the same way. Needs GnuPG 2.1 or later (Git for
  Windows' own gpg and gpgsm, Gpg4win, GPG Suite, Linux distributions');
  other X.509 programs, such as smimesign, ask in their own way
- The commit panel shows who the next commit will be by

**Shell**
- Multiple repositories in tabs, restored on start (unless **Remember tabs** is
  off); drop a folder to open it
- Folders of repositories (workspaces): pick a parent folder once (**File >
  Add Folder of Repositories**, or the repository menu) and every Git
  repository in it, up to five levels down, is listed with its branch,
  ahead / behind counts and changed files. Pick one from the repository menu
  to show it in the current tab, from the `+` menu to open a new tab, or type
  its name in the command palette. New repositories are found on each start
  (or with the rescan button); dependency and build folders (`node_modules`,
  `target`, `.venv`, ...) are not searched
- Dark and light themes, or follow the system setting (**View > Theme**);
  custom themes are JSON files in the `themes` folder next to the settings,
  started from a template that **Preferences > UI Customization** writes
- Preferences > UI Customization: where messages appear, dates in the
  system's short date and time format (Windows' region settings, macOS's
  Language & Region, Linux's `LC_TIME`; the default for new installs) or a
  pattern of your own (`DD.MM.YYYY`, `YYYY-MM-DD HH:mm`...), used
  everywhere a date is shown, an icon-only toolbar, initials instead of
  avatars, and a workspace › repository › branch breadcrumb in the toolbar
- Preferences > Notifications: when an operation ends while GitKrill is in
  the background, the taskbar button flashes (the Dock icon bounces) and the
  message appears in a system notification (Windows' notifications, macOS's
  Notification Center, the desktop's notifications on Linux), which can be
  turned off on its own; also when git waits for a password
- Clone and init. The clone dialog's Options pick the branch to check out
  (listed from the remote once the address is typed), a shallow history of
  the last so many commits, only that branch's history, and the submodules
- External tools (**Preferences > External Tools**): a merge tool, a diff
  tool, an editor and a terminal, picked from those installed (VS Code,
  Beyond Compare, KDiff3, Meld, P4Merge, WinMerge, Sublime Merge, JetBrains
  IDEs, Notepad++, Windows Terminal, PowerShell, Git Bash, ...) or typed as
  a command. Changed files open in the diff tool or editor from their context
  menu, the repository in the editor or terminal from its tab's menu
- Command palette (`Ctrl+P`): type a few letters of any action, branch, tag,
  repository or commit SHA and press Enter

Right-click almost anything (commits, labels, branches, tags, stashes, files,
pull requests) for the relevant actions. Double-click a branch to check it out.

## Network and privacy

Five features talk to the network on their own. Each has a switch: updates
in the **Help** menu, LFS locks with the automatic fetch, the others in the
**View** menu.

| Feature | What is sent | Where |
| --- | --- | --- |
| Author pictures | SHA-256 of each author's email address, or the GitHub user id from a `noreply` address | gravatar.com, avatars.githubusercontent.com |
| Hosting sidebar | Requests for the repository's pull or merge requests, issues and CI runs, and the requests you open, with a token when one is found for that host (below). On Bitbucket Data Center, signed in, its ten most recently changed branches, which of their tips and of the pull requests' heads have builds, and the builds of those that changed. Then, for the requests that changed since: their review states (one GraphQL request on GitHub signed in, each pull request's reviews without a sign-in, at most five a refresh and none once fewer than 20 of the hour's requests are left; each merge request's approvals on GitLab; each pull request's reviews on Gitea, Forgejo and Codeberg), the projects GitLab merge requests come from, and once whether the repository is a fork. In a fork switched to its parent, the same for the parent, and the fork's own lists only when shown again. On SourceHut, signed in only: the tracker's tickets and the owner's latest build jobs. While Preferences > Integrations shows an account: whose token it is, its SSH keys, and the public keys you add there | api.github.com, gitlab.com, api.bitbucket.org, codeberg.org, dev.azure.com (app.vssps.visualstudio.com for your Azure DevOps profile), todo.sr.ht and builds.sr.ht (meta.sr.ht for your SourceHut account), and only the servers you connected |
| Automatic fetch | `git fetch --prune --all` (without `--prune` when Auto-Prune is off), as you would run it | your remotes |
| Updates | A request for the latest release shortly after start and every six hours; the download when there is a newer one | api.github.com, github.com and its download host |
| LFS locks | In repositories that use LFS, `git lfs locks` when the file lists show and then at the automatic fetch interval; with automatic fetch off, only when Preferences > LFS opens or a file is locked or unlocked | your LFS server |

The clone dialog's Options ask the address typed there for its branches
(`git ls-remote`, never asking for a password) once you open them.

GitKrill also listens on a port of the loopback address (127.0.0.1, so
nothing outside the computer can reach it) for the questions of its own git
commands (a password, a passphrase); only those commands know the random
token it accepts.

A server other than the public services (github.com, gitlab.com,
bitbucket.org, codeberg.org, Azure DevOps, git.sr.ht) is never sent
anything until you have said what it runs; the answer is kept in the settings file
(`hosting_servers`). Tokens are looked for per host and sent only to that
host's API over HTTPS. They are never stored, logged or shown.

| Service | Token sources, in order |
| --- | --- |
| github.com | `GITHUB_TOKEN`, `GH_TOKEN`, `gh auth token --hostname github.com`, git's credential helper for github.com |
| GitHub Enterprise | `GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN` (unless `GH_HOST` names another server), `gh auth token --hostname <server>`, git's credential helper for the server |
| gitlab.com | `GITLAB_TOKEN` (unless `GITLAB_HOST` names another server), `glab config get token --host gitlab.com`, git's credential helper for gitlab.com |
| Your GitLab | `GITLAB_TOKEN` only when `GITLAB_HOST` names the server, `glab config get token --host <server>`, git's credential helper for the server |
| bitbucket.org | `BITBUCKET_ACCESS_TOKEN` (a repository, project or workspace access token), `BITBUCKET_API_TOKEN` with `BITBUCKET_EMAIL` (an Atlassian API token), `BITBUCKET_APP_PASSWORD` with `BITBUCKET_USERNAME`, git's credential helper for bitbucket.org (its user name and password, or the password as an OAuth token) |
| Azure DevOps | `AZURE_DEVOPS_EXT_PAT` (a personal access token), `az account get-access-token` (after `az login`), git's credential helper for the repository's address on dev.azure.com |
| codeberg.org | `CODEBERG_TOKEN`, `FORGEJO_TOKEN` when `FORGEJO_HOST` names codeberg.org, the login `tea` keeps for codeberg.org, git's credential helper for codeberg.org |
| Your Gitea / Forgejo | `GITEA_TOKEN` when `GITEA_HOST` names the server (`FORGEJO_TOKEN` and `FORGEJO_HOST` for Forgejo), the login `tea` keeps for the server, git's credential helper for the server |
| Your Bitbucket Data Center | `BITBUCKET_SERVER_TOKEN` (an HTTP access token, sent as a bearer token) when `BITBUCKET_SERVER_HOST` names the server, git's credential helper for the server (its user name and password, or the password as an access token) |
| SourceHut (git.sr.ht) | `SRHT_TOKEN` (a personal access token from meta.sr.ht > OAuth 2.0), the `access-token` hut's configuration keeps for sr.ht (not `access-token-cmd`), git's credential helper for git.sr.ht; sent to SourceHut's GraphQL endpoints only |

A server installed under a path is asked about by its name alone
(`example.com` for `example.com/gitlab`, as git's credential helper and
`glab` know it); `GITLAB_HOST` may name it either way.

A github.com token never goes to an Enterprise server, nor the other way
round. `gh` and `glab` are run without the token variables, so they answer
with what they keep for that exact host.

Preferences > Integrations shows which sign-in was found for each host and
the account it belongs to. **Disconnect** there makes GitKrill stop using it
(the host is listed in `hosting_disconnected` in the settings) without
erasing it: git and the other tools keep their sign-in.

## Shortcuts

| Key | Action |
| --- | --- |
| `Ctrl+P` | Command palette |
| `Ctrl+O` | Open repository |
| `Ctrl+W` | Close tab |
| `Up` / `Down`, `Enter` | In a repository list: choose, then open; `Ctrl+Enter` or `Ctrl`+click opens it in a new tab |
| `Ctrl+Tab` | Next tab |
| `Ctrl+F` | Search commits |
| `Ctrl+Enter` | Commit (from the message fields); start an interactive rebase |
| `Ctrl+A` / `Ctrl+C` | Select all / copy the text of a diff or blame |
| `Up` / `Down` | Previous / next commit |
| `Ctrl`+click / `Shift`+click, `Shift+Up` / `Shift+Down` | Select several commits |
| `F5` | Refresh |
| `Esc` | Close the command palette, the current view, then the diff, then the search bar, then go back to one selected commit |
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
  update.rs      checking for, downloading and applying updates
  notification.rs  system notifications
  hosting/       pull / merge requests, issues and CI: github.rs (GitHub
                 and Enterprise), gitlab.rs, bitbucket.rs (Bitbucket
                 Cloud), bitbucket_dc.rs (Bitbucket Data Center), gitea.rs
                 (Gitea, Forgejo, Codeberg), azure.rs (Azure DevOps),
                 sourcehut.rs (SourceHut), shared sign-in and network
                 (signin.rs for all but the first two); account.rs: the
                 accounts and their SSH keys
  git/
    repo.rs      snapshot loading (refs, status, history walk)
    graph.rs     lane layout and line colours
    diff.rs      file diffs and per-hunk / per-line patches
    ops.rs       every mutating operation as git invocations, plus undo
    gitflow.rs   Gitflow setup, start and finish as plain git commands
    conflict.rs  three-way conflict regions and resolutions
    rebase.rs    interactive rebase plans
    reflog.rs    reading reflogs, finding unreachable commits
    carry.rs     undo for operations that stopped on conflicts
    multi.rs     cherry-pick, revert and squash several commits
    merge.rs     merge modes, the squash merge's prepared message
    fixup.rs     fixup commits and autosquash plans
    trailers.rs  sign-off, co-author trailers, skipping hooks
    patch.rs     saving, checking and applying patch files
    archive.rs   exporting a commit as zip or tar.gz
    clone.rs     clone options, listing a remote's branches
    tags.rs      pushing all tags, deleting tags on remotes
    submodule.rs submodules: their state, moves in diffs, commands
    worktree.rs  worktrees: listing them, add, lock, remove, prune
    lfs_lock.rs  Git LFS file locks
    blame.rs     file history and blame
    versions.rs  a commit's file tree, files as they were in a commit
    utf16.rs     staging parts of UTF-16 files, blaming them
    picture.rs   decoding images (and drawing SVGs) for the image preview
    prompt/      git's, ssh's and gpg's questions answered in the app: the
                 askpass helper, the gpg wrapper, the loopback bridge
    cli.rs       running git
  ui/
    graph.rs     commit graph (graph/drag.rs: dragging branches,
                 reach.rs: hovered-branch highlight, ghost.rs: ghost labels)
    sidebar.rs   refs sidebar        hosting.rs  hosting sections
    new_pull.rs  creating a pull or merge request
    panel.rs     staging / details   files.rs    changed-file lists
    diff.rs      diff view           highlight.rs  syntax colours
    conflict.rs  conflict resolver   rebase.rs   interactive rebase
    conflict_text.rs  the resolver's text: tabs, colours, its editor
    history.rs   file history, blame
    browse.rs    every file of a commit   file_view.rs  one of them, whole
    versions.rs  opening, saving and restoring a file as it was
    reflog.rs    the reflog panel
    toolbar.rs   toolbar             dialogs.rs  modals and toasts
    gitflow.rs   Gitflow dialogs, menus and palette entries
    submodules.rs, worktrees.rs  their sidebar sections, menus, dialogs
    lfs_locks.rs LFS locks: file menus, lock icons, the list of locks
    multi/       several selected commits: selecting (pick.rs), their
                 panel, comparison, menu and palette entries
    merge.rs     merge options, the squash merge banner
    fixup.rs     fixup and autosquash menus and palette entries
    commit_options.rs  sign-off, hooks and co-authors of the next commit
    patch.rs     patch menus and the apply dialog
    clone.rs     clone dialog        tags.rs     tags on remotes
    archive.rs   "Export as archive…"
    palette.rs   command palette (Ctrl+P)
    image_diff.rs  image preview     update.rs   update offer, status bar
    prompt.rs    the password and passphrase dialog
    widgets.rs   shared widgets
  shot.rs        scripted screenshots for development
  tests.rs       end-to-end backend tests against real temp repositories
tools/           demo repository and screenshot scripts
installer/       the Windows installer (Inno Setup)
assets/          the app icon
build.rs         the Windows executable's icon and version details
```

`cargo test` runs the test suite. Settings live in
`%APPDATA%\GitKrill\settings.json`.

## Known gaps

- An operation that stopped on conflicts and was finished outside GitKrill
  is not offered for undo when its end looks like giving it up: a rebase
  of a detached HEAD, several cherry-picks, reverts or mails whose last one
  was skipped, or anything committed on top before GitKrill looked (the
  reflog still has them)
- SourceHut is covered on sr.ht only: a SourceHut of your own keeps its
  tickets and builds on hosts of their own, which nothing in a remote
  names. The patches sent to its mailing lists are not listed (the sidebar
  only says how to send them), and builds are found among the owner's
  latest 25 jobs. Other hosts (Gitee, ...) are not covered; Azure DevOps
  SSH keys cannot be listed or added in Preferences > Integrations (it has
  no API for them)
- "Review required" (the branch's rules want approvals not given yet) is
  not shown on Gitea, Forgejo and Codeberg, whose rules only repository
  admins may read, nor on GitHub without a sign-in, where only GraphQL
  says it
- The interactive rebase moves a commit only within its own line of
  history (the branch's own, or that of one merged branch), and autosquash
  leaves a fixup of the repository's first commit when merges follow it (its
  menu says so)
- UTF-16 without a byte order mark and with no Latin text in it is only
  told from other data when its first 4 KiB have a line break or a space
  (or the repository is set to UTF-16)
- In files over 16 MB, and past what 20 seconds of parsing get through in a
  large file (from 70,000 to 200,000 lines of Rust, more of simpler
  formats), syntax colours start afresh at every hunk: a block comment or
  string that runs across the start of a hunk is coloured as if it did not
- On macOS, system notifications are shown through `osascript`, so
  Notification Center lists them under Script Editor rather than GitKrill

## License

MIT. See [LICENSE](LICENSE).
