//! Synthetic data for the `--fake` render benchmark.
//!
//! A seeded simulation decides the shape of a commit DAG (which commits branch,
//! merge or end), and the rows are laid out by the same
//! [`GraphLayout`](lamina_core::graph::GraphLayout) a real repository uses. The
//! benchmark therefore draws exactly the geometry users will see, including the
//! first and last rows.

use std::ops::Range;

use lamina_core::Oid;
use lamina_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
use lamina_core::graph::{GraphLayout, GraphRows};

/// Number of rows the benchmark hands to the commit list.
pub const COMMIT_COUNT: usize = 200_000;

/// The simulation reuses lanes aggressively so the graph column stays narrow
/// enough to leave room for the commit columns.
const MAX_LANES: usize = 6;
/// 2026-11-13T00:00:00Z, so generated dates look current.
const BASE_TIME: i64 = 1_794_528_000;
const SECONDS_PER_COMMIT: i64 = 137;

/// One commit as the list needs it. Text is produced on demand instead of
/// keeping 200k `String`s alive.
pub struct CommitRow {
    pub id: u64,
    pub author: u8,
    pub time: i64,
    pub summary_seed: u32,
}

pub struct FakeRepo {
    pub commits: Vec<CommitRow>,
    pub graph: GraphRows,
    /// Widest row in `graph`. Every row paints into a column of this width so
    /// lanes stay on the same x coordinate from one row to the next.
    pub max_width: u16,
}

impl FakeRepo {
    /// Simulates a DAG in display order (children before parents) and lays it
    /// out row by row. Commits on the same lane are emitted in a run so the
    /// graph reads like `git log --graph` rather than a breadth-first walk.
    pub fn generate(count: usize) -> Self {
        let mut rng = Rng(0x5EED_1234_9ABC_DEF1);
        let mut next_id = 1u64;
        // Each slot holds the id a lane is waiting to see as a commit.
        let mut lanes: Vec<Option<u64>> = vec![Some(0)];
        let mut commits = Vec::with_capacity(count);
        let mut layout = GraphLayout::new();
        let mut graph = GraphRows::new();
        let mut parents: Vec<Oid> = Vec::with_capacity(2);
        let mut last_lane = 0usize;

        for index in 0..count {
            let current = if lanes[last_lane].is_some() && !rng.chance(0.08) {
                last_lane
            } else {
                rng.pick_occupied(&lanes).unwrap_or(last_lane)
            };
            last_lane = current;

            let node = lanes[current].expect("the chosen lane has a pending commit");
            // Other lanes waiting for the same commit converge on it here.
            for (lane, slot) in lanes.iter_mut().enumerate() {
                if lane != current && *slot == Some(node) {
                    *slot = None;
                }
            }

            parents.clear();
            let remaining = lanes.iter().filter(|slot| slot.is_some()).count();
            let last_row = index + 1 == count;
            // Ending a branch must still leave a lane alive for the next row.
            let ends_branch = last_row || (remaining > 1 && rng.chance(0.03));
            if ends_branch {
                lanes[current] = None;
            } else {
                // Occasionally the first parent is a commit another lane is
                // already waiting for, so the two lanes converge later on.
                let join = if rng.chance(0.06) {
                    rng.pick_occupied_except(&lanes, current)
                } else {
                    None
                };
                let first = match join {
                    Some(target) => lanes[target].expect("occupied lane"),
                    None => {
                        next_id += 1;
                        next_id - 1
                    }
                };
                lanes[current] = Some(first);
                parents.push(oid(first));

                // A second parent forks a lane. If none is free the commit
                // simply stays linear.
                if rng.chance(0.08)
                    && let Some(lane) = free_lane(&lanes, current)
                {
                    if lane == lanes.len() {
                        lanes.push(None);
                    }
                    lanes[lane] = Some(next_id);
                    parents.push(oid(next_id));
                    next_id += 1;
                }
            }

            layout.push(oid(node), &parents, &mut graph);
            commits.push(CommitRow {
                id: node,
                author: rng.below(24) as u8,
                time: BASE_TIME - index as i64 * SECONDS_PER_COMMIT,
                summary_seed: rng.next_u32(),
            });
        }

        let max_width = graph.iter().map(|row| row.width).max().unwrap_or(1);
        Self {
            commits,
            graph,
            max_width,
        }
    }
}

/// Synthetic ids only need to be distinct; the layout never reads objects.
fn oid(id: u64) -> Oid {
    let mut bytes = [0u8; 20];
    bytes[12..].copy_from_slice(&id.to_be_bytes());
    Oid::from_bytes_or_panic(&bytes)
}

