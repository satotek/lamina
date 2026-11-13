//! Commit graph lane layout.
//!
//! Rows are produced incrementally: commits are pushed in display order
//! (topological, newest first) and each push appends one finished row to a
//! [`GraphRows`].
//!
//! Geometry model used by the renderer: every row has a node at
//! `(node_lane, row middle)`. A [`PassThrough`] is a straight line on its lane
//! from row top to row bottom. An edge [`Segment`] with `half == Top` runs from
//! `(from_lane, row top)` to `(to_lane, row middle)`; with `half == Bottom` it
//! runs from `(from_lane, row middle)` to `(to_lane, row bottom)`.
//!
//! Storage is compact because wide histories have dozens of lanes per row:
//! pass-throughs take 4 bytes instead of two 8-byte segments, and all rows
//! share two flat arrays instead of one allocation per row.

use std::ops::Range;

use crate::Oid;

/// Number of palette entries; lane colours rotate so neighbouring lines differ.
const PALETTE: u16 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Half {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Segment {
    pub from_lane: u16,
    pub to_lane: u16,
    pub half: Half,
    /// Palette index; the renderer maps it to a colour.
    pub color: u16,
}

/// A lane that runs straight through a row without touching its node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PassThrough {
    pub lane: u16,
    pub color: u16,
}

/// One row, borrowed from [`GraphRows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphRow<'a> {
    pub node_lane: u16,
    pub node_color: u16,
    /// Number of lanes that must be reserved horizontally for this row.
    pub width: u16,
    pub passes: &'a [PassThrough],
    /// Lines that end at or start from the node, including straight ones.
    pub edges: &'a [Segment],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowMeta {
    node_lane: u16,
    node_color: u16,
    width: u16,
    /// Exclusive ends into the shared arrays; starts are the previous row's ends.
    passes_end: u32,
    edges_end: u32,
}

/// Rows of the graph in display order, stored as two flat arrays.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphRows {
    rows: Vec<RowMeta>,
    passes: Vec<PassThrough>,
    edges: Vec<Segment>,
}

impl GraphRows {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<GraphRow<'_>> {
        let meta = self.rows.get(index)?;
        let (passes_start, edges_start) = match index.checked_sub(1) {
            Some(prev) => (self.rows[prev].passes_end, self.rows[prev].edges_end),
            None => (0, 0),
        };
        Some(GraphRow {
            node_lane: meta.node_lane,
            node_color: meta.node_color,
            width: meta.width,
            passes: &self.passes[passes_start as usize..meta.passes_end as usize],
            edges: &self.edges[edges_start as usize..meta.edges_end as usize],
        })
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = GraphRow<'_>> + '_ {
        (0..self.len()).map(|index| self.get(index).expect("index in range"))
    }

    /// Copies rows `range` of `other` onto the end of `self`.
    pub fn extend_from(&mut self, other: &GraphRows, range: Range<usize>) {
        for index in range {
            let row = other.get(index).expect("range within other");
            self.push_row(
                row.node_lane,
                row.node_color,
                row.width,
                row.passes,
                row.edges,
            );
        }
    }

    /// Bytes held, for memory budgeting.
    pub fn heap_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<RowMeta>()
            + self.passes.capacity() * std::mem::size_of::<PassThrough>()
            + self.edges.capacity() * std::mem::size_of::<Segment>()
    }

    pub fn shrink_to_fit(&mut self) {
        self.rows.shrink_to_fit();
        self.passes.shrink_to_fit();
        self.edges.shrink_to_fit();
    }

    /// Appends one row. Layout code goes through [`GraphLayout::push`]; this is
    /// for producers with their own lane model, such as synthetic test data.
    pub fn push_row(
        &mut self,
        node_lane: u16,
        node_color: u16,
        width: u16,
        passes: &[PassThrough],
        edges: &[Segment],
    ) {
        self.passes.extend_from_slice(passes);
        self.edges.extend_from_slice(edges);
        self.rows.push(RowMeta {
            node_lane,
            node_color,
            width,
            passes_end: u32::try_from(self.passes.len()).expect("fewer than 4G pass-throughs"),
            edges_end: u32::try_from(self.edges.len()).expect("fewer than 4G edges"),
        });
    }
}

