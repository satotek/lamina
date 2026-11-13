//! The commit-graph column.
//!
//! Each row paints its own [`GraphRow`] with the low-level canvas API: stroked
//! paths for lane lines and lane changes, a filled quad for the commit node.
//! No text lives here, so the row can be painted independently of the list's
//! text layout.

use gpui::prelude::*;
use gpui::{Bounds, Hsla, PathBuilder, Pixels, Window, canvas, div, fill, point, px, size};
use strata_core::graph::{GraphRow, Half};

pub const ROW_HEIGHT: f32 = 22.0;
pub const LANE_WIDTH: f32 = 12.0;
const STROKE_WIDTH: f32 = 1.5;
const NODE_RADIUS: f32 = 3.5;

/// `lanes` is the width of the widest row in the list, not of this row: the
/// column must not resize per row or lanes would drift horizontally.
pub fn graph_column(row: &GraphRow, colors: [Hsla; 8], lanes: u16) -> impl IntoElement {
    let width = px(LANE_WIDTH * f32::from(lanes.max(1)));
    let row = row.clone();

    div().w(width).h_full().flex_none().child(
        canvas(
            move |bounds, _window, _cx| bounds,
            move |bounds, _, window, _cx| paint_row(&row, &colors, bounds, window),
        )
        .size_full(),
    )
}

fn paint_row(row: &GraphRow, colors: &[Hsla; 8], bounds: Bounds<Pixels>, window: &mut Window) {
    let top = bounds.origin.y;
    let height = bounds.size.height;
    let middle = top + height / 2.0;
    let bottom = top + height;
    let quarter = height / 4.0;

    for segment in &row.segments {
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
    bounds.origin.x + px((f32::from(lane) + 0.5) * LANE_WIDTH)
}
