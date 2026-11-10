//! "Highlight associated rows when hovering over a branch": while the pointer
//! is on a branch (a label in the graph, or a row in the left panel), the
//! graph dims every loaded commit that branch does not contain.
//!
//! Views that show branches call [`RefHover::hover`] with the tip of the one
//! under the pointer; the graph calls [`RefHover::reached`] once per frame
//! before painting a row, with the tip of its own label under the pointer if
//! there is one. The left panel is drawn before the graph, so every hover
//! shows in the frame it happens, and nothing asks for extra frames. A hover
//! counts for the frame it was seen in only: one from a view that is no
//! longer drawn is ignored.
//!
//! The rows a tip reaches are worked out once per tip and history, in one
//! pass over the loaded rows from the tip down (rows are ordered children
//! first, so every parent comes after its child), and kept as a bit set.
//! Frames in between cost a hash lookup and an `Arc` clone.

use std::sync::{Arc, Weak};

use eframe::egui;

use crate::git::{History, Oid};

/// The branch the pointer is on, and the rows it reaches. Kept by the tab
/// (`RepoTab::ref_hover`).
#[derive(Default)]
pub struct RefHover {
    /// The tip hovered while drawing, and in which frame.
    seen: Option<(Oid, u64)>,
    /// What `rows` was computed for. The weak reference keeps the history's
    /// allocation from being reused while it is held, so comparing addresses
    /// cannot mistake a new history for the old one.
    key: Option<(Oid, Weak<History>)>,
    rows: Arc<Rows>,
}

impl RefHover {
    /// The pointer is on a branch whose tip is `tip`, this frame.
    pub fn hover(&mut self, ctx: &egui::Context, tip: Oid) {
        self.seen = Some((tip, ctx.cumulative_frame_nr()));
    }

    /// The rows reached by the branch under the pointer: `label`, the tip of
    /// a graph label the pointer is on, or else what [`RefHover::hover`] was
    /// told this frame. `None` when nothing is highlighted: no branch
    /// hovered, `enabled` false, or the tip not among the loaded rows.
    pub fn reached(&mut self, ctx: &egui::Context, history: &Arc<History>, label: Option<Oid>, enabled: bool) -> Option<Arc<Rows>> {
        let frame = ctx.cumulative_frame_nr();
        let seen = self.seen.take().filter(|(_, at)| *at == frame).map(|(tip, _)| tip);
        let tip = label.or(seen).filter(|_| enabled)?;
        let row = *history.index_of.get(&tip)? as usize;

        let fresh = self.key.as_ref().is_some_and(|(oid, weak)| *oid == tip && Weak::as_ptr(weak) == Arc::as_ptr(history));
        if !fresh {
            // Reuses the buffer: the clone handed out last frame is gone.
            Arc::make_mut(&mut self.rows).fill(history, row);
            self.key = Some((tip, Arc::downgrade(history)));
        }
        Some(self.rows.clone())
    }
}

/// A set of row numbers.
#[derive(Clone, Default, Debug)]
pub struct Rows {
    bits: Vec<u64>,
}

impl Rows {
    pub fn contains(&self, row: usize) -> bool {
        self.bits.get(row / 64).is_some_and(|word| word & (1 << (row % 64)) != 0)
    }

    fn insert(&mut self, row: usize) {
        self.bits[row / 64] |= 1 << (row % 64);
    }