/// A lane that is waiting for a commit which has not been pushed yet.
#[derive(Debug, Clone, Copy)]
struct Lane {
    expected: Oid,
    color: u16,
}

#[derive(Debug, Default)]
pub struct GraphLayout {
    /// One slot per lane: `Some` while a line descends towards a commit that has
    /// not been pushed yet, `None` when the lane is free for reuse.
    lanes: Vec<Option<Lane>>,
    /// Rotating palette cursor for newly drawn lanes.
    color_cursor: u16,
    /// Reused per push so laying out a row does not allocate.
    scratch_edges: Vec<Segment>,
    scratch_passes: Vec<PassThrough>,
    scratch_arriving: Vec<usize>,
    scratch_parents: Vec<Oid>,
}

impl GraphLayout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lays out the next commit in display order and appends its row to `out`.
    ///
    /// The layout keeps no rows itself, so callers can split `out` into chunks
    /// without the history being stored twice.
    pub fn push(&mut self, id: Oid, parents: &[Oid], out: &mut GraphRows) {
        let mut segments = std::mem::take(&mut self.scratch_edges);
        let mut passes = std::mem::take(&mut self.scratch_passes);
        let mut arriving = std::mem::take(&mut self.scratch_arriving);
        let mut scheduled = std::mem::take(&mut self.scratch_parents);
        segments.clear();
        passes.clear();
        arriving.clear();
        scheduled.clear();

        // Lanes a child already reserved for this commit. The leftmost one holds
        // the node; the others bend into it so merges stay visible.
        arriving.extend(
            self.lanes
                .iter()
                .enumerate()
                .filter(|(_, lane)| matches!(lane, Some(lane) if lane.expected == id))
                .map(|(lane, _)| lane),
        );

        let (node_lane, node_color) = match arriving.first().copied() {
            Some(first) => {
                let color = self.lane_color(first);
                for &other in &arriving[1..] {
                    segments.push(Segment {
                        from_lane: other as u16,
                        to_lane: first as u16,
                        half: Half::Top,
                        color: self.lane_color(other),
                    });
                    self.lanes[other] = None;
                }
                // The lane carried a line in from the row above.
                segments.push(Segment {
                    from_lane: first as u16,
                    to_lane: first as u16,
                    half: Half::Top,
                    color,
                });
                self.lanes[first] = None;
                (first, color)
            }
            None => {
                // A branch tip nobody reserved: leftmost free lane, new colour.
                let lane = self.free_lane();
                (lane, self.take_color())
            }
        };

        // Lanes that neither end nor start here run straight through the row.
        // Collected before parent lanes are allocated so that a lane freed by a
        // merge above can be reused by a parent of this very commit.
        for (index, lane) in self.lanes.iter().enumerate() {
            if let Some(lane) = lane {
                passes.push(PassThrough {
                    lane: index as u16,
                    color: lane.color,
                });
            }
        }

        for &parent in parents {
            // Duplicate or self parents would draw zero-width edges twice.
            if parent == id || scheduled.contains(&parent) {
                continue;
            }
            let is_first = scheduled.is_empty();
            scheduled.push(parent);

            let (lane, color) = if is_first {
                // The first-parent chain stays in this lane and keeps its colour.
                (node_lane, node_color)
            } else if let Some(existing) = self.lane_waiting_for(parent) {
                // Another child already opened a lane for this parent; join it.
                (existing, self.lane_color(existing))
            } else {
                let lane = self.free_lane();
                (lane, self.take_color())
            };

            self.lanes[lane] = Some(Lane {
                expected: parent,
                color,
            });
            segments.push(Segment {
                from_lane: node_lane as u16,
                to_lane: lane as u16,
                half: Half::Bottom,
                color,
            });
        }

        // Deterministic order keeps snapshots stable and is cheap to sort.
        segments.sort_by_key(|segment| {
            (
                segment.half == Half::Bottom,
                segment.from_lane,
                segment.to_lane,
            )
        });

        let max_lane = segments
            .iter()
            .flat_map(|segment| [segment.from_lane, segment.to_lane])
            .chain(passes.iter().map(|pass| pass.lane))
            .chain([node_lane as u16])
            .max()
            .unwrap_or(0);

        out.push_row(
            node_lane as u16,
            node_color,
            max_lane + 1,
            &passes,
            &segments,
        );

        self.scratch_edges = segments;
        self.scratch_passes = passes;
        self.scratch_arriving = arriving;
        self.scratch_parents = scheduled;
    }

    fn lane_color(&self, lane: usize) -> u16 {
        self.lanes
            .get(lane)
            .copied()
            .flatten()
            .map_or(0, |lane| lane.color)
    }

    fn lane_waiting_for(&self, id: Oid) -> Option<usize> {
        self.lanes
            .iter()
            .position(|lane| matches!(lane, Some(lane) if lane.expected == id))
    }

    /// Leftmost free lane, extending the lane vector when every slot is taken.
    fn free_lane(&mut self) -> usize {
        match self.lanes.iter().position(Option::is_none) {
            Some(lane) => lane,
            None => {
                self.lanes.push(None);
                self.lanes.len() - 1
            }
        }
    }

    fn take_color(&mut self) -> u16 {
        let color = self.color_cursor;
        self.color_cursor = (color + 1) % PALETTE;
        color
    }
}

