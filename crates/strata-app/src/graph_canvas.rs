//! The commit-graph column.
//!
//! Each row paints its own [`GraphRow`] with the low-level canvas API: stroked
//! paths for lane lines and lane changes, a filled quad for the commit node.
//! No text lives here, so the row can be painted independently of the list's
//! text layout.

use gpui::prelude::*;
use gpui::{Bounds, Hsla, PathBuilder, Pixels, Window, canvas, div, fill, point, px, size};
use strata_core::graph::{GraphRow, Half, PassThrough, Segment};

pub const ROW_HEIGHT: f32 = 28.0;
pub const LANE_WIDTH: f32 = 14.0;
/// Distance from the column's left edge to the first lane's centre.
const LANE_PADDING: f32 = 14.0;
/// The graph column never gets narrower than this, so the "Graph" heading and
/// the first lane keep their places even for a linear history.
pub const MIN_WIDTH: f32 = 72.0;
const STROKE_WIDTH: f32 = 2.0;
const NODE_RADIUS: f32 = 5.0;

/// Width of the graph column for a list whose widest row has `lanes` lanes.
pub fn column_width(lanes: u16) -> f32 {
    let lanes = f32::from(lanes.max(1));
    (LANE_PADDING * 2.0 + (lanes - 1.0) * LANE_WIDTH).max(MIN_WIDTH)
}

/// `lanes` is the width of the widest row in the list, not of this row: the
/// column must not resize per row or lanes would drift horizontally.
pub fn graph_column(row: GraphRow<'_>, colors: [Hsla; 8], lanes: u16) -> impl IntoElement {
    let width = px(column_width(lanes));
    // The paint closure must own its data; only visible rows are copied.
    let row = OwnedRow {
        node_lane: row.node_lane,
        node_color: row.node_color,
        passes: row.passes.to_vec(),
        edges: row.edges.to_vec(),
    };

    div().w(width).h_full().flex_none().child(
        canvas(
            move |bounds, _window, _cx| bounds,
            move |bounds, _, window, _cx| paint_row(&row, &colors, bounds, window),
        )
        .size_full(),
    )
}

struct OwnedRow {
    node_lane: u16,
    node_color: u16,
    passes: Vec<PassThrough>,
    edges: Vec<Segment>,
}

fn paint_row(row: &OwnedRow, colors: &[Hsla; 8], bounds: Bounds<Pixels>, window: &mut Window) {
    let top = bounds.origin.y;
    let height = bounds.size.height;
    let middle = top + height / 2.0;
    let bottom = top + height;
    let quarter = height / 4.0;

    for pass in &row.passes {
        let color = colors[usize::from(pass.color) % colors.len()];
        let x = lane_x(bounds, pass.lane);
        let mut path = PathBuilder::stroke(px(STROKE_WIDTH));
        path.move_to(point(x, top));
        path.line_to(point(x, bottom));
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }

    for segment in &row.edges {
        let color = colors[usize::from(segment.color) % colors.len()];
        let from_x = lane_x(bounds, segment.from_lane);
        let to_x = lane_x(bounds, segment.to_lane);
        let mut path = PathBuilder::stroke(px(STROKE_WIDTH));

        match segment.half {
            Half::Top => {
                path.move_to(point(from_x, top));
                if segment.from_lane == segment.to_lane {
                    path.line_to(point(to_x, middle));
                } else {
                    path.cubic_bezier_to(
                        point(to_x, middle),
                        point(from_x, middle - quarter),
                        point(to_x, middle - quarter),
                    );
                }
            }
            Half::Bottom => {
                path.move_to(point(from_x, middle));
                if segment.from_lane == segment.to_lane {
                    path.line_to(point(to_x, bottom));
                } else {
                    path.cubic_bezier_to(
                        point(to_x, bottom),
                        point(from_x, middle + quarter),
                        point(to_x, bottom - quarter),
                    );
                }
            }
        }

        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }

    let center = point(lane_x(bounds, row.node_lane), middle);
    let diameter = NODE_RADIUS * 2.0;
    window.paint_quad(
        fill(
            Bounds::new(
                point(center.x - px(NODE_RADIUS), center.y - px(NODE_RADIUS)),
                size(px(diameter), px(diameter)),
            ),
            colors[usize::from(row.node_color) % colors.len()],
        )
        .corner_radii(px(NODE_RADIUS)),
    );
}

fn lane_x(bounds: Bounds<Pixels>, lane: u16) -> Pixels {
    bounds.origin.x + px(LANE_PADDING + f32::from(lane) * LANE_WIDTH)
}
