# Changelog

Each release's section here becomes its notes on GitHub, and what the update
dialog shows. Add a `## x.y.z` section before tagging `vx.y.z`.

## 0.5.0

- Undo covers a rebase, merge, cherry-pick, revert or `git am` that
  stopped on conflicts and was finished in a terminal or another tool,
  even while GitKrill was closed; one that was aborted leaves nothing to
  undo
- A `git am` stopped on a conflict is undone as a whole once Continue
  finishes it
- Interactive rebase: the commits a merge brought in are listed under it
  and can be reworded, squashed, dropped or reordered one by one; a merge
  can be squashed into the commit below it; a base on a merged-in branch
  works
- Autosquash folds fixups across merges, into merged branches too, and a
  fixup it leaves says why in its menu
- Hunks and lines of a file turned from UTF-8 into UTF-16 (or back) are
  staged, unstaged and discarded in part, each version keeping its own
  encoding; UTF-16 without a byte order mark is recognised in Chinese,
  Japanese, Russian, Arabic and other scripts
- In files over 1 MB or 20,000 lines, a hunk inside a block comment or
  string is coloured as one, once the lines before it have been parsed in
  the background
- In a fork, a Fork / Parent switch shows the parent's pull requests,
  issues and CI, your own pull requests first; Azure DevOps forks know
  their parent and open pull requests into it
- Review states on Gitea, Forgejo and Codeberg, and on GitHub without a
  sign-in
- Bitbucket Data Center servers: pull requests with their reviewers,
  builds, opening and checking out pull requests, the account and its SSH
  keys
- SourceHut: tickets and builds in the sidebar, and how to send patches
- Bitbucket Cloud no longer lists issues: Atlassian removed its issue
  tracker on 20 August 2026
- Asks for the passphrase of X.509 (gpgsm) signing certificates itself too
- On macOS, system notifications come from GitKrill itself instead of
  Script Editor

## 0.4.0

- Select several commits (Ctrl+click, Shift+click): compare any two, see
  their combined changes, cherry-pick, revert or squash them together
- Diffs side by side or as the whole file, and without whitespace changes
- Compare a commit with the working tree, or a branch from where it forked
  ("what it adds on its own")
- Browse every file of a commit, read it as it was, and open, save or
  restore that version; file history and blame from any commit
- Commit options: sign off, skip hooks, co-authors picked from the history
- Merge options: always a merge commit, fast-forward only, or a squash
  merge; fixup commits and autosquash
- Interactive rebase over merge commits
- A reflog panel (View > Reflog), and undo for operations that stopped on
  conflicts and were continued, and for drag actions that switch branches
- Search commits by changed file, code (`git log -S`) or date
- Bisect from a commit's menu, with a banner to answer Good, Bad or Skip
- Show only one branch in the graph
- Remotes: add, edit and remove them from the sidebar
- Ignore a file, folder or extension from its menu; stash with a message,
  only some files, or only what is staged
- Patch files: save commits or changes as patches, and apply them (`git am`
  or to the working tree, with a three-way fallback); export an archive
- Clone options: branch, shallow history, single branch, submodules
- Push all tags at once, delete a tag on a remote
- Submodules and worktrees in the sidebar; Git LFS file locking
- Asks for HTTPS passwords and SSH and GPG passphrases itself when no
  helper or agent answers
- System notifications, and dates in the system's format
- Conflict resolver: the common ancestor, syntax colours, selectable sides,
  line wrapping and your tab size
- UTF-16 files can be blamed, and their hunks and lines staged
- Syntax colours follow the whole file, so a comment or string that starts
  above a hunk is coloured right; TypeScript, Kotlin, Swift, Dart,
  PowerShell, Dockerfile, Vue, Zig and more
- SVG changes shown as pictures, with their text a click away
- Bitbucket, Azure DevOps, Codeberg, Gitea and Forgejo in the sidebar; review
  states and approvals on pull requests; pull requests from and into forks;
  GitLab installed under a path

## 0.3.0

- Preferences (File > Preferences, or Ctrl+,): general, profiles, SSH and
  GPG keys, hosting accounts, commit graph, commit, editor, encoding,
  external tools, Gitflow, Git hooks, LFS, appearance and notifications
- Profiles: switch author identity, SSH key and commit signing from the tab
  strip
- Commit and push, or commit and open a pull request, in one click; commit
  templates
- Gitflow: set up, start and finish features, releases and hotfixes, with no
  extra program to install
- Open conflicts in your merge tool, and files in your diff tool or editor
- Git LFS: track patterns, fetch and pull LFS files
- Custom colour themes, your own date format, toasts in any corner, a
  toolbar without labels
- Hovering a branch dims the commits it does not contain
- Folders of repositories: add one and every repository in it is a click
  away, from the repository menu, the + menu or Ctrl+P
- Hide branches, whole folders of branches, remotes or tags from the graph:
  the eye on their sidebar row, or right-click
- When Git is missing, the installer and the app offer to install it
- UTF-16 files are diffed as text, and a repository can be read in another
  encoding

## 0.2.0

- Windows installer that keeps GitKrill up to date by itself, and portable
  builds for Windows and Linux that ask before updating
- Image previews in diffs: side by side, swipe or onion skin
- Command palette (Ctrl+P) for actions, branches, tags, repositories and commits
- Light theme, or follow the system setting (View > Theme)
- Select and copy text in diffs and blame
- GitLab (gitlab.com and your own server) and GitHub Enterprise in the sidebar
- Open pull and merge requests from GitKrill: drop a branch on the list
- The executable has an icon and version details

## 0.1.0

- First release: commit graph, staging down to single lines, conflict
  resolver, interactive rebase, file history and blame, author pictures,
  dragging branches onto each other, automatic fetch and a GitHub sidebar
