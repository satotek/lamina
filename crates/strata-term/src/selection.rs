//! Buffer-coordinate selection model for terminal text and UI highlighting.

use std::ops::RangeInclusive;

use crate::snapshot::ScrollbarSnapshot;

/// A 0-indexed cell coordinate in full terminal buffer space (including scrollback).
///
/// Row `0` is the oldest retained scrollback row, and `row` increases downward
/// through scrollback into the active viewport (`0..scrollbar.total_rows`).
/// Field order (`row` then `col`) gives natural reading-order `Ord`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BufferPoint {
    pub row: usize,
    pub col: u16,
}

impl BufferPoint {
    pub const fn new(row: usize, col: u16) -> Self {
        Self { row, col }
    }

    /// Convert a 0-indexed visible viewport coordinate into full buffer coordinates
    /// using the snapshot's [`ScrollbarSnapshot`].
    pub fn from_viewport(scrollbar: &ScrollbarSnapshot, col: u16, viewport_row: u16) -> Self {
        let row = (scrollbar.viewport_offset as usize).saturating_add(usize::from(viewport_row));
        Self { row, col }
    }
}

/// Inclusive terminal selection in buffer coordinates.
///
/// Endpoints preserve drag direction (`start` may appear after `end` in reading
/// order). Use [`TerminalSelection::ordered`] when normalized bounds are needed.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct TerminalSelection {
    pub start: BufferPoint,
    pub end: BufferPoint,
    pub rectangle: bool,
}

impl TerminalSelection {
    /// Create a linear (flow-wrapped) selection from `start` to `end` inclusive.
    pub const fn new(start: BufferPoint, end: BufferPoint) -> Self {
        Self {
            start,
            end,
            rectangle: false,
        }
    }

    /// Create a rectangular block selection whose opposite corners are `start` and `end`.
    pub const fn rectangular(start: BufferPoint, end: BufferPoint) -> Self {
        Self {
            start,
            end,
            rectangle: true,
        }
    }

    /// Normalize endpoints so the first point is top-left (or earliest in reading
    /// order for linear selections) and the second is bottom-right (or latest).
    pub fn ordered(&self) -> (BufferPoint, BufferPoint) {
        if self.rectangle {
            let min_row = self.start.row.min(self.end.row);
            let max_row = self.start.row.max(self.end.row);
            let min_col = self.start.col.min(self.end.col);
            let max_col = self.start.col.max(self.end.col);
            (
                BufferPoint::new(min_row, min_col),
                BufferPoint::new(max_row, max_col),
            )
        } else if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    /// Returns `true` if `point` falls inside this selection.
    pub fn contains(&self, point: BufferPoint) -> bool {
        let (first, second) = self.ordered();
        if self.rectangle {
            (first.row..=second.row).contains(&point.row)
                && (first.col..=second.col).contains(&point.col)
        } else {
            point >= first && point <= second
        }
    }

    /// Returns the inclusive column range selected on `buffer_row` for a grid of
    /// width `cols`, or `None` if `buffer_row` is not covered by the selection.
    pub fn row_col_range(&self, buffer_row: usize, cols: u16) -> Option<RangeInclusive<u16>> {
        if cols == 0 {
            return None;
        }
        let last_col = cols - 1;
        let (first, second) = self.ordered();
        if buffer_row < first.row || buffer_row > second.row {
            return None;
        }

        if self.rectangle {
            let start_col = first.col.min(last_col);
            let end_col = second.col.min(last_col);
            return (start_col <= end_col).then_some(start_col..=end_col);
        }

        if first.row == second.row {
            let start_col = first.col.min(last_col);
            let end_col = second.col.min(last_col);
            (start_col <= end_col).then_some(start_col..=end_col)
        } else if buffer_row == first.row {
            Some(first.col.min(last_col)..=last_col)
        } else if buffer_row == second.row {
            Some(0..=second.col.min(last_col))
        } else {
            Some(0..=last_col)
        }
    }

    /// Convenience wrapper around [`TerminalSelection::row_col_range`] for a visible
    /// 0-indexed viewport row.
    pub fn viewport_row_col_range(
        &self,
        scrollbar: &ScrollbarSnapshot,
        viewport_row: u16,
        cols: u16,
    ) -> Option<RangeInclusive<u16>> {
        let buffer_row =
            (scrollbar.viewport_offset as usize).saturating_add(usize::from(viewport_row));
        self.row_col_range(buffer_row, cols)
    }
}
