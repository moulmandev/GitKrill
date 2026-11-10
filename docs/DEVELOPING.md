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
| Show a full-size view instead of the graph | Add state to your variant of `Overlay` (`src/state.rs`), set `tab.overlay = Some(...)`. `ui::overlay` calls your `show(ui, tab, shared, view) -> bool` (false closes). Start with `widgets::overlay_bar` for the title bar. Escape closes overlays. |
| Ask the user something | `shared.show(Dialog::...)` (`src/ui/dialogs.rs`); `Dialog::confirm(title, body, button, cmd)` for destructive confirmations. |
| Tell the user something | `shared.toast(ToastKind::Success \| Error, title, body)`. |
| Offer an action in the command palette (`Ctrl+P`) | Push an `Entry` in `entries` (`src/ui/palette.rs`, see "More actions") whose closure calls the function the button or menu already calls. Leave the entry out while it cannot run. |
| Persist a preference | Add a field to `Settings` (`src/state.rs`) and to its `Default`; it is saved automatically. |
| Do network or other non-repository background work | Spawn your own thread, send results over a channel, call `ctx.request_repaint()` when something arrives, and poll the channel from your UI code. Never block in UI code. |

`tab.jump_to(oid)` selects a commit and scrolls the graph to it.
`tab.is_running("Label prefix")` tells whether an operation is in flight, and
`tab.take_outcome(label, since)` how one you started ended (its failure is
already shown to the user as a toast).

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
- **git2 0.21**: most string getters return `Result<&str, Error>`; prefer the
  `*_bytes()` variants. `Repository::stash_foreach` needs `&mut`.
- **libgit2 revwalk**: any sort mode makes it walk all of history first. See
  `Loader::walk` for how the history page stays O(page size).
- `GIT_LITERAL_PATHSPECS` breaks `git stash push --include-untracked`; it is
  therefore set per step (`Step::with_paths`), not globally.

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
- For a demo repository whose remote must look like GitHub, set the fetch URL
  to github.com, a separate push URL to a local bare repository
  (`git remote set-url --push origin <bare>`), and `"auto_fetch_minutes": 0`
  and `"fetch_avatars": false` in the run's `cfg/settings.json`.
- Screenshot runs keep their avatar cache under the config folder too, so
  they never touch the real one.
- To see the light theme, put `{"theme": "Light"}` in `cfg/settings.json`
  before the run (`"auto_fetch_minutes": 0` keeps the 8-second fetch from
  changing the demo mid-script).
- `tools/shot.sh` keeps settings in a `cfg` folder next to the output. Never
  run the app for testing without `GITKRILL_CONFIG_DIR` set, or the user's real
  tab list gets temp repositories in it.
- On Windows the shell is Git Bash. Long here-documents containing
  apostrophes can fail; write files with an editor tool instead.
