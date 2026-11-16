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

impl From<strata_core::graph::Half> for HalfDto {
    fn from(half: strata_core::graph::Half) -> Self {
        match half {
            strata_core::graph::Half::Top => Self::Top,
            strata_core::graph::Half::Bottom => Self::Bottom,
        }
    }
}

impl From<HalfDto> for strata_core::graph::Half {
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

impl From<&strata_core::graph::Segment> for SegmentDto {
    fn from(segment: &strata_core::graph::Segment) -> Self {
        Self {
            from_lane: segment.from_lane,
            to_lane: segment.to_lane,
            half: segment.half.into(),
            color: segment.color,
        }
    }
}

impl From<SegmentDto> for strata_core::graph::Segment {
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

impl From<&strata_core::graph::PassThrough> for PassThroughDto {
    fn from(pass: &strata_core::graph::PassThrough) -> Self {
        Self {
            lane: pass.lane,
            color: pass.color,
        }
    }
}

impl From<PassThroughDto> for strata_core::graph::PassThrough {
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

impl From<strata_core::graph::GraphRow<'_>> for GraphRowDto {
    fn from(row: strata_core::graph::GraphRow<'_>) -> Self {
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
pub fn rows_from_dto(rows: &[GraphRowDto]) -> strata_core::graph::GraphRows {
    let mut graph = strata_core::graph::GraphRows::new();
    for row in rows {
        let passes: Vec<strata_core::graph::PassThrough> =
            row.passes.iter().copied().map(Into::into).collect();
        let edges: Vec<strata_core::graph::Segment> =
            row.edges.iter().copied().map(Into::into).collect();
        graph.push_row(row.node_lane, row.node_color, row.width, &passes, &edges);
    }
    graph
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
pub fn chunk_ids(chunk: &CommitChunkDto) -> Result<Vec<strata_core::Oid>, ProtocolError> {
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

    #[test]
    fn rows_rebuild_in_order() {
        let graph = rows_from_dto(&[
            GraphRowDto {
                node_lane: 0,
                node_color: 1,
                width: 1,
                passes: Vec::new(),
                edges: vec![SegmentDto {
                    from_lane: 0,
                    to_lane: 0,
                    half: HalfDto::Bottom,
                    color: 1,
                }],
            },
            GraphRowDto {
                node_lane: 0,
                node_color: 1,
                width: 1,
                passes: Vec::new(),
                edges: Vec::new(),
            },
        ]);
        assert_eq!(graph.len(), 2);
        assert_eq!(graph.get(0).expect("row").edges.len(), 1);
        assert!(graph.get(1).expect("row").edges.is_empty());
    }
}
