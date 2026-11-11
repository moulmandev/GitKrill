//! Which commits are selected together: Ctrl+click toggles one, Shift+click
//! takes every commit between the anchor and the one clicked (in graph
//! order), Shift+Up / Down does the same from the keyboard, and a plain click
//! goes back to one commit.
//!
//! The commit the details follow, and that the arrow keys move from, stays
//! the tab's `selection` (the *focus*); [`Picked`] holds the others with it.
//! Fewer than two commits is no multi-selection at all, so every view that
//! only knows one selected commit keeps working unchanged. The WIP row and
//! stashes never join: a Ctrl or Shift click on them selects them alone.

use std::collections::HashSet;

use crate::git::{History, Oid, RowKind};

#[derive(Clone, Debug, Default)]
pub struct Picked {
    set: HashSet<Oid>,
    /// Where a Shift range starts: the last commit clicked without Shift.
    anchor: Option<Oid>,
    /// Changes whenever `set` does, for whatever is worked out from it.
    generation: u64,
}

/// A commit that can be selected with others: a loaded row, not the WIP row
/// or a stash.
pub fn selectable(history: &History, oid: Oid) -> bool {
    row(history, oid).is_some_and(|r| history.commits[r].kind == RowKind::Commit)
}

fn row(history: &History, oid: Oid) -> Option<usize> {
    history.index_of.get(&oid).map(|i| *i as usize)
}

impl Picked {
    /// Two or more commits are selected.
    pub fn is_active(&self) -> bool {
        self.set.len() >= 2
    }

    pub fn contains(&self, oid: Oid) -> bool {
        self.set.contains(&oid)
    }

    pub fn set(&self) -> &HashSet<Oid> {
        &self.set
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        // One commit left is an ordinary selection.
        if self.set.len() == 1 {
            self.set.clear();
        }
    }

    /// Back to one commit, or none: what a plain click does. `anchor` is
    /// where a later Shift+click starts from.
    pub fn single(&mut self, anchor: Option<Oid>) {
        if !self.set.is_empty() {
            self.set.clear();
            self.changed();
        }
        self.anchor = anchor;
    }

    /// Exactly these commits (comparing two, say), focus on the first.
    pub fn select(&mut self, oids: &[Oid]) {
        self.set = oids.iter().copied().collect();
        self.anchor = oids.first().copied();
        self.changed();
    }

    /// Ctrl+click on `oid`, `focus` being the commit selected so far.
    /// Returns the commit to focus afterwards.
    pub fn toggle(&mut self, history: &History, focus: Option<Oid>, oid: Oid) -> Oid {
        if self.set.is_empty()
            && let Some(focus) = focus.filter(|f| selectable(history, *f))
        {
            // Starting from one selected commit: it stays.
            self.set.insert(focus);
        }
        let next = if self.set.len() > 1 && self.set.remove(&oid) {
            let at = row(history, oid).unwrap_or(0);
            // The focus moves to the closest commit still selected.
            match focus.filter(|f| *f != oid && self.set.contains(f)) {
                Some(focus) => focus,
                None => self
                    .set
                    .iter()
                    .copied()
                    .min_by_key(|o| (row(history, *o).unwrap_or(usize::MAX).abs_diff(at), row(history, *o)))
                    .unwrap_or(oid),
            }
        } else {
            self.set.insert(oid);
            oid
        };
        self.anchor = Some(oid);
        self.changed();
        next
    }

    /// Shift+click on `oid`: every commit from the anchor to it, in graph
    /// order, instead of what was selected (`add`, for Ctrl+Shift: as well
    /// as). Without an anchor this is a plain selection of `oid`. Returns
    /// the commit to focus.
    pub fn extend(&mut self, history: &History, focus: Option<Oid>, oid: Oid, add: bool) -> Oid {
        let anchor = self
            .anchor
            .filter(|a| selectable(history, *a))
            .or(focus.filter(|f| selectable(history, *f)));
        let (Some(anchor), Some(to)) = (anchor, row(history, oid)) else {
            self.single(Some(oid));
            return oid;
        };
        let from = row(history, anchor).unwrap_or(to);
        if !add {
            self.set.clear();
        }
        let (low, high) = (from.min(to), from.max(to));
        self.set.extend(
            history.commits[low..=high].iter().filter(|c| c.kind == RowKind::Commit).map(|c| c.oid),
        );
        self.anchor = Some(anchor);
        self.changed();
        oid
    }

    /// The selected commits in graph order: newest first.
    pub fn ordered(&self, history: &History) -> Vec<Oid> {
        let mut rows: Vec<usize> = self.set.iter().filter_map(|oid| row(history, *oid)).collect();
        rows.sort_unstable();
        rows.into_iter().map(|r| history.commits[r].oid).collect()
    }

