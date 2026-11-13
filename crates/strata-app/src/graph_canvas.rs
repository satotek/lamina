//! The commit-graph column.
//!
//! Each row paints its own [`GraphRow`] with the low-level canvas API: stroked
//! paths for lane lines and lane changes, a filled quad for the commit node.
//! No text lives here, so the row can be painted independently of the list's
//! text layout.
//!
//! Lane x positions are global: lane `i` sits at the same distance from the
//! canvas' left edge on every row, so lines run straight down the list. The
//! canvas itself is as wide as the row needs, and the row's text starts after
//! it, so a row with two lanes does not pay for the widest row in history.
//!
//! Rows wider than [`MAX_LANES`] reserve no more than the cap: their extra
//! lanes are clipped, a gradient fades them out, and a node beyond the cap
//! becomes a small marker on the edge so the row still shows its commit.

use gpui::prelude::*;
use gpui::{
    Bounds, ContentMask, Hsla, PathBuilder, Pixels, Window, canvas, div, fill, linear_color_stop,
    linear_gradient, point, px, size,
};
use strata_core::graph::{GraphRow, Half, PassThrough, Segment};

pub const ROW_HEIGHT: f32 = 28.0;
/// Distance between two lane centres.
pub const LANE_WIDTH: f32 = 12.0;
/// Distance from the canvas' left edge to the first lane's centre.
const LANE_PADDING: f32 = 8.0;
const STROKE_WIDTH: f32 = 1.5;
const NODE_RADIUS: f32 = 4.5;
/// Radius of the marker that stands in for a node past the cap.
const MARKER_RADIUS: f32 = 3.0;
/// Lanes a row may reserve before the graph is clipped. The cap keeps the text
/// column readable on histories with dozens of parallel branches.
pub const MAX_LANES: u16 = 12;
/// Width of the fade that erases clipped lane ends at a row's right edge.
const FADE_WIDTH: f32 = 4.0;
/// Room between the last visible lane (or the fade) and the row's text.
const TEXT_GAP: f32 = 9.0;

/// Distance of a lane's centre from the canvas' left edge.
///
/// This takes no row width on purpose: lanes must not move horizontally when
/// the row above or below is wider.
pub fn lane_offset(lane: u16) -> f32 {
    LANE_PADDING + f32::from(lane) * LANE_WIDTH
}

/// Width of the graph part of a row that reserves `lanes` lanes.
///
/// Zero lanes means an empty row: it gets the same indent as a single-lane row,
/// which keeps the text aligned for rows whose graph ends up empty. Rows wider
/// than [`MAX_LANES`] stop growing at the cap.
pub fn indent(lanes: u16) -> f32 {
    let lanes = f32::from(lanes.clamp(1, MAX_LANES));
    LANE_PADDING + (lanes - 1.0) * LANE_WIDTH + NODE_RADIUS + TEXT_GAP
}

/// Indent for a row without a graph, such as a search result, so its text does
/// not jump to the left edge of the list.
pub fn search_indent() -> f32 {
    indent(1)
}

/// Where a clipped row's lanes stop and the fade begins.
fn clip_edge() -> f32 {
    lane_offset(MAX_LANES - 1) + NODE_RADIUS
}

/// The marker that replaces a node beyond the cap, as an x offset, or `None`
/// when the node is inside the row's visible lanes.
pub fn node_marker(lane: u16, lanes: u16) -> Option<f32> {
    let visible = lanes.clamp(1, MAX_LANES);
    (lane >= visible).then_some(clip_edge())
}

/// Lanes the row paints: its layout width, widened if a line reaches further.
///
/// [`GraphRow::width`] already covers passes, edges and the node for rows from
/// `strata-core`; hand-built rows (the synthetic benchmark) may leave a gap, so
/// the painted geometry is the final word.
pub fn row_lanes(row: GraphRow<'_>) -> u16 {
    let painted = row
        .passes
        .iter()
        .map(|pass| pass.lane)
        .chain(
            row.edges
                .iter()
                .flat_map(|edge| [edge.from_lane, edge.to_lane]),
        )
        .chain([row.node_lane])
        .max()
        .map_or(0, |lane| lane.saturating_add(1));
    row.width.max(painted).max(1)
}

/// A row's graph, sized to that row's own indent.
///
/// `fade` is the colour behind the row, used by the gradient at a clipped edge.
pub fn graph_column(row: GraphRow<'_>, colors: [Hsla; 8], fade: Hsla) -> impl IntoElement {
    let lanes = row_lanes(row);
    let width = px(indent(lanes));
    // The paint closure must own its data; only visible rows are copied.
    let row = OwnedRow {
        node_lane: row.node_lane,
        node_color: row.node_color,
        lanes,
        passes: row.passes.to_vec(),
        edges: row.edges.to_vec(),
    };

    div().w(width).h_full().flex_none().child(
        canvas(
            move |bounds, _window, _cx| bounds,
            move |bounds, _, window, _cx| paint_row(&row, &colors, fade, bounds, window),
        )
        .size_full(),
    )
}

struct OwnedRow {
    node_lane: u16,
    node_color: u16,
    lanes: u16,
    passes: Vec<PassThrough>,
    edges: Vec<Segment>,
}

