//! Development aid: drive the real UI with scripted input, write PNG
//! screenshots, and exit. Nothing here runs unless `GITKRILL_SHOT` is set.
//!
//! ```text
//! GITKRILL_SHOT=out.png        final screenshot; the app exits after writing it
//! GITKRILL_SHOT_SIZE=1500x860  window size in points
//! GITKRILL_SHOT_FOLDER=dir     what folder dialogs answer instead of asking
//! GITKRILL_SHOT_ROW=3          select this graph row once the repository loads
//! GITKRILL_SHOT_SCRIPT=...     semicolon-separated steps, run after loading:
//!     click:x,y  rclick:x,y  dclick:x,y  move:x,y     (points, not pixels)
//!     press:x,y  release:x,y  left button down / up, for drags:
//!                             press:60,200;move:80,220;move:300,400;release:300,400
//!     type:some text         key:Enter  key:ctrl+F
//!     hold:shift  hold:none  keep modifiers down for the steps in between:
//!                             hold:shift;click:300,400;hold:none
//!     scroll:-40             wheel lines at the pointer (negative = down)
//!     wait:30                pause, in 16 ms units (so 30 is about half a second)
//!     sleep:800              pause, in milliseconds
//!     shot:name.png          intermediate screenshot, saved next to the final one
//! ```
//!
//! After every step the script waits for background git work to finish and
//! for the UI to settle, so steps can be written back to back. Prefix a step
//! with `!` to run it without waiting, which is how to capture something
//! mid-flight: `click:723,84;!sleep:600;!shot:pushing.png`.
//!
//! The window is parked off-screen and takes no focus, so runs do not disturb
//! whoever is using the machine. Set `GITKRILL_SHOT_VISIBLE=1` to watch.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, Modifiers, PointerButton, Pos2, ViewportCommand};

use crate::state::RepoTab;

/// How long the UI gets to settle after each step. Time, not frames: an
/// unfocused window is not throttled to the display's refresh rate, and
/// popups fade in over real time.
const SETTLE: Duration = Duration::from_millis(220);

enum Step {
    Move(Pos2),
    Click(Pos2, PointerButton, u8),
    /// Primary button pressed (`true`) or released at a position.
    Button(Pos2, bool),
    Type(String),
    Key(Modifiers, Key),
    Hold(Modifiers),
    Sleep(Duration),
    /// Wheel notches at the current pointer position; negative scrolls down.
    Scroll(f32),
    Shot(PathBuf),
}

pub struct Shot {
    path: PathBuf,
    row: Option<usize>,
    /// Each step with its "do not wait for idle first" flag.
    steps: VecDeque<(bool, Step)>,
    /// Raw events to feed egui, one batch per frame.
    pending: VecDeque<Vec<Event>>,
    loaded: bool,
    /// When the UI last became idle.
    settled_since: Option<Instant>,
    sleep_until: Option<Instant>,
    /// A screenshot was requested and has not come back yet.
    awaiting: Option<PathBuf>,
    finishing: bool,
}

fn parse_pos(text: &str) -> Option<Pos2> {
    let (x, y) = text.split_once(',')?;
    Some(Pos2::new(x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn parse_key(text: &str) -> Option<(Modifiers, Key)> {
    let mut modifiers = Modifiers::NONE;
    let mut key = None;
    for part in text.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" | "cmd" => modifiers = modifiers | Modifiers::COMMAND | Modifiers::CTRL,
            "shift" => modifiers |= Modifiers::SHIFT,
            "alt" => modifiers |= Modifiers::ALT,
            "up" => key = Some(Key::ArrowUp),
            "down" => key = Some(Key::ArrowDown),
            other => key = Key::from_name(other).or_else(|| Key::from_name(part.trim())),
        }
    }
    Some((modifiers, key?))
}

fn parse_script(script: &str) -> VecDeque<(bool, Step)> {
    script
        .split(';')
        .filter(|raw| !raw.trim().is_empty())
        .filter_map(|raw| {
            let raw = raw.trim();
            let (eager, raw) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw),
            };
            let (verb, arg) = raw.split_once(':').unwrap_or((raw, ""));
            let step = match verb {
                "move" => Step::Move(parse_pos(arg)?),
                "click" => Step::Click(parse_pos(arg)?, PointerButton::Primary, 1),
                "dclick" => Step::Click(parse_pos(arg)?, PointerButton::Primary, 2),
                "rclick" => Step::Click(parse_pos(arg)?, PointerButton::Secondary, 1),
                "press" => Step::Button(parse_pos(arg)?, true),
                "release" => Step::Button(parse_pos(arg)?, false),
                "type" => Step::Type(arg.to_owned()),
                "hold" => Step::Hold(match arg.trim() {
                    "none" | "" => Modifiers::NONE,
                    // A key name is needed to parse; only its modifiers count.
                    held => parse_key(&format!("{held}+A"))?.0,
                }),
                "key" => {
                    let (modifiers, key) = parse_key(arg)?;
                    Step::Key(modifiers, key)
                }
                "wait" => Step::Sleep(Duration::from_millis(arg.parse::<u64>().ok()? * 16)),
                "sleep" => Step::Sleep(Duration::from_millis(arg.parse().ok()?)),
                "scroll" => Step::Scroll(arg.parse().ok()?),
                // Relative to the final screenshot's folder.
                "shot" => Step::Shot(arg.into()),
                _ => {
                    eprintln!("unknown script step: {raw}");
                    return None;
                }
            };
            Some((eager, step))
        })
        .collect()
}