fn free_lane(lanes: &[Option<u64>], current: usize) -> Option<usize> {
    lanes
        .iter()
        .enumerate()
        .find(|(lane, slot)| *lane != current && slot.is_none())
        .map(|(lane, _)| lane)
        .or_else(|| (lanes.len() < MAX_LANES).then_some(lanes.len()))
}

/// SplitMix64: tiny, deterministic, and independent of the `rand` crate so the
/// generated graph is identical on every machine and every run.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }

    fn chance(&mut self, probability: f32) -> bool {
        let unit = (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32;
        unit < probability
    }

    fn pick_occupied(&mut self, lanes: &[Option<u64>]) -> Option<usize> {
        let mut seen = 0;
        let target = self.below(lanes.iter().filter(|slot| slot.is_some()).count().max(1));
        for (lane, slot) in lanes.iter().enumerate() {
            if slot.is_some() {
                if seen == target {
                    return Some(lane);
                }
                seen += 1;
            }
        }
        None
    }

    fn pick_occupied_except(&mut self, lanes: &[Option<u64>], except: usize) -> Option<usize> {
        let candidates: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(lane, slot)| *lane != except && slot.is_some())
            .map(|(lane, _)| lane)
            .collect();
        if candidates.is_empty() {
            None
        } else {
            Some(candidates[self.below(candidates.len())])
        }
    }
}

const VERBS: [&str; 12] = [
    "Add", "Fix", "Refactor", "Optimise", "Document", "Split", "Rename", "Cache", "Guard",
    "Simplify", "Extend", "Drop",
];

const OBJECTS: [&str; 12] = [
    "lane layout",
    "diff highlighting",
    "commit walker",
    "ref picker",
    "status refresh",
    "scroll anchoring",
    "hunk splitting",
    "worktree lock",
    "reflog reader",
    "path filter",
    "index cache",
    "error toast",
];

const AREAS: [&str; 10] = [
    "graph",
    "diff",
    "repo",
    "git_cli",
    "watch",
    "highlight",
    "patch",
    "status",
    "log",
    "app",
];

const AUTHORS: [&str; 24] = [
    "Ada Lovelace",
    "Grace Hopper",
    "Alan Kay",
    "Barbara Liskov",
    "Ken Thompson",
    "Margaret Hamilton",
    "Dennis Ritchie",
    "Radia Perlman",
    "Rob Pike",
    "Frances Allen",
    "Linus Torvalds",
    "Anita Borg",
    "Edsger Dijkstra",
    "Karen Spärck Jones",
    "Bjarne Stroustrup",
    "Sophie Wilson",
    "Guido van Rossum",
    "Jean Bartik",
    "Brian Kernighan",
    "Evelyn Boyd Granville",
    "John Carmack",
    "Carol Shaw",
    "Anders Hejlsberg",
    "Shafi Goldwasser",
];

pub fn summary(seed: u32) -> String {
    let seed = seed as usize;
    format!(
        "{} {} in {}",
        VERBS[seed % VERBS.len()],
        OBJECTS[(seed >> 5) % OBJECTS.len()],
        AREAS[(seed >> 11) % AREAS.len()]
    )
}

pub fn author(index: u8) -> &'static str {
    AUTHORS[index as usize % AUTHORS.len()]
}

pub fn short_id(id: u64) -> String {
    format!(
        "{:07x}",
        (id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 36) as u32
    )
}

/// Builds a diff with a few hundred lines and word-level emphasis, shaped like
/// what `lamina_core::diff` will return once the diff task lands.
pub fn sample_diff(seed: u32) -> FileDiff {
    FileDiff {
        hunks: (0..5)
            .map(|number| sample_hunk(number, seed.wrapping_add(number * 17)))
            .collect(),
        binary: false,
    }
}

fn sample_hunk(number: u32, seed: u32) -> Hunk {
    let old_start = 40 + number * 210;
    let new_start = old_start + number * 7;
    let mut lines = Vec::with_capacity(64);
    let mut old_no = old_start;
    let mut new_no = new_start;

    for step in 0..60u32 {
        match (step + seed) % 9 {
            6 => {
                let identifier = IDENTIFIERS[((seed + step) % IDENTIFIERS.len() as u32) as usize];
                let old = format!(
                    "    let {identifier}_color = palette[{} % palette.len()];",
                    old_no
                );
                let new = format!(
                    "    let {identifier}_colour = palette[{} % palette.len()];",
                    new_no
                );
                let (old_emphasis, new_emphasis) = changed_region(&old, &new);
                lines.push(diff_line(
                    LineKind::Removed,
                    Some(old_no),
                    None,
                    old,
                    vec![old_emphasis],
                ));
                lines.push(diff_line(
                    LineKind::Added,
                    None,
                    Some(new_no),
                    new,
                    vec![new_emphasis],
                ));
                old_no += 1;
                new_no += 1;
            }
            8 if number.is_multiple_of(2) => {
                let content = context_text(old_no);
                lines.push(diff_line(
                    LineKind::Removed,
                    Some(old_no),
                    None,
                    content,
                    Vec::new(),
                ));
                old_no += 1;
            }
            7 if !number.is_multiple_of(2) => {
                let content = context_text(new_no + 1);
                lines.push(diff_line(
                    LineKind::Added,
                    None,
                    Some(new_no),
                    content,
                    Vec::new(),
                ));
                new_no += 1;
            }
            _ => {
                let content = context_text(old_no);
                lines.push(diff_line(
                    LineKind::Context,
                    Some(old_no),
                    Some(new_no),
                    content,
                    Vec::new(),
                ));
                old_no += 1;
                new_no += 1;
            }
        }
    }

    Hunk {
        old_start,
        old_len: old_no - old_start,
        new_start,
        new_len: new_no - new_start,
        lines,
    }
}