/// Render rows as text so layouts can be compared in snapshot tests.
///
/// Every row becomes three lines (top half, middle with the node, bottom half).
/// A lane occupies two characters: the line glyph and its palette index. Empty
/// cells are `..`, the node is `*`, crossings are `/` and `\`.
pub fn render_ascii(rows: &GraphRows, labels: &[&str]) -> String {
    let mut out = String::new();

    for (index, row) in rows.iter().enumerate() {
        let segments = expanded_segments(row);

        let computed_width = segments
            .iter()
            .flat_map(|segment| [segment.from_lane, segment.to_lane])
            .chain([row.node_lane])
            .max()
            .unwrap_or(0)
            + 1;
        let width = (row.width.max(computed_width)) as usize;

        let mut top = vec![None; width];
        let mut middle = vec![None; width];
        let mut bottom = vec![None; width];

        for segment in &segments {
            let from = segment.from_lane as usize;
            let to = segment.to_lane as usize;
            match segment.half {
                Half::Top => {
                    top[from] = Some(('|', segment.color));
                    middle[from] = Some(if from == to {
                        ('|', segment.color)
                    } else {
                        (bend(from, to), segment.color)
                    });
                }
                Half::Bottom => {
                    // A bottom segment bends on the line where it lands, so the
                    // glyph sits at the target lane. A lane can keep running and
                    // also receive an edge; `+` marks that join.
                    let glyph = if from == to { '|' } else { bend(from, to) };
                    bottom[to] = Some(match (bottom[to], glyph) {
                        (Some((existing, _)), incoming)
                            if existing != incoming && (existing == '|' || incoming == '|') =>
                        {
                            ('+', segment.color)
                        }
                        _ => (glyph, segment.color),
                    });
                }
            }
        }
        middle[row.node_lane as usize] = Some(('*', row.node_color));

        write_cells(&mut out, &top);
        write_cells(&mut out, &middle);
        if let Some(label) = labels.get(index).filter(|label| !label.is_empty()) {
            out.push_str("  ");
            out.push_str(label);
        }
        out.push('\n');
        write_cells(&mut out, &bottom);
    }

    out
}

/// Every line of a row as segments, with pass-throughs split into Top+Bottom
/// pairs, in a fixed order. For renderers and tests that want one uniform list.
pub fn expanded_segments(row: GraphRow<'_>) -> Vec<Segment> {
    let mut segments: Vec<Segment> = row.edges.to_vec();
    for pass in row.passes {
        for half in [Half::Top, Half::Bottom] {
            segments.push(Segment {
                from_lane: pass.lane,
                to_lane: pass.lane,
                half,
                color: pass.color,
            });
        }
    }
    segments.sort_by_key(|segment| {
        (
            segment.half == Half::Bottom,
            segment.from_lane,
            segment.to_lane,
        )
    });
    segments
}

fn bend(from: usize, to: usize) -> char {
    if to < from { '/' } else { '\\' }
}

fn write_cells(out: &mut String, cells: &[Option<(char, u16)>]) {
    let mut line = String::with_capacity(cells.len() * 2);
    for cell in cells {
        match cell {
            Some((glyph, color)) => {
                line.push(*glyph);
                line.push(char::from_digit(u32::from(*color), 10).unwrap_or('?'));
            }
            None => line.push_str(".."),
        }
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

#[cfg(test)]
mod tests;