    /// The rows reachable from row `tip` through parent links, itself
    /// included.
    fn fill(&mut self, history: &History, tip: usize) {
        let commits = &history.commits;
        self.bits.clear();
        self.bits.resize(commits.len().div_ceil(64), 0);
        self.insert(tip);
        for (row, commit) in commits.iter().enumerate().skip(tip) {
            if !self.contains(row) {
                continue;
            }
            for parent in &commit.parents {
                if let Some(&parent_row) = history.index_of.get(parent) {
                    self.insert(parent_row as usize);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::graph::Graph;
    use crate::git::{CommitInfo, RowKind};

    fn oid(n: u8) -> Oid {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        Oid::from_bytes(&bytes).unwrap()
    }

    fn history(rows: &[(u8, &[u8])]) -> Arc<History> {
        let commits: Vec<CommitInfo> = rows
            .iter()
            .map(|(id, parents)| CommitInfo {
                oid: oid(*id),
                parents: parents.iter().map(|p| oid(*p)).collect(),
                summary: String::new(),
                body_preview: String::new(),
                author: String::new(),
                email: String::new(),
                time: 0,
                kind: RowKind::Commit,
            })
            .collect();
        let graph = Graph::build(&commits);
        let index_of = commits.iter().enumerate().map(|(i, c)| (c.oid, i as u32)).collect();
        Arc::new(History { commits, graph, index_of, has_more: false })
    }

    fn reached(rows: &Rows, count: usize) -> Vec<usize> {
        (0..count).filter(|row| rows.contains(*row)).collect()
    }

    #[test]
    fn a_branch_reaches_its_own_line_and_what_it_merged() {
        // 6 (main) merges 4 (feature); 5 is another branch off 2.
        //   6 -> 3, 4;  5 -> 2;  4 -> 2;  3 -> 2;  2 -> 1
        let history = history(&[(6, &[3, 4]), (5, &[2]), (4, &[2]), (3, &[2]), (2, &[1]), (1, &[])]);
        let ctx = egui::Context::default();
        let mut hover = RefHover::default();

        hover.hover(&ctx, oid(6));
        let rows = hover.reached(&ctx, &history, None, true).unwrap();
        assert_eq!(reached(&rows, 6), vec![0, 2, 3, 4, 5]);
        drop(rows);

        hover.hover(&ctx, oid(5));
        let rows = hover.reached(&ctx, &history, None, true).unwrap();
        assert_eq!(reached(&rows, 6), vec![1, 4, 5]);
    }

    #[test]
    fn nothing_is_highlighted_without_a_hover_or_when_turned_off() {
        let history = history(&[(2, &[1]), (1, &[])]);
        let ctx = egui::Context::default();
        let mut hover = RefHover::default();
        assert!(hover.reached(&ctx, &history, None, true).is_none());

        hover.hover(&ctx, oid(2));
        assert!(hover.reached(&ctx, &history, None, false).is_none());
        // Each hover counts for one latch: the pointer left.
        hover.hover(&ctx, oid(2));
        assert!(hover.reached(&ctx, &history, None, true).is_some());
        assert!(hover.reached(&ctx, &history, None, true).is_none());

        // A tip that is not loaded highlights nothing.
        hover.hover(&ctx, oid(9));
        assert!(hover.reached(&ctx, &history, None, true).is_none());

        // The graph's own label comes first.
        hover.hover(&ctx, oid(1));
        let rows = hover.reached(&ctx, &history, Some(oid(2)), true).unwrap();
        assert!(rows.contains(0) && rows.contains(1));
        drop(rows);

        // A hover from an earlier frame came from a view no longer drawn.
        hover.hover(&ctx, oid(2));
        ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        assert!(hover.reached(&ctx, &history, None, true).is_none());
    }

    #[test]
    fn the_rows_are_computed_again_for_a_new_history_only() {
        let first = history(&[(2, &[1]), (1, &[])]);
        let ctx = egui::Context::default();
        let mut hover = RefHover::default();
        hover.hover(&ctx, oid(2));
        let rows = hover.reached(&ctx, &first, None, true).unwrap();
        assert_eq!(reached(&rows, 2), vec![0, 1]);
        drop(rows);

        // The same tip on a history with a commit above it.
        let second = history(&[(3, &[2]), (2, &[1]), (1, &[])]);
        hover.hover(&ctx, oid(2));
        let rows = hover.reached(&ctx, &second, None, true).unwrap();
        assert_eq!(reached(&rows, 3), vec![1, 2]);
    }

    /// The rows `rows` holds, as full commit ids.
    fn ids(history: &History, rows: &Rows) -> std::collections::BTreeSet<String> {
        (0..history.commits.len()).filter(|row| rows.contains(*row)).map(|row| history.commits[row].oid.to_string()).collect()
    }

    #[test]
    fn a_real_branch_reaches_what_git_says_it_contains() {
        use crate::tests::{Fixture, git};
        let fx = Fixture::new("reach");
        fx.write("a.txt", "1\n");
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "feature"]);
        fx.write("b.txt", "1\n");
        fx.commit_all("feature 1");
        fx.write("b.txt", "2\n");
        fx.commit_all("feature 2");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("a.txt", "2\n");
        fx.commit_all("main 1");
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]);
        git(&fx.dir, &["checkout", "-q", "-b", "side", "HEAD~1"]);
        fx.write("c.txt", "1\n");
        fx.commit_all("side 1");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.handle.set_limit(usize::MAX);
        let snap = fx.snapshot();

        let ctx = egui::Context::default();
        let mut hover = RefHover::default();
        for branch in ["main", "feature", "side"] {
            let tip = snap.branches.iter().find(|b| b.name == branch).and_then(|b| b.oid).unwrap();
            hover.hover(&ctx, tip);
            let rows = hover.reached(&ctx, &snap.history, None, true).unwrap();
            let expected = git(&fx.dir, &["rev-list", branch]).lines().map(str::to_owned).collect();
            assert_eq!(ids(&snap.history, &rows), expected, "{branch}");
        }
    }