fn paint_row(
    row: &OwnedRow,
    colors: &[Hsla; 8],
    fade: Hsla,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    let top = bounds.origin.y;
    let height = bounds.size.height;
    let middle = top + height / 2.0;
    let bottom = top + height;
    let quarter = height / 4.0;

    let clipped = row.lanes > MAX_LANES;
    let visible = row.lanes.min(MAX_LANES);
    // A clipped row paints inside the cap; its canvas is already only that
    // wide, so the mask also keeps lane changes from straddling the text.
    let clip = if clipped {
        Bounds::new(bounds.origin, size(px(clip_edge()), height))
    } else {
        bounds
    };

    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        for pass in &row.passes {
            if pass.lane >= visible {
                continue;
            }
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
            // An edge entirely past the cap has nothing to contribute once
            // clipped; one that crosses it still enters from the fade.
            if segment.from_lane.min(segment.to_lane) >= visible {
                continue;
            }
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
    });

    if clipped {
        let edge = bounds.origin.x + px(clip_edge());
        // Fading only the last four pixels keeps the cap's final lane crisp
        // while dissolving clipped lane ends into the row's fill.
        window.paint_quad(fill(
            Bounds::new(
                point(edge - px(FADE_WIDTH), top),
                size(px(FADE_WIDTH), height),
            ),
            linear_gradient(
                90.0,
                // Both stops must share the colour: interpolating towards a
                // transparent black would wash the fade grey.
                linear_color_stop(fade.opacity(0.0), 0.0),
                linear_color_stop(fade, 1.0),
            ),
        ));
    }

    // The node goes on top of the fade: only the lines are clipped, and a
    // commit dot on the edge lane must stay crisp.
    if row.node_lane < visible {
        paint_node(
            bounds,
            middle,
            row.node_lane,
            colors[usize::from(row.node_color) % colors.len()],
            window,
        );
    } else if let Some(x) = node_marker(row.node_lane, row.lanes) {
        let color = colors[usize::from(row.node_color) % colors.len()];
        let diameter = MARKER_RADIUS * 2.0;
        window.paint_quad(
            fill(
                Bounds::new(
                    point(
                        bounds.origin.x + px(x) - px(MARKER_RADIUS),
                        middle - px(MARKER_RADIUS),
                    ),
                    size(px(diameter), px(diameter)),
                ),
                color,
            )
            .corner_radii(px(MARKER_RADIUS)),
        );
    }
}

fn paint_node(bounds: Bounds<Pixels>, middle: Pixels, lane: u16, color: Hsla, window: &mut Window) {
    let diameter = NODE_RADIUS * 2.0;
    window.paint_quad(
        fill(
            Bounds::new(
                point(
                    lane_x(bounds, lane) - px(NODE_RADIUS),
                    middle - px(NODE_RADIUS),
                ),
                size(px(diameter), px(diameter)),
            ),
            color,
        )
        .corner_radii(px(NODE_RADIUS)),
    );
}

fn lane_x(bounds: Bounds<Pixels>, lane: u16) -> Pixels {
    bounds.origin.x + px(lane_offset(lane))
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::graph::{Half, Segment};

    fn row<'a>(
        node_lane: u16,
        width: u16,
        passes: &'a [PassThrough],
        edges: &'a [Segment],
    ) -> GraphRow<'a> {
        GraphRow {
            node_lane,
            node_color: 0,
            width,
            passes,
            edges,
        }
    }

    #[test]
    fn lane_positions_do_not_depend_on_the_row_width() {
        assert_eq!(lane_offset(0), LANE_PADDING);
        assert_eq!(lane_offset(3), LANE_PADDING + 3.0 * LANE_WIDTH);
        assert_eq!(lane_offset(3) - lane_offset(2), LANE_WIDTH);

        // Same lane, rows of very different widths: the offset is the same.
        let narrow = row(1, 2, &[], &[]);
        let wide = row(1, 40, &[], &[]);
        assert_eq!(lane_offset(narrow.node_lane), lane_offset(wide.node_lane));
    }

    #[test]
    fn indent_grows_by_lane_until_the_cap() {
        assert_eq!(indent(0), indent(1), "an empty row keeps one lane's indent");
        assert_eq!(indent(5), indent(1) + 4.0 * LANE_WIDTH);
        assert!(
            indent(MAX_LANES) == indent(MAX_LANES + 1) && indent(MAX_LANES) == indent(500),
            "the cap is the widest indent a row may reserve"
        );
        assert!(
            indent(MAX_LANES) < 160.0,
            "the cap must leave room for text in a narrow window"
        );
    }

    #[test]
    fn nodes_past_the_cap_become_a_marker_at_the_edge() {
        assert_eq!(node_marker(4, 5), None, "inside a narrow row");
        assert_eq!(node_marker(11, 12), None, "the last lane before the cap");
        assert_eq!(node_marker(2, 40), None, "a visible node in a wide row");
        assert_eq!(node_marker(0, 0), None, "an empty row still shows lane 0");

        let marker = node_marker(12, 13).expect("first lane past the cap");
        assert_eq!(marker, clip_edge());
        assert_eq!(node_marker(30, 31), Some(marker));
        assert_eq!(node_marker(0, 40), None);
    }

    #[test]
    fn row_lanes_cover_geometry_the_layout_did_not_reserve() {
        let pass = PassThrough { lane: 6, color: 0 };
        assert_eq!(
            row_lanes(row(0, 1, &[pass], &[])),
            7,
            "a pass-through widens a hand-built row"
        );

        let edge = Segment {
            from_lane: 2,
            to_lane: 9,
            half: Half::Bottom,
            color: 0,
        };
        assert_eq!(row_lanes(row(1, 1, &[], &[edge])), 10);
        assert_eq!(
            row_lanes(row(3, 0, &[], &[])),
            4,
            "width zero uses the node"
        );
        assert_eq!(row_lanes(row(0, 0, &[], &[])), 1, "never zero lanes");
    }
}
