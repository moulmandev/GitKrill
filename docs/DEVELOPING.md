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
| Change several branches and tags in one undoable operation | `.undoable(UndoKind::Refs(vec![RefMove::new("refs/heads/develop"), ...]))`: the operator notes where each ref pointed before and after; undo and redo move them all back (or forth), HEAD included, and refuse if one has moved since. `git::gitflow::finish` uses it. |
| Say what is left when an operation stops on conflicts | `.after_conflict("Finish … again once the merge is committed.")` on the `GitCmd`: added to the "stopped on conflicts" toast. |
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
| Give every git command an environment variable | `git::cli::set_session_env` (the SSH key and author of the profile in use). |
| Change what the commit button does | `src/ui/commit_button.rs`: the split button (`show`), what runs once a commit succeeded (`follow_up`), the commit template in the message fields (`prefill`). |
| Ask a hosting account something (who is signed in, its SSH keys) | `hosting::account::AccountSession::start(account, ctx)`, polled from the UI; `account::local_keys`, `parse_public_key` and `fingerprint` read `.pub` files. A host in `settings.hosting_disconnected` is never signed in to. |
| Do network or other non-repository background work | Spawn your own thread, send results over a channel, call `ctx.request_repaint()` when something arrives, and poll the channel from your UI code. Never block in UI code. |

Fetch commands start from `ops::fetch(&[...])`, which adds `--prune`
unless Auto-Prune (Preferences > General) is off. After any command whose
steps run checkout, switch, pull, merge, rebase, reset to a commit,
cherry-pick or revert (`prefs::general::moves_head`), the operator thread
runs `git submodule update --init --recursive` when "Keep submodules up to
date" is on and the repository has a `.gitmodules` file.

`tab.jump_to(oid)` selects a commit and scrolls the graph to it.
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
  therefore set per step (`Step::with_paths`), not globally.
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
- Folder dialogs cannot be clicked through by a script: with
  `GITKRILL_SHOT_FOLDER=<dir>` set, "Add a folder of repositories" picks
  `<dir>` instead of asking. Workspaces can also be put straight into the
  run's `cfg/settings.json` (`"workspaces": [{"root": "<dir>", "repos": []}]`);
  they are scanned on start.
- `tools/shot.sh` keeps settings in a `cfg` folder next to the output. Never
  run the app for testing without `GITKRILL_CONFIG_DIR` set, or the user's real
  tab list gets temp repositories in it.
- On Windows the shell is Git Bash. Long here-documents containing
  apostrophes can fail; write files with an editor tool instead.