    /// How long loading and highlighting take on a big repository. Run with
    /// `GITKRILL_BIG_REPO=<working directory> cargo test --release big_repository -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn big_repository_timings() {
        use std::time::Instant;
        let Some(dir) = std::env::var_os("GITKRILL_BIG_REPO") else { return };
        for limit in [500, 3000, 6000, 12_000, usize::MAX] {
            // The best of three, each with a fresh loader so its caches are
            // empty (the operating system's file cache is warm after the first).
            let mut best = None;
            let mut last = None;
            for _ in 0..3 {
                let mut loader = crate::git::repo::Loader::open(std::path::Path::new(&dir)).unwrap();
                let started = Instant::now();
                let snap = loader.snapshot(limit, &Default::default()).unwrap();
                let took = started.elapsed();
                best = Some(best.map_or(took, |best: std::time::Duration| best.min(took)));
                last = Some((loader, snap));
            }
            let ((loader, snap), loaded) = (last.unwrap(), best.unwrap());
            let history = &snap.history;
            let bodies: usize = history.commits.iter().map(|c| c.body_preview.capacity()).sum();
            let with_body = history.commits.iter().filter(|c| !c.body_preview.is_empty()).count();

            // What the description adds to reading a commit: its body,
            // flattened onto one line the way `commit_info` does it.
            let read: Vec<git2::Commit<'_>> =
                history.commits.iter().filter_map(|c| loader.repo().find_commit(c.oid).ok()).collect();
            let started = Instant::now();
            let mut flattened = 0;
            for commit in &read {
                let Some(body) = commit.body_bytes() else { continue };
                let text = String::from_utf8_lossy(body);
                let mut flat = String::new();
                for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    if !flat.is_empty() {
                        flat.push_str(" | ");
                    }
                    flat.push_str(line);
                    if flat.len() > 240 {
                        break;
                    }
                }
                flattened += flat.len();
            }
            let describe = started.elapsed();
            assert!(flattened > 0 || with_body == 0);

            let ctx = egui::Context::default();
            let mut hover = RefHover::default();
            let tip = snap.head.oid.unwrap();
            hover.hover(&ctx, tip);
            let started = Instant::now();
            let rows = hover.reached(&ctx, &snap.history, None, true).unwrap();
            let first = started.elapsed();
            drop(rows);
            hover.hover(&ctx, tip);
            let started = Instant::now();
            let rows = hover.reached(&ctx, &snap.history, None, true).unwrap();
            let again = started.elapsed();
            let reached = (0..history.commits.len()).filter(|row| rows.contains(*row)).count();
            println!(
                "limit {limit:>20}: {:>6} rows in {loaded:>9.1?}; descriptions {with_body} rows, {} KiB, \
                 {describe:.2?}; highlight {reached} rows in {first:.2?} (then {again:.2?})",
                history.commits.len(),
                bodies / 1024,
            );
        }
    }

    #[test]
    fn many_rows_take_one_pass() {
        // A long line of commits; the hovered one is halfway down.
        let count = 5000u32;
        let mut commits = Vec::new();
        let id = |n: u32| {
            let mut bytes = [0u8; 20];
            bytes[16..].copy_from_slice(&n.to_be_bytes());
            Oid::from_bytes(&bytes).unwrap()
        };
        for n in (1..=count).rev() {
            let mut parents = Vec::new();
            if n > 1 {
                parents.push(id(n - 1));
            }
            commits.push(CommitInfo {
                oid: id(n),
                parents,
                summary: String::new(),
                body_preview: String::new(),
                author: String::new(),
                email: String::new(),
                time: 0,
                kind: RowKind::Commit,
            });
        }
        let graph = Graph::build(&commits);
        let index_of = commits.iter().enumerate().map(|(i, c)| (c.oid, i as u32)).collect();
        let history = Arc::new(History { commits, graph, index_of, has_more: false });

        let ctx = egui::Context::default();
        let mut hover = RefHover::default();
        hover.hover(&ctx, id(2500));
        let rows = hover.reached(&ctx, &history, None, true).unwrap();
        assert!(!rows.contains(2499));
        assert!((2500..5000).all(|row| rows.contains(row)));
    }
}