impl Shot {
    /// The folder a scripted run picks in place of showing a folder dialog
    /// (`GITKRILL_SHOT_FOLDER`), which a script could not click through.
    pub fn picked_folder() -> Option<PathBuf> {
        std::env::var_os("GITKRILL_SHOT")?;
        std::env::var_os("GITKRILL_SHOT_FOLDER").map(PathBuf::from)
    }

    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("GITKRILL_SHOT")?;
        Some(Shot {
            path: path.into(),
            row: std::env::var("GITKRILL_SHOT_ROW").ok().and_then(|v| v.parse().ok()),
            steps: parse_script(&std::env::var("GITKRILL_SHOT_SCRIPT").unwrap_or_default()),
            pending: VecDeque::new(),
            loaded: false,
            settled_since: None,
            sleep_until: None,
            awaiting: None,
            finishing: false,
        })
    }

    /// Called before egui processes a frame's input.
    pub fn inject(&mut self, raw: &mut egui::RawInput) {
        // Scripted runs must not depend on where the real mouse happens to be.
        raw.events.retain(|event| matches!(event, Event::Screenshot { .. }));
        if let Some(events) = self.pending.pop_front() {
            raw.events.extend(events);
        }
    }

    fn queue(&mut self, step: Step) {
        let none = Modifiers::NONE;
        match step {
            Step::Move(pos) => self.pending.push_back(vec![Event::PointerMoved(pos)]),
            Step::Click(pos, button, times) => {
                self.pending.push_back(vec![Event::PointerMoved(pos)]);
                for _ in 0..times {
                    self.pending.push_back(vec![Event::PointerButton {
                        pos,
                        button,
                        pressed: true,
                        modifiers: none,
                    }]);
                    self.pending.push_back(vec![Event::PointerButton {
                        pos,
                        button,
                        pressed: false,
                        modifiers: none,
                    }]);
                }
            }
            Step::Button(pos, pressed) => {
                self.pending.push_back(vec![Event::PointerMoved(pos)]);
                self.pending.push_back(vec![Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: none,
                }]);
            }
            Step::Type(text) => self.pending.push_back(vec![Event::Text(text)]),
            Step::Hold(modifiers) => self.pending.push_back(vec![Event::ModifiersChanged(modifiers)]),
            Step::Key(modifiers, key) => {
                let event = |pressed| Event::Key {
                    key,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers,
                };
                self.pending.push_back(vec![Event::ModifiersChanged(modifiers), event(true)]);
                self.pending.push_back(vec![event(false), Event::ModifiersChanged(none)]);
            }
            Step::Scroll(lines) => self.pending.push_back(vec![Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, lines),
                phase: egui::TouchPhase::Move,
                modifiers: none,
            }]),
            Step::Sleep(duration) => self.sleep_until = Some(Instant::now() + duration),
            Step::Shot(_) => {}
        }
    }

    /// Called after each frame. Returns true once the final screenshot is
    /// written and the window has been asked to close.
    pub fn drive(&mut self, ctx: &egui::Context, mut tab: Option<&mut RepoTab>) -> bool {
        let image = ctx.input(|i| {
            i.events.iter().find_map(|event| match event {
                Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            if let Some(path) = self.awaiting.take() {
                let [w, h] = image.size;
                let saved = image::save_buffer(
                    &path,
                    image.as_raw(),
                    w as u32,
                    h as u32,
                    image::ColorType::Rgba8,
                );
                if let Err(e) = saved {
                    eprintln!("screenshot {} failed: {e}", path.display());
                }
            }
            if self.finishing {
                ctx.send_viewport_cmd(ViewportCommand::Close);
                return true;
            }
        }
        ctx.request_repaint();

        if !self.loaded {
            let ready = tab.as_ref().is_none_or(|t| t.snap.is_some() || t.error.is_some());
            if !ready {
                return false;
            }
            if let (Some(row), Some(tab)) = (self.row, tab.as_deref_mut()) {
                tab.select_row(row);
                tab.scroll_to = Some(row);
            }
            self.loaded = true;
        }

        // One thing at a time: injected input, then background work, then a
        // moment for the result to be laid out, faded in and painted.
        if !self.pending.is_empty() || self.awaiting.is_some() {
            self.settled_since = None;
            return false;
        }
        if let Some(until) = self.sleep_until {
            if Instant::now() < until {
                return false;
            }
            self.sleep_until = None;
        }
        let eager = self.steps.front().is_some_and(|(eager, _)| *eager);
        if !eager {
            let busy = tab.as_ref().is_some_and(|t| {
                t.busy.iter().any(|op| op.label != "Auto-fetch")
                    || t.has_pending_jobs()
                    || t.diff.as_ref().is_some_and(|d| d.data.is_none())
                    || (matches!(t.selection, crate::state::Selection::Commit(_)) && t.details.is_none())
            });
            if busy {
                self.settled_since = None;
                return false;
            }
            let since = *self.settled_since.get_or_insert_with(Instant::now);
            if since.elapsed() < SETTLE {
                return false;
            }
        }
        self.settled_since = None;

        match self.steps.pop_front().map(|(_, step)| step) {
            Some(Step::Shot(name)) => {
                self.awaiting = Some(self.path.with_file_name(name));
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
            }
            Some(step) => self.queue(step),
            None => {
                self.awaiting = Some(self.path.clone());
                self.finishing = true;
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
            }
        }
        false
    }
}
