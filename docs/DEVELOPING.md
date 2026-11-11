# Developing GitKrill

What you need to know to add a feature without reading the whole codebase.

## Shape of the program

- **One UI thread** (egui, immediate mode). It never touches the disk or the
  network. `App::ui` in `src/app.rs` runs every frame: poll background results,
  lay out the panels, apply queued actions.
- **Per repository tab** (`RepoTab` in `src/state.rs`) there are two background
  threads (`src/worker.rs`):
  - the *reader* owns a libgit2 handle and answers refresh / diff / details
    requests and arbitrary read-only *jobs*;
  - the *operator* runs mutating `git` commands (`GitCmd`) one at a time and
    asks the reader for a fresh snapshot afterwards.
- A file watcher marks the tab dirty when files change; the UI then refreshes.
- `RepoSnapshot` (`src/git/mod.rs`) is everything the UI shows about a
  repository at one moment: refs, status, history page, graph layout. It is
  immutable and shared by `Arc`.

Rule of thumb: **read through libgit2 on the reader thread, write through the
`git` CLI on the operator thread.** Writes go through the CLI so hooks,
signing, credential helpers and the user's config behave as in a terminal.

## Extension points

| You want to | Use |
| --- | --- |
| Read something from the repository without blocking the UI | `tab.spawn_job("feature:key", \|loader\| ...)` then `tab.take_result::<T>("feature:key")` on a later frame. The closure gets a `&Loader`; `loader.repo()` is the `git2::Repository`. It may also shell out with `git::cli::run`. |
| Change the repository | Build a `GitCmd` (`src/git/ops.rs`: `GitCmd::new`, `.step`, `GitCmd::empty` + `Step::new(...).env(...)`) and call `tab.run(cmd)`. Progress, toasts and the refresh afterwards are automatic. Name it with a short imperative label; set `.done("Past tense")` for the success toast; `.quiet()` for routine operations. |
| Do something git has no command for (write a file) as a step of a command | `cmd.steps.push(Step::internal(Internal::new("what", move \|workdir\| ...)))` (`src/git/internal.rs`): it runs on the operator thread in its turn, its `Err` stops the command and becomes its error toast, its `Ok` text joins the output. `.undoable(UndoKind::Files(FileUndo { files: vec![FileState::new(path)], undo_steps }))` makes it undoable: the operator reads each file before and after; undo writes back what was there (refusing if the file changed since), then runs `undo_steps`; redo replays the command. `git::ignore::append_step` adds a line to an ignore file this way. |
| Ignore a path, or stash some of the changes | `git::ignore` (patterns that match exactly one file, folder or extension; `ignore(pattern, file, target, untrack)`), `ui::ignore::open`; `git::stash::push(&StashOptions { .. })`, `ui::stash::open` (or `Dialog::Stash(Box::new(StashForm::paths(..)))`). |
| Search the graph, or open the search with a query | `ui::graph::search::start(tab, "path:src/ui")` opens the search bar with that typed in. `git::search::Query::parse` reads the query language (words plus `path:`, `code:`, `regex:`, `author:`, `after:`, `before:`); `Query::matches` answers what the loaded history can, `log_args` gives the `git log` for the rest, and `git::search::Scan` runs it on a thread of its own (results as they come, stopped when dropped). |
| Know whether a bisect is running, or move it on | `tab.bisect.state` (`git::bisect::State`: terms, bad / good / skipped commits, `Progress`), read again by `ui::bisect` whenever the snapshot changes while `snap.state == RepoState::Bisect`, terminal-started bisects included; `git::bisect::read(repo)` from a job. Commands: `git::bisect::start(oid, bad)`, `mark(term, oid)` (a term of `State::terms`, or `"skip"`), `stop()`. |
| Show one branch alone | `ui::solo::enter(tab, "refs/heads/x", "x")` and `ui::solo::leave(tab, &settings)`. It swaps in `Hidden::only(path)` (everything hidden but that ref, the `HEAD` rule hiding the checked out branch too) with `tab.apply_hidden()`, which sends `tab.hidden` to the reader without saving it; while `tab.solo` is set, `set_hidden` changes the view but not the remembered rules. |
| Know what the installed git supports | `git::cli::known_version()` is what `git --version` last answered (no process started); `git::stash::parse_version` reads it. |
| Change several branches and tags in one undoable operation | `.undoable(UndoKind::Refs(vec![RefMove::new("refs/heads/develop"), ...]))`: the operator notes where each ref pointed before and after; undo and redo move them all back (or forth), HEAD included, and refuse if one has moved since. `git::gitflow::finish` uses it. |
| Undo an operation whose inverse is known when it is built, and does not depend on HEAD | `.undoable(UndoKind::Inverse(Box::new(inverse)))`: undo runs `inverse` (labelled `Undo <label>`), redo the command's own steps again. `git::remotes` uses it: removing a remote is undone by adding it back with every setting it had. |
| Know a remote's push URLs, fetch refspecs or the branches tracking it | `git::remotes::read(loader.repo(), name)` in a `tab.spawn_job` (the snapshot's `RemoteInfo` has the fetch URL only). `ui::remotes::open_add`, `open_edit` and `open_remove` open the dialogs. |
| Say what is left when an operation stops on conflicts | `.after_conflict("Finish … again once the merge is committed.")` on the `GitCmd`: added to the "stopped on conflicts" toast. |
| Know which commits are selected | `tab.selection` is the one the details follow (the *focus*). When several are selected, `tab.multi` (`ui::multi`) has them all: `tab.multi.is_active()`, `tab.multi.contains(oid)`, `tab.multi.picked.ordered(&history)` (graph order). `tab.select(...)` selects one on its own; `tab.focus(...)` moves the focus and keeps the others. `ui::multi::compare(tab, shared, a, b)` selects two commits to compare, loading more history first when one is not in the graph yet. |
| Undo a command that stages changes without moving HEAD (`git merge --squash`) | `.undoable(UndoKind::Staged)`: undo is `git reset --merge` (the index and the files it changed go back to HEAD, other local changes stay), redo runs the command again; both expect HEAD where it was. |
| Merge in a chosen way | `git::merge::merge(source, into, MergeMode)` (`Default`, `NoFastForward`, `FastForwardOnly`, `Squash`), or `checkout_and_merge` for a branch that is not checked out; `ui::merge::run` does the same and remembers a squash's branch names for its prepared message, `ui::merge::options_menu` is the "Merge options" submenu. A squash merge waiting to be committed is `snap.squash_message` (git's `SQUASH_MSG`; `ui::merge::squash_pending` also wants changes staged or conflicted), `git::merge::prepared_message` turns it into a summary and description, `git::merge::abort_squash` aborts it. |
| Make fixup commits or fold them in | `git::fixup::fixup(target)` and `squash_into(target, message)` (undone like commits; commands labelled `git::fixup::FIXUP_LABEL…` leave the commit panel's message alone). `ui::fixup::plan(snap)` is the autosquash the checked-out branch allows (`git::fixup::plan` with the other branches as bounds), `git::fixup::autosquash(&plan, &snap.head)` runs it. |
| Run git on several commits | `git::multi::cherry_pick` (oldest first) and `revert` (newest first) are one `git ... --stdin` each, undoable as one HEAD move. `git::multi::check_squash` (reader thread) checks that a run can be squashed; `ui::rebase::RebaseView::squash(base, run, warning)` opens the planner with it set up. `git::multi::facts` says the same from the loaded history alone, cheaply enough for a menu. |
| Know whether the repository uses Gitflow | `snap.gitflow` (`git::gitflow::Config`, read with every snapshot); `ui::gitflow::open_start` / `open_finish` open the dialogs. |
| Show a full-size view instead of the graph | Add state to your variant of `Overlay` (`src/state.rs`), set `tab.overlay = Some(...)`. `ui::overlay` calls your `show(ui, tab, shared, view) -> bool` (false closes). Start with `widgets::overlay_bar` for the title bar. Escape closes overlays. |
| Ask the user something | `shared.show(Dialog::...)` (`src/ui/dialogs.rs`); `Dialog::confirm(title, body, button, cmd)` for destructive confirmations. |
| Tell the user something | `shared.toast(ToastKind::Success \| Error, title, body)`. |
| Offer an action in the command palette (`Ctrl+P`) | Push an `Entry` in `entries` (`src/ui/palette.rs`, see "More actions") whose closure calls the function the button or menu already calls. Leave the entry out while it cannot run. |
| Offer a list of repositories to open | `ui::workspace::list` (filter, workspaces, recent, branch and changes per row); the summaries come from `shared.workspaces.summary`, read in the background |
| Persist a preference | Add a field to your area's struct in `src/prefs/<area>.rs` (held by `Settings::<area>`; per repository: `RepoPrefs` via `prefs::repo` / `prefs::repo_mut`); it is saved automatically. Older top-level fields of `Settings` stay where they are. |
| Show a preference | Its section's page in `src/ui/prefs/<section>.rs` (see "Preferences" below). |
| Read or change git's configuration | `crate::prefs::gitconfig`: `page.global` / `page.repo` on a preferences page (`get`, `get_bool`, `set`, `set_bool`; read in the background, written through `git config`); `gitconfig::read` / `write` / `effective` from background code. |
| Hand a file or folder to the user's merge tool, diff tool, editor or terminal | `crate::external`: `merge(tools, file)` gives a `GitCmd` for `tab.run`; `diff`, `edit` and `terminal` start the program on a short-lived thread. Pass `&shared.settings.tools`. Failures come back as toasts; a `GITKRILL_SHOT` run prints the command instead. To support another tool, add it to `KNOWN` (where to look, its command lines). |
| Give every git command an environment variable | `git::cli::set_session_env` (the SSH key and author of the profile in use). It replaces the whole set and `prefs::identity::apply` owns it: add what you need to `identity::session_env` rather than calling it from elsewhere. |
| Change what the commit button does | `src/ui/commit_button.rs`: the split button (`show`), what runs once a commit succeeded (`follow_up`), the commit template in the message fields (`prefill`). `commit_blocker` and `commit_staged` commit from a button elsewhere (a squash merge's Continue) the way the panel does. |
| Add trailers or flags to the panel's commits | `git::trailers::CommitOptions`: `message` adds `Co-authored-by:` trailers the message does not have yet (before a trailing sign-off), `apply` adds `--signoff` (left out when `ident` already signed the message) and `--no-verify` to the `git commit` steps. `ui::commit_options::options(ctx, tab, prefs)` is what the next commit gets, `sign_off_ident` who git signs off as, `committed` resets them. |
| Ask a hosting account something (who is signed in, its SSH keys) | `hosting::account::AccountSession::start(account, ctx)`, polled from the UI; `account::local_keys`, `parse_public_key` and `fingerprint` read `.pub` files. A host in `settings.hosting_disconnected` is never signed in to. |
| Know who commits | `prefs::identity`: `settings.identity.current()` is the profile in use; `identity::generation()` changes whenever the identity git commands get may have changed (ask git again then, as `ui::prefs::profiles::commit_author` does). |
| Show the text of a file (not git's own data) | Decode it in the background with `git::text::format_for(workdir)` (the repository's encoding and the tab width: `display_text_in`, `decode_file`), keep the `Format` with the result, and load again when `text::generation()` has moved on (`ui/diff.rs` does). |
| Show a diff of one file | `tab.open_diff(target)` (or `tab.diff = Some(DiffView { target, data: None })` plus `handle.diff(target)`). The reader works it out in the mode set in Preferences > Editor (`git::diff::mode()`: whole file, ignore whitespace) and the answer says which (`FileDiff::mode`); the view asks again when they differ. From a job, `loader.diff_with(&target, git::diff::mode())`, and load again when `diff.mode` is no longer `git::diff::mode()` (`ui/history.rs` does); `loader.diff(&target)` is the plain diff of three lines of context, the one to build patches from. A whole-file diff has `Hunk::gap` hunks between the real ones (unchanged lines, no header, nothing to stage); a diff with whitespace ignored must not be applied (`ui::diff::can_apply_parts`). `ui::diff::read_only` paints one in the layout chosen (`ui::diff_split` for side by side). |
| Compare a commit with the working tree, or show a file as it is in a commit | `DiffSource::WorkingTree { from }`: read-only like `Compare`, loaded again with every snapshot; its file list is `loader.working_tree_files(from)`. `DiffSource::At(commit)`: the whole file as one gap hunk (`loader.file_at`), painted by `ui::file_view` with one column of numbers. `is_committed()` is true for both, so nothing in them is staged; `reads_working_tree()` says the new side is read from disk. `loader.forked_files(other, tip)` gives the merge base and the files `tip` changed since (`other...tip`). |
| Show a comparison in the right panel | `ui::multi::compare(tab, shared, a, b)` (tree to tree), `compare_forked(tab, shared, tip, other)` (what `tip` adds since their merge base), `compare_with_working_tree(tab, shared, oid)`. Each loads more history first when a commit is not in the graph yet. |
| Open file history or blame at a commit | `HistoryView::history(path).starting_at(Some(oid))` (or `::blame`), then `tab.overlay = Some(Overlay::History(view))`. The title bar names the commit and offers "Go to HEAD". |
| Offer a file as it was in a commit | `ui::versions::of_file(tab, source, file)` says which version a file list of `source` stands for (the deleted version for a deletion, none for the working tree's lists); `ui::versions::menu_items(ui, shared, &version, which)` draws "Open / Save … as… / Restore …" and returns an `Act` for `ui::versions::apply` once the menu is drawn. The backend is `git::versions`: `read` (through the checkout filters), `write_temp`, `save`, and `restore`, an undoable `GitCmd` (`UndoKind::Files`). |
| List every file of a commit | `git::versions::tree(repo, oid)` on the reader thread: entries in file-browser order (folders first, each knowing where its contents end) and the files by path. `ui::browse` shows it in the commit details. |
| Paint code | `prefs::editor::look()` (one atomic load): `font()`, `small()`, `line_height()`, `numbers`, `wrap`, `highlight`. `theme::code(size)` is the code font; `ui::diff::{first_rows, wrap_rows, segments}` wrap lines with rows kept from frame to frame. |
| Do network or other non-repository background work | Spawn your own thread, send results over a channel, call `ctx.request_repaint()` when something arrives, and poll the channel from your UI code. Never block in UI code. |

Fetch commands start from `ops::fetch(&[...])`, which adds `--prune`
unless Auto-Prune (Preferences > General) is off. After any command whose
steps run checkout, switch, pull, merge, rebase, reset to a commit,
cherry-pick or revert (`prefs::general::moves_head`), the operator thread
runs `git submodule update --init --recursive` when "Keep submodules up to
date" is on and the repository has a `.gitmodules` file.

`tab.jump_to(oid)` selects a commit and scrolls the graph to it.
`tab.ref_hover.hover(ui.ctx(), tip)`, called on each frame the pointer is on
something that stands for a branch (sidebar rows and graph labels do), makes
the graph dim the loaded commits that `tip` does not reach.
`tab.hidden` says which refs the graph leaves out (`src/git/hidden.rs`;
the snapshot still lists them) and `tab.set_hidden(settings, path, hide)`
changes that for a ref or a folder. The checked out branch is always shown
unless `hidden.hides_head()` (only while one branch is shown alone,
`ui::solo`).
`tab.is_running("Label prefix")` tells whether an operation is in flight, and
`tab.take_outcome(label, since)` how one you started ended (its failure is
already shown to the user as a toast).

## Preferences

File > Preferences (Ctrl+, or the gear at the right end of the tab strip)
replaces the repository view with a page of sections (`src/ui/prefs/mod.rs`,
`Section`). Each section is one file, `src/ui/prefs/<section>.rs`, with a
`State` (kept while the page is open) and `show(ui, page, state)`. `Page`
gives the egui context, `shared` (settings, toasts, dialogs), the tab in
front, and the global and repository git configuration files.

- Build pages from `ui::prefs::form`: `row(ui, label, |ui| ...)` puts the label
  in the left column and the controls on the right, then `help`, `warning`
  or `error` under the control; `checkbox`, `text`, `number`, `choice`,
  `path` (with Browse), `button` (tinted: accent for Browse, green for
  Generate, red for Disconnect), `heading` for groups, `intro` for a
  paragraph under the title. `config_bool` and `config_text` bind a control
  straight to a git setting.
- Settings GitKrill owns live in `src/prefs/<area>.rs`; settings git owns
  stay in git's files (`core.autocrlf`, `commit.gpgsign`, `gitflow.*`) so
  every git tool sees them.
- Anything that must be put into effect outside `Settings` (environment of
  git commands, fonts, theme files) goes in the area's `apply(ctx,
  settings)`, called at start and after each change of any setting: compare
  with what was applied last and return early when nothing of yours changed.
- Nothing may cost anything while the page is closed, or slow a frame
  while it is open: run git, gpg, ssh, font scans and network requests on a
  background thread (`ConfigFile` does this for configuration).
- Tests and scripted runs must never write the user's own git
  configuration. `Scope::Global` is `<GITKRILL_CONFIG_DIR>/gitconfig`
  whenever `GITKRILL_CONFIG_DIR` is set without `GIT_CONFIG_GLOBAL`, and
  `tools/shot.sh` points `GIT_CONFIG_GLOBAL` at `cfg/gitconfig`; in `cargo
  test` use `Scope::File` with a temporary file.
- The command palette lists "Preferences: <section>" for every section.
- Colours of a custom theme (`crate::prefs::appearance`) are read by name
  from the `Palette` fields: a colour added to `Palette` must also go in
  `colours()` there (a test fails otherwise), and gets the base set's value
  in theme files that do not name it.
- Dates go through `state::format_time` (with the time of day) or
  `prefs::appearance::custom_date`, so they follow Preferences > UI
  Customization > Date/Time Short Format; do not call chrono's `format`
  for a date shown to the user.

## Conventions

- Colours, fonts and icons come from `src/theme.rs` (`theme::icons::NAME` are
  Phosphor icons; `theme::font`, `theme::bold`, `theme::mono`). Colours are
  functions (`theme::text_dim()`, `theme::lane_color(i)`) that read the
  palette in use, so the theme can change at runtime: call them every frame
  and do not keep their result in a cache or a `const`. Do not hard-code
  colours in feature code. Every colour has to work in both sets, `DARK`
  and `LIGHT`: text on a solid coloured fill is `theme::on_accent()` over
  `theme::solid(color, amount)` (or `theme::deep(color)`), translucent
  shades are `theme::shadow()` / `theme::well()`, and floating cards use
  `theme::bg_float()`. `theme::contrast` measures what a test should check.
- Lists that can be long are virtualised: allocate the full content size, then
  paint only the rows inside the viewport (`ui/graph.rs`, `ui/diff.rs`).
- Custom-painted rows use `ui.interact(rect, id, Sense::click())` for input and
  a `Painter` for drawing; `widgets.rs` has the shared pieces (`row_background`,
  `elided`, `tint_button`, `icon_button`, `spinner`, `menu_item`, ...).
- Custom-painted text gets editor-style selection and copying from
  `ui/textsel.rs` (diff and blame use it): describe the text as `Lines`, call
  `Selection::interact` with a function from screen point to text position,
  and `Selection::paint` behind each row's text.
- Comments say what the code does or why, for a reader who was not there when
  it was written.
- Strings from git may not be UTF-8: use the `*_bytes()` accessors with
  `git::repo::lossy`.

## Library notes

- **egui 0.36** differs from most examples online. Panels are
  `egui::Panel::top/bottom/left/right(id).show(ui, ...)` and take a `Ui`;
  `eframe::App` implements `fn ui(&mut self, ui, frame)`; `Frame::new()`,
  `CornerRadius`, `Margin` (i8 fields); `painter.rect(.., StrokeKind::Inside)`;
  menus via `response.context_menu`, `egui::Popup::menu(&response).show(..)`
  and `ui.menu_button` (they close on click by default); modals via
  `egui::Modal`. When unsure, read the source:
  `~/.cargo/registry/src/index.crates.io-*/egui-0.36.2/src`.
- `Response::has_focus()` is false whenever the window itself has no
  keyboard focus, which is always the case in `tools/shot.sh` runs (the
  window is off-screen and takes none). Logic that must survive that (keep
  what is being typed) asks `ui.memory(|m| m.has_focus(response.id))`.
- **git2 0.21**: most string getters return `Result<&str, Error>`; prefer the
  `*_bytes()` variants. `Repository::stash_foreach` needs `&mut`.
- **libgit2 revwalk**: any sort mode makes it walk all of history first. See
  `Loader::walk` for how the history page stays O(page size).
- `GIT_LITERAL_PATHSPECS` breaks `git stash push --include-untracked`; it is
  therefore set per step (`Step::with_paths`), not globally. `git::stash`
  makes stash paths literal with `:(literal)` instead.
- **`git stash push` traps** (git 2.51): with `--staged` and paths it puts
  the working files of every other staged file back to HEAD too, and
  `--pathspec-from-file` is refused; `--staged` saves the stash before
  reverse-applying the staged changes to the working tree, so unstaged
  edits next to staged lines leave the stash saved and nothing removed
  (`git::stash::staged_check` dry-runs that); a path only HEAD has (a
  staged deletion, a rename's old name) is refused, or with
  `--include-untracked` the stash is saved and then the command fails.
- **git-lfs sets itself up** in a repository on most commands, even ones
  that only list (`git lfs track`, `git lfs ls-files`): it installs its
  hooks and writes `lfs.repositoryformatversion` to `.git/config`. Ask
  `git::lfs::status` whether a repository uses LFS (it looks without
  git-lfs) before running anything else of it.

## Checking your work

```sh
cargo build
cargo test          # backend tests run against real temporary repositories
cargo clippy --all-targets
```

Backend tests can use `crate::tests::Fixture` (a temp repository plus the real
worker threads): see `src/tests.rs`.

**Look at the UI.** `tools/shot.sh` drives the real app with scripted clicks
and writes PNGs; `tools/demo-repo.sh <dir>` builds a repository with branches,
merges, a stash, a remote and a dirty tree to point it at.

```sh
tools/demo-repo.sh scratch/demo
tools/shot.sh scratch/demo scratch/shots/final.png "rclick:560,234;shot:menu.png;key:Escape"
```

- The script language is documented at the top of `src/shot.rs`.
- Coordinates are egui points. Screenshots are in pixels; divide by the
  display scale (1.25 on the main development machine).
- Find coordinates by taking a screenshot first, then script the clicks.
- The window is off-screen and takes no focus; runs do not disturb the user.
- Scripts wait for git operations and reader-thread jobs to finish before each
  step. Work on other threads (blame, avatars, hosting) is not waited for: add
  a `sleep:`.
- `press:x,y` / `move:x,y` / `release:x,y` script a drag.
- `hold:shift` holds a modifier for clicks, but a `key:` step sends its own
  modifiers: write `key:shift+ArrowDown`, not `hold:shift;key:ArrowDown`.
- `GITKRILL_HOSTING_FIXTURE=<dir>` makes the hosting sidebar answer from
  `<dir>/<endpoint>.json` instead of the network: `pulls`, `issues` and
  `runs` for GitHub, `merge_requests`, `issues` and `pipelines` for GitLab,
  in each API's own JSON. `<endpoint>.head` stages a status and headers
  (a rate limit, an outage). To show a GitLab or self-hosted remote, point
  the demo's origin at it (`git remote set-url origin
  https://gitlab.com/group/app.git`) and put `"auto_fetch_minutes": 0` in
  the run's `cfg/settings.json` first, so the app never fetches from the
  real host; `"hosting_servers": {"git.example.com": "gitlab"}` there
  connects a made-up server without clicking through the offer.
- Opening a pull request is answered from `create.json` (and `create.head`
  for another status), the request body is written to `create.sent.json`,
  and `pulls.after.json` replaces `pulls.json` once one was opened. An empty
  file named `signed-in` stands in for a token; without it the fixture is
  signed out. Browser links clicked during a `GITKRILL_SHOT` run are
  printed, not opened.
- External tools (merge, diff, editor, terminal) are never started by a
  `GITKRILL_SHOT` run either: "would run: ..." on standard error shows the
  exact command line. Preferences > External Tools still lists the tools
  really installed, so set the run's choices in `cfg/settings.json`
  (`"tools": {"merge": {"program": {"id": "vscode", ...}}}`) or click them.
- Preferences > Integrations reads the account from `user.json` (`GET
  /user`, in the service's own JSON: `login` for GitHub, `username` for
  GitLab) and its keys from `keys.json` (`keys.head` stages a refusal such
  as a missing scope: `404` with `x-accepted-oauth-scopes`, or GitLab's
  `403`). Adding a key is answered from `keys.post.json` (201 unless
  `keys.post.head` says otherwise), the body sent is written to
  `keys.sent.json`, and `keys.after.json` replaces `keys.json` once a key
  was added. Public keys are looked for, and generated by the real
  `ssh-keygen`, in `<fixture>/ssh`; without a fixture, runs with
  `GITKRILL_CONFIG_DIR` set use `<config dir>/ssh`, never `~/.ssh`.
- For a demo repository whose remote must look like GitHub, set the fetch URL
  to github.com, a separate push URL to a local bare repository
  (`git remote set-url --push origin <bare>`), and `"auto_fetch_minutes": 0`
  and `"fetch_avatars": false` in the run's `cfg/settings.json`.
- Screenshot runs keep their avatar cache under the config folder too, so
  they never touch the real one.
- To see the light theme, put `{"theme": "Light"}` in `cfg/settings.json`
  before the run (`"auto_fetch_minutes": 0` keeps the 8-second fetch from
  changing the demo mid-script).
- Updates (`src/update.rs`) are off in builds made from source. To try them,
  serve a folder on the loopback address (`python -m http.server 8765 --bind
  127.0.0.1`) holding `latest.json` in the shape of GitHub's
  `releases/latest` answer and the files under `download/<tag>/`, then run with
  `GITKRILL_UPDATE_FEED=http://127.0.0.1:8765`, and `GITKRILL_UPDATE_KIND=
  installed|portable|browser` to pick the flow. Screenshot runs only check
  against such a feed, and print instead of restarting. A portable run
  really replaces the executable it runs as: point `BIN` at a copy.
- `GITKRILL_SHOT_NO_LFS=1` makes a scripted run behave as if git-lfs were
  not installed (Preferences > LFS).
- To try commit signing, give the run a throwaway GnuPG home: `export
  MSYS2_ENV_CONV_EXCL=GNUPGHOME GNUPGHOME=/c/Users/<you>/AppData/Local/Temp/<area>/gnupg`
  (Git for Windows' gpg only reads the `/c/...` form, and Git Bash would
  rewrite it to `C:/...` for the app without the first variable). Start
  its agent first, detached (`gpgconf --launch gpg-agent </dev/null
  >/dev/null 2>&1`): one started by the app inherits the run's output pipe
  and keeps a `| tail` waiting. Stop it afterwards with `gpgconf --kill
  gpg-agent`. SSH keys made from Preferences > SSH go to
  `<GITKRILL_CONFIG_DIR>/ssh` during runs, never `~/.ssh`.
- Folder dialogs cannot be clicked through by a script: with
  `GITKRILL_SHOT_FOLDER=<dir>` set, "Add a folder of repositories" picks
  `<dir>` instead of asking. Workspaces can also be put straight into the
  run's `cfg/settings.json` (`"workspaces": [{"root": "<dir>", "repos": []}]`);
  they are scanned on start.
- Git being missing: `GITKRILL_GIT=<path>` makes GitKrill use exactly that
  git (point it at a file that does not exist), and `GITKRILL_GIT_FEED=
  http://127.0.0.1:<port>` stands in for Git for Windows' releases like
  `GITKRILL_UPDATE_FEED` does for GitKrill's. With `LOCALAPPDATA` pointed at
  a scratch folder too, "Install Git" runs whatever installer the feed
  serves with `/DIR=<LOCALAPPDATA>\Programs\Git`; a stand-in that writes a
  `git.exe` answering `--version` there completes the flow. Never let a
  test run the real Git installer on a development machine. CI runs the
  real one, on a runner whose own Git is hidden (`/INSTALLGIT=1`).
- Unknown command-line flags start the GUI. Set `GITKRILL_CONFIG_DIR` even
  for one-off runs of the binary.
- `tools/shot.sh` keeps settings in a `cfg` folder next to the output. Never
  run the app for testing without `GITKRILL_CONFIG_DIR` set, or the user's real
  tab list gets temp repositories in it.
- On Windows the shell is Git Bash. Long here-documents containing
  apostrophes can fail; write files with an editor tool instead.
