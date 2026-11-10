//! Lane layout for the commit graph.
//!
//! Commits arrive children-first. Each row gets a lane for its node plus a
//! list of segments describing what to draw inside that row:
//!
//! * `Through(l)` - a line passes straight down lane `l`.
//! * `In(l)`      - a line enters from the top of lane `l` and ends at the node.
//! * `Out(l)`     - a line leaves the node and exits at the bottom of lane `l`.
//!
//! Colours belong to branch lines, not to columns: every time a lane is opened
//! it takes the next palette colour that no line currently running is using.
//! Two branches that reuse the same column one after the other, or run side
//! by side, are therefore always told apart by colour.

use super::{CommitInfo, Oid};

/// Number of distinct line colours handed out before they repeat.
pub const COLORS: u8 = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegKind {
    Through,
    In,
    Out,
}

#[derive(Clone, Copy, Debug)]
pub struct Seg {
    pub kind: SegKind,
    pub lane: u16,
    /// Palette index of the line this segment belongs to.
    pub color: u8,
}

#[derive(Clone, Copy, Debug)]
struct RowLayout {
    lane: u16,
    color: u8,
    start: u32,
    len: u16,
}

/// What to draw for one row.
pub struct Row<'a> {
    pub lane: u16,
    pub color: u8,
    pub segs: &'a [Seg],
}

#[derive(Default)]
pub struct Graph {
    rows: Vec<RowLayout>,
    segs: Vec<Seg>,
    /// Highest lane index used anywhere.
    pub max_lane: u16,
}

/// A lane waiting for a commit, drawn in a palette colour.
type Lane = Option<(Oid, u8)>;

impl Graph {
    pub fn build(commits: &[CommitInfo]) -> Graph {
        let mut rows = Vec::with_capacity(commits.len());
        let mut segs: Vec<Seg> = Vec::with_capacity(commits.len() * 3);
        let mut lanes: Vec<Lane> = Vec::new();
        let mut next_color = 0u8;
        let mut max_lane = 0u16;

        for commit in commits {
            let start = segs.len() as u32;

            // Lanes that were waiting for this commit end here. The node sits
            // on the leftmost of them and inherits its colour.
            let mut node: Option<(usize, u8)> = None;
            for (j, slot) in lanes.iter_mut().enumerate() {
                if let Some((oid, color)) = *slot
                    && oid == commit.oid
                {
                    node.get_or_insert((j, color));
                    segs.push(Seg { kind: SegKind::In, lane: j as u16, color });
                    *slot = None;
                }
            }
            let (lane, color) = match node {
                Some(found) => found,
                None => {
                    let color = pick_color(&lanes, &mut next_color);
                    (alloc(&mut lanes), color)
                }
            };

            // Everything still occupied is unrelated and passes through.
            for (j, slot) in lanes.iter().enumerate() {
                if let Some((_, color)) = slot {
                    segs.push(Seg { kind: SegKind::Through, lane: j as u16, color: *color });
                }
            }

            for (pi, parent) in commit.parents.iter().enumerate() {
                if commit.parents[..pi].contains(parent) {
                    continue;
                }
                let waiting = lanes.iter().position(|s| s.is_some_and(|(oid, _)| oid == *parent));
                if pi == 0 {
                    // The first parent continues this line, even when another
                    // lane already waits for it: both run in parallel and
                    // converge on the parent's row.
                    lanes[lane] = Some((*parent, color));
                    segs.push(Seg { kind: SegKind::Out, lane: lane as u16, color });
                } else if let Some(j) = waiting {
                    if j != lane {
                        let line = lanes[j].map_or(color, |(_, c)| c);
                        segs.push(Seg { kind: SegKind::Out, lane: j as u16, color: line });
                    }
                } else {
                    // A merged-in branch starts a line of its own.
                    let line = pick_color(&lanes, &mut next_color);
                    let j = alloc(&mut lanes);
                    lanes[j] = Some((*parent, line));
                    segs.push(Seg { kind: SegKind::Out, lane: j as u16, color: line });
                }
            }

            while matches!(lanes.last(), Some(None)) {
                lanes.pop();
            }

            let row_segs = &segs[start as usize..];
            let row_max = row_segs.iter().map(|s| s.lane).max().unwrap_or(0).max(lane as u16);
            max_lane = max_lane.max(row_max);
            rows.push(RowLayout {
                lane: lane as u16,
                color,
                start,
                len: (segs.len() as u32 - start) as u16,
            });
        }

        Graph { rows, segs, max_lane }
    }

