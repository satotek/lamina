//! Commit graph lane layout. Owned by the graph task.
//!
//! Rows are produced incrementally: commits are pushed in display order
//! (topological, newest first) and each push yields one finished row.
//!
//! Geometry model used by the renderer: every row has a node at
//! `(node_lane, row middle)`. A `Segment` with `half == Top` runs from
//! `(from_lane, row top)` to `(to_lane, row middle)`; with `half == Bottom`
//! it runs from `(from_lane, row middle)` to `(to_lane, row bottom)`.
//! A lane that just passes through a row is a Top segment plus a Bottom
//! segment on the same lane.

use crate::Oid;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRow {
    pub node_lane: u16,
    pub node_color: u16,
    pub segments: Vec<Segment>,
    /// Number of lanes that must be reserved horizontally for this row.
    pub width: u16,
}

#[derive(Debug, Default)]
pub struct GraphLayout {
    rows: Vec<GraphRow>,
}

impl GraphLayout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append the next commit in display order and return its row.
    pub fn push(&mut self, _id: Oid, _parents: &[Oid]) -> &GraphRow {
        todo!("graph task")
    }

    pub fn rows(&self) -> &[GraphRow] {
        &self.rows
    }
}