    /// After a refresh: forgets commits the history no longer has (or no
    /// longer loads). Returns the commit to focus instead of `focus` when
    /// that one went and others are left.
    pub fn retain(&mut self, history: &History, focus: Option<Oid>) -> Option<Oid> {
        let before = self.set.len();
        self.set.retain(|oid| selectable(history, *oid));
        if self.set.len() == before {
            return None;
        }
        let stays = focus.is_some_and(|f| self.set.contains(&f));
        let next = (!stays).then(|| self.ordered(history).first().copied()).flatten();
        self.changed();
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::CommitInfo;
    use crate::git::graph::Graph;

    fn oid(n: u8) -> Oid {
        Oid::from_bytes(&[n; 20]).unwrap()
    }

    /// Rows 0..n, each on the next; `kinds` overrides some rows.
    fn history(n: u8, kinds: &[(u8, RowKind)]) -> History {
        let commits: Vec<CommitInfo> = (0..n)
            .map(|i| CommitInfo {
                oid: oid(i + 1),
                parents: if i + 1 < n { vec![oid(i + 2)] } else { Vec::new() },
                summary: String::new(),
                body_preview: String::new(),
                author: String::new(),
                email: String::new(),
                time: 0,
                kind: kinds.iter().find(|(row, _)| *row == i).map_or(RowKind::Commit, |(_, kind)| *kind),
            })
            .collect();
        let graph = Graph::build(&commits);
        let index_of = commits.iter().enumerate().map(|(i, c)| (c.oid, i as u32)).collect();
        History { commits, graph, index_of, has_more: false }
    }

    fn picked(p: &Picked, h: &History) -> Vec<u8> {
        p.ordered(h).iter().map(|o| o.as_bytes()[0]).collect()
    }

    #[test]
    fn ctrl_click_adds_and_removes_commits() {
        let h = history(6, &[]);
        let mut p = Picked::default();
        p.single(Some(oid(2)));
        assert_eq!(p.toggle(&h, Some(oid(2)), oid(5)), oid(5));
        assert!(p.is_active());
        assert_eq!(picked(&p, &h), [2, 5]);
        assert_eq!(p.toggle(&h, Some(oid(5)), oid(4)), oid(4));
        assert_eq!(picked(&p, &h), [2, 4, 5]);
        // Taking the focused one out moves the focus to the closest left.
        assert_eq!(p.toggle(&h, Some(oid(4)), oid(4)), oid(5));
        assert_eq!(picked(&p, &h), [2, 5]);
        // Down to one commit: an ordinary selection again.
        assert_eq!(p.toggle(&h, Some(oid(5)), oid(2)), oid(5));
        assert!(!p.is_active());
        assert!(p.set().is_empty());
    }

    #[test]
    fn ctrl_click_on_the_only_selected_commit_keeps_it() {
        let h = history(4, &[]);
        let mut p = Picked::default();
        assert_eq!(p.toggle(&h, Some(oid(3)), oid(3)), oid(3));
        assert!(!p.is_active());
    }

    #[test]
    fn shift_click_takes_the_range_in_graph_order_from_the_anchor() {
        // Row 0 is the WIP row, row 3 a stash.
        let h = history(8, &[(0, RowKind::Wip), (3, RowKind::Stash)]);
        let mut p = Picked::default();
        p.single(Some(oid(6)));
        assert_eq!(p.extend(&h, Some(oid(6)), oid(2), false), oid(2));
        assert_eq!(picked(&p, &h), [2, 3, 5, 6], "the stash in between stays out");
        // Another Shift+click starts from the same anchor and replaces it.
        p.extend(&h, Some(oid(2)), oid(8), false);
        assert_eq!(picked(&p, &h), [6, 7, 8]);
        // Ctrl+Shift adds the range to what is there.
        p.toggle(&h, Some(oid(8)), oid(2));
        p.extend(&h, Some(oid(2)), oid(3), true);
        assert_eq!(picked(&p, &h), [2, 3, 6, 7, 8]);
    }

    #[test]
    fn shift_click_without_an_anchor_selects_one_commit() {
        let h = history(5, &[(0, RowKind::Wip)]);
        let mut p = Picked::default();
        // The WIP row was selected: no anchor.
        p.single(None);
        assert_eq!(p.extend(&h, None, oid(3), false), oid(3));
        assert!(!p.is_active());
        // Then the clicked commit is the anchor.
        p.extend(&h, Some(oid(3)), oid(5), false);
        assert_eq!(picked(&p, &h), [3, 4, 5]);
    }

    #[test]
    fn commits_that_disappear_are_dropped_and_the_focus_moves() {
        let h = history(6, &[]);
        let mut p = Picked::default();
        p.select(&[oid(2), oid(3), oid(5)]);
        assert_eq!(p.retain(&h, Some(oid(2))), None, "nothing went");

        // Everything after commit 2 rewritten away.
        let mut q = p.clone();
        let rewritten = history(2, &[]);
        let generation = q.generation();
        assert_eq!(q.retain(&rewritten, Some(oid(5))), Some(oid(2)), "the one left takes the focus");
        assert!(!q.is_active(), "one commit is no multi-selection");
        assert_ne!(q.generation(), generation);

        let after = history(4, &[]);
        let mut r = Picked::default();
        r.select(&[oid(1), oid(2), oid(6)]);
        assert_eq!(r.retain(&after, Some(oid(6))), Some(oid(1)), "the focus went, the newest left takes it");
        assert_eq!(picked(&r, &after), [1, 2]);
    }

    #[test]
    fn a_page_load_keeps_the_selection() {
        let h = history(4, &[]);
        let more = history(9, &[]);
        let mut p = Picked::default();
        p.select(&[oid(1), oid(4)]);
        let generation = p.generation();
        assert_eq!(p.retain(&more, Some(oid(1))), None);
        assert_eq!(picked(&p, &more), [1, 4]);
        assert_eq!(p.generation(), generation);
        assert_eq!(picked(&p, &h), [1, 4]);
    }
}