    /// Node lane, node colour and segments for row `i`.
    pub fn row(&self, i: usize) -> Row<'_> {
        let r = self.rows[i];
        Row {
            lane: r.lane,
            color: r.color,
            segs: &self.segs[r.start as usize..r.start as usize + r.len as usize],
        }
    }

    #[cfg(test)]
    pub fn lane_of(&self, i: usize) -> u16 {
        self.rows[i].lane
    }

    #[cfg(test)]
    pub fn color_of(&self, i: usize) -> u8 {
        self.rows[i].color
    }
}

/// First free lane, growing the list when everything is taken. The slot is
/// left empty; the caller fills it.
fn alloc(lanes: &mut Vec<Lane>) -> usize {
    match lanes.iter().position(|s| s.is_none()) {
        Some(j) => j,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

/// The next palette colour not used by any line currently running. Colours
/// are handed out in rotation so consecutive branches differ too.
fn pick_color(lanes: &[Lane], next: &mut u8) -> u8 {
    let in_use = |color: u8| lanes.iter().any(|s| s.is_some_and(|(_, c)| c == color));
    let chosen = (0..COLORS)
        .map(|step| (*next + step) % COLORS)
        .find(|color| !in_use(*color))
        .unwrap_or(*next % COLORS);
    *next = (chosen + 1) % COLORS;
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RowKind;

    fn oid(n: u8) -> Oid {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        Oid::from_bytes(&bytes).unwrap()
    }

    fn commit(id: u8, parents: &[u8]) -> CommitInfo {
        CommitInfo {
            oid: oid(id),
            parents: parents.iter().map(|p| oid(*p)).collect(),
            summary: String::new(),
            body_preview: String::new(),
            author: String::new(),
            email: String::new(),
            time: 0,
            kind: RowKind::Commit,
        }
    }

    #[test]
    fn linear_history_stays_on_lane_zero() {
        let g = Graph::build(&[commit(3, &[2]), commit(2, &[1]), commit(1, &[])]);
        assert_eq!(g.max_lane, 0);
        assert!((0..3).all(|i| g.lane_of(i) == 0));
        assert!((0..3).all(|i| g.color_of(i) == 0));
    }

    #[test]
    fn merge_opens_and_closes_a_second_lane() {
        // 4 merges 3 (lane 0) and 2 (side branch); both descend from 1.
        let g = Graph::build(&[
            commit(4, &[3, 2]),
            commit(3, &[1]),
            commit(2, &[1]),
            commit(1, &[]),
        ]);
        assert_eq!(g.lane_of(0), 0);
        assert_eq!(g.lane_of(1), 0);
        assert_eq!(g.lane_of(2), 1);
        assert_eq!(g.lane_of(3), 0);
        assert_eq!(g.max_lane, 1);

        // The merge row leaves on both lanes.
        let outs: Vec<u16> =
            g.row(0).segs.iter().filter(|s| s.kind == SegKind::Out).map(|s| s.lane).collect();
        assert_eq!(outs, vec![0, 1]);

        // The root row receives both lanes.
        let ins: Vec<u16> =
            g.row(3).segs.iter().filter(|s| s.kind == SegKind::In).map(|s| s.lane).collect();
        assert_eq!(ins, vec![0, 1]);
    }

    #[test]
    fn two_tips_sharing_a_parent_run_in_parallel() {
        let g = Graph::build(&[commit(3, &[1]), commit(2, &[1]), commit(1, &[])]);
        assert_eq!(g.lane_of(0), 0);
        assert_eq!(g.lane_of(1), 1);
        assert_eq!(g.lane_of(2), 0);
        assert!(g.row(1).segs.iter().any(|s| s.kind == SegKind::Through && s.lane == 0));
        assert_ne!(g.color_of(0), g.color_of(1));
    }

    #[test]
    fn every_branch_line_gets_its_own_colour() {
        // Two side branches merged one after the other reuse lane 1, but not
        // its colour.
        let g = Graph::build(&[
            commit(7, &[6, 5]),
            commit(6, &[4, 3]),
            commit(5, &[4]),
            commit(4, &[2]),
            commit(3, &[2]),
            commit(2, &[]),
        ]);
        let main = g.color_of(0);
        assert_eq!(g.color_of(1), main);
        assert_eq!(g.color_of(3), main);
        let (first, second) = (g.color_of(2), g.color_of(4));
        assert_ne!(first, main);
        assert_ne!(second, main);
        assert_ne!(first, second);
        // A line keeps one colour from its merge down to its own commits.
        let out = g.row(0).segs.iter().find(|s| s.kind == SegKind::Out && s.lane != 0).unwrap();
        assert_eq!(out.color, first);
    }
}
