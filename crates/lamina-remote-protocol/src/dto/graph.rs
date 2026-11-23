//! Graph rows travel with their commits so the app can draw a remote history
//! without rebuilding lane layout from parent ids.

use serde::{Deserialize, Serialize};

use crate::codec::ProtocolError;
use crate::dto::repo::oid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HalfDto {
    Top,
    Bottom,
}

impl From<lamina_core::graph::Half> for HalfDto {
    fn from(half: lamina_core::graph::Half) -> Self {
        match half {
            lamina_core::graph::Half::Top => Self::Top,
            lamina_core::graph::Half::Bottom => Self::Bottom,
        }
    }
}

impl From<HalfDto> for lamina_core::graph::Half {
    fn from(half: HalfDto) -> Self {
        match half {
            HalfDto::Top => Self::Top,
            HalfDto::Bottom => Self::Bottom,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentDto {
    pub from_lane: u16,
    pub to_lane: u16,
    pub half: HalfDto,
    pub color: u16,
}

impl From<&lamina_core::graph::Segment> for SegmentDto {
    fn from(segment: &lamina_core::graph::Segment) -> Self {
        Self {
            from_lane: segment.from_lane,
            to_lane: segment.to_lane,
            half: segment.half.into(),
            color: segment.color,
        }
    }
}

impl From<SegmentDto> for lamina_core::graph::Segment {
    fn from(segment: SegmentDto) -> Self {
        Self {
            from_lane: segment.from_lane,
            to_lane: segment.to_lane,
            half: segment.half.into(),
            color: segment.color,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassThroughDto {
    pub lane: u16,
    pub color: u16,
}

impl From<&lamina_core::graph::PassThrough> for PassThroughDto {
    fn from(pass: &lamina_core::graph::PassThrough) -> Self {
        Self {
            lane: pass.lane,
            color: pass.color,
        }
    }
}

impl From<PassThroughDto> for lamina_core::graph::PassThrough {
    fn from(pass: PassThroughDto) -> Self {
        Self {
            lane: pass.lane,
            color: pass.color,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphRowDto {
    pub node_lane: u16,
    pub node_color: u16,
    pub width: u16,
    pub passes: Vec<PassThroughDto>,
    pub edges: Vec<SegmentDto>,
}

impl From<lamina_core::graph::GraphRow<'_>> for GraphRowDto {
    fn from(row: lamina_core::graph::GraphRow<'_>) -> Self {
        Self {
            node_lane: row.node_lane,
            node_color: row.node_color,
            width: row.width,
            passes: row.passes.iter().map(PassThroughDto::from).collect(),
            edges: row.edges.iter().map(SegmentDto::from).collect(),
        }
    }
}

/// Rebuilds the flat row storage from wire rows, preserving order.
///
/// The store addresses one row's pass-throughs with a `u16` length and its
/// totals with `u32`s, and a push that overflows either panics inside
/// `GraphRows`. Wire rows are untrusted, so the counts are refused here: a
/// single row needs only a few hundred kilobytes on the wire to overflow its
/// `u16`.
pub fn rows_from_dto(rows: &[GraphRowDto]) -> Result<lamina_core::graph::GraphRows, ProtocolError> {
    let mut total_passes = 0usize;
    let mut total_edges = 0usize;
    for row in rows {
        if row.passes.len() > usize::from(u16::MAX) {
            return Err(ProtocolError::Malformed(
                "graph row has too many pass-throughs",
            ));
        }
        total_passes += row.passes.len();
        total_edges += row.edges.len();
    }
    if total_passes > u32::MAX as usize || total_edges > u32::MAX as usize {
        return Err(ProtocolError::Malformed("graph rows carry too many lines"));
    }
    let mut graph = lamina_core::graph::GraphRows::new();
    for row in rows {
        let passes: Vec<lamina_core::graph::PassThrough> =
            row.passes.iter().copied().map(Into::into).collect();
        let edges: Vec<lamina_core::graph::Segment> =
            row.edges.iter().copied().map(Into::into).collect();
        graph.push_row(row.node_lane, row.node_color, row.width, &passes, &edges);
    }
    Ok(graph)
}

/// A commit list sent in one frame, paired with its rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitChunkDto {
    pub generation: u64,
    pub commits: Vec<crate::dto::repo::CommitDto>,
    pub rows: Vec<GraphRowDto>,
    pub complete: bool,
}

/// Validates that a chunk keeps commits and rows aligned.
pub fn chunk_ids(chunk: &CommitChunkDto) -> Result<Vec<lamina_core::Oid>, ProtocolError> {
    if chunk.commits.len() != chunk.rows.len() {
        return Err(ProtocolError::Malformed(
            "snapshot chunk has a different number of commits and rows",
        ));
    }
    chunk.commits.iter().map(|commit| oid(commit.id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(passes: Vec<PassThroughDto>) -> GraphRowDto {
        GraphRowDto {
            node_lane: 0,
            node_color: 1,
            width: 1,
            passes,
            edges: Vec::new(),
        }
    }

    #[test]
    fn rows_rebuild_in_order() {
        let graph = rows_from_dto(&[
            GraphRowDto {
                edges: vec![SegmentDto {
                    from_lane: 0,
                    to_lane: 0,
                    half: HalfDto::Bottom,
                    color: 1,
                }],
                ..row(Vec::new())
            },
            row(Vec::new()),
        ])
        .expect("rows");
        assert_eq!(graph.len(), 2);
        assert_eq!(graph.get(0).expect("row").edges.len(), 1);
        assert!(graph.get(1).expect("row").edges.is_empty());
    }

    #[test]
    fn a_row_with_too_many_pass_throughs_is_refused() {
        // One row with more pass-throughs than the row layout's u16 can
        // address. Reaching the underlying panic needs only a few hundred
        // kilobytes of wire data, so this is a hostile frame, not a bug.
        let error = rows_from_dto(&[row(vec![
            PassThroughDto { lane: 0, color: 0 };
            usize::from(u16::MAX) + 1
        ])])
        .expect_err("too many pass-throughs must be refused");
        assert!(matches!(error, ProtocolError::Malformed(_)), "{error:?}");
    }

    #[test]
    fn a_row_at_the_pass_through_limit_still_rebuilds() {
        let graph = rows_from_dto(&[row(vec![
            PassThroughDto { lane: 1, color: 2 };
            usize::from(u16::MAX)
        ])])
        .expect("rows at the limit");
        assert_eq!(graph.len(), 1);
        assert_eq!(
            graph.get(0).expect("row").passes.len(),
            usize::from(u16::MAX)
        );
    }
}