const IDENTIFIERS: [&str; 12] = [
    "node", "lane", "segment", "hunk", "diff", "cache", "index", "row", "graph", "picker",
    "status", "render",
];

fn context_text(number: u32) -> String {
    match number % 6 {
        0 => format!("    let node = layout.row({number});"),
        1 => format!("    let width = row.width.max({});", number % 12),
        2 => format!("    self.cache.insert({number}, row.node_lane);"),
        3 => format!("    // lanes above {number} stay reserved for the graph"),
        4 => format!("    window.paint_path(path, palette[{}]);", number % 8),
        _ => format!("    segments.extend(row.segments.iter().filter(|s| s.to_lane < {number}));"),
    }
}

fn diff_line(
    kind: LineKind,
    old_lineno: Option<u32>,
    new_lineno: Option<u32>,
    content: String,
    emphasis: Vec<Range<usize>>,
) -> DiffLine {
    let emphasis = emphasis
        .into_iter()
        .filter(|range| range.start < range.end && range.end <= content.len())
        .collect();
    DiffLine {
        kind,
        old_lineno,
        new_lineno,
        content,
        no_newline_at_eof: false,
        emphasis,
    }
}

/// Marks the region that differs between two versions of a line by trimming
/// their common prefix and suffix. That is what a word-level diff would
/// emphasise for the identifier renames generated above.
fn changed_region(old: &str, new: &str) -> (Range<usize>, Range<usize>) {
    let old_chars: Vec<(usize, char)> = old.char_indices().collect();
    let new_chars: Vec<(usize, char)> = new.char_indices().collect();

    let mut prefix = 0;
    while prefix < old_chars.len()
        && prefix < new_chars.len()
        && old_chars[prefix].1 == new_chars[prefix].1
    {
        prefix += 1;
    }

    let mut suffix = 0;
    while suffix < old_chars.len() - prefix
        && suffix < new_chars.len() - prefix
        && old_chars[old_chars.len() - 1 - suffix].1 == new_chars[new_chars.len() - 1 - suffix].1
    {
        suffix += 1;
    }

    (
        byte_range(&old_chars, prefix, suffix, old.len()),
        byte_range(&new_chars, prefix, suffix, new.len()),
    )
}

fn byte_range(chars: &[(usize, char)], prefix: usize, suffix: usize, len: usize) -> Range<usize> {
    let start = chars.get(prefix).map(|(index, _)| *index).unwrap_or(len);
    let end = chars
        .get(chars.len().saturating_sub(suffix))
        .map(|(index, _)| *index)
        .unwrap_or(len);
    start..end.max(start)
}

#[cfg(test)]
mod tests {
    use lamina_core::graph::Half;

    use super::*;

    #[test]
    fn graph_rows_stay_inside_their_width() {
        let repo = FakeRepo::generate(4_000);
        for row in repo.graph.iter() {
            assert!(row.node_lane < row.width);
            for segment in row.edges {
                assert!(segment.from_lane < row.width);
                assert!(segment.to_lane < row.width);
            }
            for pass in row.passes {
                assert!(pass.lane < row.width);
            }
        }
    }

    #[test]
    fn first_row_has_nothing_above_its_node() {
        let repo = FakeRepo::generate(500);
        let first = repo.graph.get(0).expect("rows were generated");
        assert!(first.passes.is_empty());
        assert!(
            first
                .edges
                .iter()
                .all(|segment| segment.half == Half::Bottom)
        );
    }

    #[test]
    fn emphasis_ranges_land_on_char_boundaries() {
        let diff = sample_diff(7);
        for hunk in &diff.hunks {
            for line in &hunk.lines {
                for range in &line.emphasis {
                    assert!(line.content.is_char_boundary(range.start));
                    assert!(line.content.is_char_boundary(range.end));
                }
            }
        }
    }
}
