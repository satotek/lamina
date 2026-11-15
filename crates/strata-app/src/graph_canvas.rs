//! The commit-graph column.
//!
//! Each row paints its own [`GraphRow`] with the low-level canvas API: filled
//! quads for the straight lane lines, one stroked path per colour for the lane
//! changes, and a filled quad for the commit node. No text lives here, so the
//! row can be painted independently of the list's text layout.
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
    Bounds, ContentMask, Hsla, PathBuilder, Pixels, Point, Window, canvas, div, fill,
    linear_color_stop, linear_gradient, point, px, size,
};
use strata_core::graph::{GraphRow, Half, PassThrough, Segment};

use crate::style::LANES;

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
/// `boundary` draws a dashed stub under the node for a shallow clone's last
/// fetched commit, where the line to the parents was never fetched; `muted`
/// paints that stub. `merge_edge` is the dashed run a merge in progress adds on
/// the way down to `MERGE_HEAD`.
pub fn graph_column(
    row: GraphRow<'_>,
    boundary: bool,
    merge_edge: Option<MergeEdge>,
    colors: [Hsla; LANES],
    fade: Hsla,
    muted: Hsla,
) -> impl IntoElement {
    // A row above MERGE_HEAD's node may not have that lane yet, and the dashed
    // run would fall outside the row's indent and never be painted.
    let lanes = row_lanes(row).max(merge_edge.map_or(0, |edge| edge.lane.saturating_add(1)));
    let width = px(indent(lanes));
    // The paint closure must own its data; only visible rows are copied.
    let row = OwnedRow {
        node_lane: row.node_lane,
        node_color: row.node_color,
        lanes,
        passes: row.passes.to_vec(),
        edges: row.edges.to_vec(),
        boundary,
        merge_edge,
        muted,
    };

    div().w(width).h_full().flex_none().child(
        canvas(
            move |bounds, _window, _cx| bounds,
            move |bounds, _, window, _cx| paint_row(&row, &colors, fade, bounds, window),
        )
        .size_full(),
    )
}

/// Graph geometry for the synthetic uncommitted row at the top of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UncommittedGraph {
    pub head_lane: u16,
    pub head_color: u16,
    pub has_head_edge: bool,
    pub merge_target: Option<(u16, u16)>,
    /// Row of the `MERGE_HEAD` commit in the commit list, when the graph shows
    /// it. The target's lane alone cannot say how far down to draw.
    pub merge_target_index: Option<usize>,
    pub lanes: u16,
}

/// The dashed line a merge in progress draws on its way to `MERGE_HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeEdge {
    /// `MERGE_HEAD`'s node lane, which the edge follows down the list.
    pub lane: u16,
    /// `MERGE_HEAD`'s node colour.
    pub color: u16,
    /// This row holds the target node, so the edge stops at the node instead of
    /// running through the row.
    pub to_node: bool,
}

impl UncommittedGraph {
    /// The dashed run this merge edge contributes to commit row `index`.
    ///
    /// `None` once the target is behind us, or when the graph does not show the
    /// `MERGE_HEAD` commit at all (its branch is hidden), so nothing is drawn
    /// towards a node that is not on screen.
    pub fn merge_edge_at(&self, index: usize) -> Option<MergeEdge> {
        let (lane, color) = self.merge_target?;
        let target = self.merge_target_index?;
        (index <= target).then_some(MergeEdge {
            lane,
            color,
            to_node: index == target,
        })
    }
}

/// Graph column for the synthetic uncommitted row: a hollow dashed circle on
/// `HEAD`'s lane, a dashed edge down to `HEAD`, and an optional dashed curve to
/// `MERGE_HEAD`'s lane while merging.
pub fn uncommitted_graph_column(
    graph: UncommittedGraph,
    colors: [Hsla; LANES],
    fade: Hsla,
) -> impl IntoElement {
    let lanes = graph.lanes.max(1);
    let width = px(indent(lanes));
    div().w(width).h_full().flex_none().child(
        canvas(
            move |bounds, _window, _cx| bounds,
            move |bounds, _, window, _cx| {
                paint_uncommitted_row(graph, &colors, fade, bounds, window)
            },
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
    boundary: bool,
    merge_edge: Option<MergeEdge>,
    muted: Hsla,
}

/// A point in lane space: `x` is a distance from the row's left edge, `y` a
/// fraction of the row's height. Keeping the geometry free of pixels lets the
/// batching helpers be tested without a window.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pt {
    x: f32,
    y: f32,
}

/// A straight run of line, which a filled quad paints exactly.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Straight {
    /// Left edge to the line's centre.
    x: f32,
    /// Start and end of the run, as fractions of the row's height.
    from: f32,
    to: f32,
}

/// A lane change: a cubic Bézier, which only the tessellator can paint.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Curve {
    from: Pt,
    control_a: Pt,
    control_b: Pt,
    to: Pt,
}

/// One row's lines, as far as the paint pass is concerned.
///
/// Straight runs are kept apart from curves because a quad is all a straight
/// run needs; curves are sorted by palette slot so a row builds one path per
/// colour instead of one per segment.
#[derive(Default)]
struct Linework {
    straight: Vec<(usize, Straight)>,
    curves: Vec<(usize, Curve)>,
}

/// The palette slot a segment's colour index resolves to.
fn slot(color: u16) -> usize {
    usize::from(color) % LANES
}

/// Splits a row's linework into quads and curves, dropping what the cap hides.
///
/// `visible` is the number of lanes the row actually paints. An edge that
/// starts or ends inside the cap is kept whole: its far end runs under the
/// fade rather than stopping at the clip.
fn linework(row: &OwnedRow, visible: u16) -> Linework {
    // Every pass and edge contributes at most one run, so the buffers never
    // grow while they are filled.
    let capacity = row.passes.len() + row.edges.len();
    let mut straight = Vec::with_capacity(capacity);
    let mut curves = Vec::with_capacity(capacity);

    for pass in &row.passes {
        if pass.lane >= visible {
            continue;
        }
        straight.push((
            slot(pass.color),
            Straight {
                x: lane_offset(pass.lane),
                from: 0.0,
                to: 1.0,
            },
        ));
    }

    for segment in &row.edges {
        if segment.from_lane.min(segment.to_lane) >= visible {
            continue;
        }
        let color = slot(segment.color);
        let from_x = lane_offset(segment.from_lane);
        let to_x = lane_offset(segment.to_lane);
        let (from_y, to_y, control_y) = match segment.half {
            Half::Top => (0.0, 0.5, 0.25),
            Half::Bottom => (0.5, 1.0, 0.75),
        };

        if segment.from_lane == segment.to_lane {
            straight.push((
                color,
                Straight {
                    x: from_x,
                    from: from_y,
                    to: to_y,
                },
            ));
            continue;
        }

        // Both tangents are vertical, so the curve leaves and enters its lanes
        // straight down; the controls sit a quarter row in from each end.
        curves.push((
            color,
            Curve {
                from: Pt {
                    x: from_x,
                    y: from_y,
                },
                control_a: Pt {
                    x: from_x,
                    y: control_y,
                },
                control_b: Pt {
                    x: to_x,
                    y: control_y,
                },
                to: Pt { x: to_x, y: to_y },
            },
        ));
    }

    curves.sort_by_key(|(color, _)| *color);

    Linework { straight, curves }
}

fn paint_row(
    row: &OwnedRow,
    colors: &[Hsla; LANES],
    fade: Hsla,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    let top = bounds.origin.y;
    let height = bounds.size.height;
    let middle = top + height / 2.0;
    let left = bounds.origin.x;

    let clipped = row.lanes > MAX_LANES;
    let visible = row.lanes.min(MAX_LANES);
    // A clipped row paints inside the cap; its canvas is already only that
    // wide, so the mask also keeps lane changes from straddling the text.
    let clip = if clipped {
        Bounds::new(bounds.origin, size(px(clip_edge()), height))
    } else {
        bounds
    };

    let lines = linework(row, visible);

    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        // A straight run has the shape its stroke would have tessellated into,
        // so a quad paints the same pixels without touching the tessellator.
        for (color, run) in &lines.straight {
            let x = left + px(run.x) - px(STROKE_WIDTH / 2.0);
            window.paint_quad(fill(
                Bounds::new(
                    point(x, top + height * run.from),
                    size(px(STROKE_WIDTH), height * (run.to - run.from)),
                ),
                colors[*color],
            ));
        }

        // Curves are sorted by colour, so this builds at most one path per
        // palette slot per row. `PathBuilder` has no capacity to reserve; not
        // rebuilding one per segment is what keeps its buffers from growing.
        let mut paths: [Option<PathBuilder>; LANES] = std::array::from_fn(|_| None);
        for (color, curve) in &lines.curves {
            let path = paths[*color].get_or_insert_with(|| PathBuilder::stroke(px(STROKE_WIDTH)));
            path.move_to(lane_point(left, top, height, curve.from));
            path.cubic_bezier_to(
                lane_point(left, top, height, curve.to),
                lane_point(left, top, height, curve.control_a),
                lane_point(left, top, height, curve.control_b),
            );
        }
        for (color, path) in paths.into_iter().enumerate() {
            if let Some(path) = path
                && let Ok(path) = path.build()
            {
                window.paint_path(path, colors[color]);
            }
        }

        // The merge edge is dashed and painted over the lanes, because the
        // branch it points at may not have a line of its own this far up.
        if let Some(edge) = row.merge_edge
            && edge.lane < visible
        {
            let end = if edge.to_node {
                // Leave the node itself its solid outline.
                middle - px(NODE_RADIUS + 1.0)
            } else {
                top + height
            };
            paint_dashed_vertical(left, top, end, edge.lane, colors[slot(edge.color)], window);
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
    let node_x = if row.node_lane < visible {
        paint_node(
            bounds,
            middle,
            row.node_lane,
            colors[slot(row.node_color)],
            window,
        );
        Some(lane_offset(row.node_lane))
    } else if let Some(x) = node_marker(row.node_lane, row.lanes) {
        let color = colors[slot(row.node_color)];
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
        Some(x)
    } else {
        None
    };

    if row.boundary
        && let Some(x) = node_x
    {
        paint_boundary_stub(bounds, middle, x, row.muted, window);
    }
}

/// Three short dashes below a shallow boundary's node: the parents exist on
/// the remote, but their side of the line was not fetched.
fn paint_boundary_stub(
    bounds: Bounds<Pixels>,
    middle: Pixels,
    lane: f32,
    color: Hsla,
    window: &mut Window,
) {
    const DASH: f32 = 1.5;
    const GAP: f32 = 2.0;
    let x = bounds.origin.x + px(lane) - px(DASH / 2.0);
    let mut y = middle + px(NODE_RADIUS + 1.0);
    for _ in 0..3 {
        window.paint_quad(fill(
            Bounds::new(point(x, y), size(px(DASH), px(DASH))),
            color,
        ));
        y += px(DASH + GAP);
    }
}

/// Turns a lane-space point into a canvas coordinate.
fn lane_point(left: Pixels, top: Pixels, height: Pixels, at: Pt) -> Point<Pixels> {
    point(left + px(at.x), top + height * at.y)
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

fn cubic_bezier_pt(curve: Curve, t: f32) -> Pt {
    let u = 1.0 - t;
    let uu = u * u;
    let tt = t * t;
    let uuu = uu * u;
    let uut3 = 3.0 * uu * t;
    let utt3 = 3.0 * u * tt;
    let ttt = tt * t;
    Pt {
        x: uuu * curve.from.x
            + uut3 * curve.control_a.x
            + utt3 * curve.control_b.x
            + ttt * curve.to.x,
        y: uuu * curve.from.y
            + uut3 * curve.control_a.y
            + utt3 * curve.control_b.y
            + ttt * curve.to.y,
    }
}

/// Parametric dashed spans in `[0.0, 1.0]` for the bottom-half cubic curve,
/// leaving space around the node center at `t = 0`.
const CURVE_DASHES: [(f32, f32); 3] = [(0.18, 0.36), (0.50, 0.68), (0.82, 1.0)];

fn dashed_curve_segments(curve: Curve) -> Vec<Vec<Pt>> {
    const STEPS: usize = 6;
    CURVE_DASHES
        .iter()
        .map(|&(start, end)| {
            (0..=STEPS)
                .map(|step| {
                    let t = start + (end - start) * (step as f32 / STEPS as f32);
                    cubic_bezier_pt(curve, t)
                })
                .collect()
        })
        .collect()
}

fn dashed_circle_arcs(center_x: f32, center_y: f32, radius: f32) -> Vec<Vec<(f32, f32)>> {
    use std::f32::consts::TAU;
    const DASHES: usize = 6;
    const DUTY: f32 = 0.62;
    const STEPS: usize = 4;
    let period = TAU / DASHES as f32;
    (0..DASHES)
        .map(|index| {
            let start = index as f32 * period;
            let span = period * DUTY;
            (0..=STEPS)
                .map(|step| {
                    let angle = start + span * (step as f32 / STEPS as f32);
                    (
                        center_x + radius * angle.cos(),
                        center_y + radius * angle.sin(),
                    )
                })
                .collect()
        })
        .collect()
}

fn paint_uncommitted_row(
    graph: UncommittedGraph,
    colors: &[Hsla; LANES],
    fade: Hsla,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    let top = bounds.origin.y;
    let height = bounds.size.height;
    let middle = top + height / 2.0;
    let left = bounds.origin.x;

    let clipped = graph.lanes > MAX_LANES;
    let visible = graph.lanes.min(MAX_LANES);
    let clip = if clipped {
        Bounds::new(bounds.origin, size(px(clip_edge()), height))
    } else {
        bounds
    };

    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        if let Some((merge_lane, merge_color)) = graph.merge_target {
            if merge_lane != graph.head_lane && graph.head_lane.min(merge_lane) < visible {
                let head_x = lane_offset(graph.head_lane);
                let merge_x = lane_offset(merge_lane);
                let curve = Curve {
                    from: Pt { x: head_x, y: 0.5 },
                    control_a: Pt { x: head_x, y: 0.75 },
                    control_b: Pt {
                        x: merge_x,
                        y: 0.75,
                    },
                    to: Pt { x: merge_x, y: 1.0 },
                };
                let mut builder = PathBuilder::stroke(px(STROKE_WIDTH));
                for polyline in dashed_curve_segments(curve) {
                    if let Some((first, rest)) = polyline.split_first() {
                        builder.move_to(lane_point(left, top, height, *first));
                        for pt in rest {
                            builder.line_to(lane_point(left, top, height, *pt));
                        }
                    }
                }
                if let Ok(path) = builder.build() {
                    window.paint_path(path, colors[slot(merge_color)]);
                }
            } else if !graph.has_head_edge && merge_lane < visible {
                paint_dashed_vertical(
                    left,
                    middle + px(NODE_RADIUS + 1.0),
                    top + height,
                    merge_lane,
                    colors[slot(merge_color)],
                    window,
                );
            }
        }

        if graph.has_head_edge && graph.head_lane < visible {
            paint_dashed_vertical(
                left,
                middle + px(NODE_RADIUS + 1.0),
                top + height,
                graph.head_lane,
                colors[slot(graph.head_color)],
                window,
            );
        }
    });

    if clipped {
        let edge = bounds.origin.x + px(clip_edge());
        window.paint_quad(fill(
            Bounds::new(
                point(edge - px(FADE_WIDTH), top),
                size(px(FADE_WIDTH), height),
            ),
            linear_gradient(
                90.0,
                linear_color_stop(fade.opacity(0.0), 0.0),
                linear_color_stop(fade, 1.0),
            ),
        ));
    }

    let head_color = colors[slot(graph.head_color)];
    if graph.head_lane < visible {
        let center_x = lane_offset(graph.head_lane);
        let diameter = NODE_RADIUS * 2.0;
        // Hollow interior masks the top of any downward edge under the ring.
        window.paint_quad(
            fill(
                Bounds::new(
                    point(left + px(center_x - NODE_RADIUS), middle - px(NODE_RADIUS)),
                    size(px(diameter), px(diameter)),
                ),
                fade,
            )
            .corner_radii(px(NODE_RADIUS)),
        );
        let mut builder = PathBuilder::stroke(px(STROKE_WIDTH));
        for arc in dashed_circle_arcs(center_x, 0.0, NODE_RADIUS - STROKE_WIDTH * 0.5) {
            if let Some((first, rest)) = arc.split_first() {
                builder.move_to(point(left + px(first.0), middle + px(first.1)));
                for pt in rest {
                    builder.line_to(point(left + px(pt.0), middle + px(pt.1)));
                }
            }
        }
        if let Ok(path) = builder.build() {
            window.paint_path(path, head_color);
        }
    } else if let Some(x) = node_marker(graph.head_lane, graph.lanes) {
        let diameter = MARKER_RADIUS * 2.0;
        window.paint_quad(
            fill(
                Bounds::new(
                    point(left + px(x - MARKER_RADIUS), middle - px(MARKER_RADIUS)),
                    size(px(diameter), px(diameter)),
                ),
                fade,
            )
            .corner_radii(px(MARKER_RADIUS)),
        );
        let mut builder = PathBuilder::stroke(px(STROKE_WIDTH));
        for arc in dashed_circle_arcs(x, 0.0, MARKER_RADIUS - STROKE_WIDTH * 0.5) {
            if let Some((first, rest)) = arc.split_first() {
                builder.move_to(point(left + px(first.0), middle + px(first.1)));
                for pt in rest {
                    builder.line_to(point(left + px(pt.0), middle + px(pt.1)));
                }
            }
        }
        if let Ok(path) = builder.build() {
            window.paint_path(path, head_color);
        }
    }
}

fn paint_dashed_vertical(
    left: Pixels,
    from: Pixels,
    to: Pixels,
    lane: u16,
    color: Hsla,
    window: &mut Window,
) {
    const DASH: f32 = 2.5;
    const GAP: f32 = 2.0;
    let x = left + px(lane_offset(lane)) - px(STROKE_WIDTH / 2.0);
    let mut y = from;
    while y < to {
        let dash_height = px(DASH).min(to - y);
        window.paint_quad(fill(
            Bounds::new(point(x, y), size(px(STROKE_WIDTH), dash_height)),
            color,
        ));
        y += px(DASH + GAP);
    }
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

    fn owned(node_lane: u16, lanes: u16, passes: &[PassThrough], edges: &[Segment]) -> OwnedRow {
        OwnedRow {
            node_lane,
            node_color: 0,
            lanes,
            passes: passes.to_vec(),
            edges: edges.to_vec(),
            boundary: false,
            merge_edge: None,
            muted: Hsla::default(),
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

    #[test]
    fn passes_become_full_height_straight_runs() {
        let passes = [
            PassThrough { lane: 0, color: 2 },
            // A colour index past the table wraps to the same slot a paint
            // would have resolved it to.
            PassThrough {
                lane: 2,
                color: LANES as u16 + 2,
            },
        ];
        let lines = linework(&owned(0, 3, &passes, &[]), 3);

        assert!(lines.curves.is_empty());
        assert_eq!(
            lines.straight,
            vec![
                (
                    2,
                    Straight {
                        x: lane_offset(0),
                        from: 0.0,
                        to: 1.0,
                    },
                ),
                (
                    2,
                    Straight {
                        x: lane_offset(2),
                        from: 0.0,
                        to: 1.0,
                    },
                ),
            ],
            "one quad per pass, full row height, at the lane's centre"
        );
    }

    #[test]
    fn straight_edges_become_quads_and_lane_changes_become_curves() {
        let edges = [
            Segment {
                from_lane: 1,
                to_lane: 1,
                half: Half::Top,
                color: 0,
            },
            Segment {
                from_lane: 3,
                to_lane: 3,
                half: Half::Bottom,
                color: 1,
            },
            Segment {
                from_lane: 0,
                to_lane: 2,
                half: Half::Top,
                color: 1,
            },
            Segment {
                from_lane: 4,
                to_lane: 1,
                half: Half::Bottom,
                color: 0,
            },
        ];
        let lines = linework(&owned(1, 5, &[], &edges), 5);

        assert_eq!(
            lines.straight,
            vec![
                (
                    0,
                    Straight {
                        x: lane_offset(1),
                        from: 0.0,
                        to: 0.5,
                    },
                ),
                (
                    1,
                    Straight {
                        x: lane_offset(3),
                        from: 0.5,
                        to: 1.0,
                    },
                ),
            ],
            "edges that stay in their lane are straight quads for their half"
        );
        assert_eq!(
            lines.curves,
            vec![
                (
                    0,
                    Curve {
                        from: Pt {
                            x: lane_offset(4),
                            y: 0.5,
                        },
                        control_a: Pt {
                            x: lane_offset(4),
                            y: 0.75,
                        },
                        control_b: Pt {
                            x: lane_offset(1),
                            y: 0.75,
                        },
                        to: Pt {
                            x: lane_offset(1),
                            y: 1.0,
                        },
                    },
                ),
                (
                    1,
                    Curve {
                        from: Pt {
                            x: lane_offset(0),
                            y: 0.0,
                        },
                        control_a: Pt {
                            x: lane_offset(0),
                            y: 0.25,
                        },
                        control_b: Pt {
                            x: lane_offset(2),
                            y: 0.25,
                        },
                        to: Pt {
                            x: lane_offset(2),
                            y: 0.5,
                        },
                    },
                ),
            ],
            "lane changes keep their vertical tangents, grouped by colour"
        );
    }

    #[test]
    fn curves_are_grouped_so_a_row_builds_one_path_per_colour() {
        // Colours alternate so an unsorted batch would open a new path per
        // segment instead of per colour.
        let edges: Vec<Segment> = (0..6)
            .map(|lane| Segment {
                from_lane: lane,
                to_lane: lane + 1,
                half: Half::Top,
                color: lane % 3,
            })
            .collect();
        let lines = linework(&owned(0, 7, &[], &edges), 7);

        let colours: Vec<usize> = lines.curves.iter().map(|(color, _)| *color).collect();
        assert_eq!(colours, vec![0, 0, 1, 1, 2, 2]);
        let mut runs = colours.clone();
        runs.dedup();
        assert_eq!(runs.len(), 3, "one run per colour, not per segment");
    }

    #[test]
    fn the_cap_drops_hidden_lanes_but_keeps_edges_that_cross_it() {
        let passes = [
            PassThrough {
                lane: MAX_LANES - 1,
                color: 0,
            },
            PassThrough {
                lane: MAX_LANES,
                color: 1,
            },
        ];
        let edges = [
            // Entirely past the cap: nothing of it is visible.
            Segment {
                from_lane: MAX_LANES,
                to_lane: MAX_LANES + 3,
                half: Half::Top,
                color: 0,
            },
            // Leaves the cap from a visible lane: its visible part runs under
            // the fade.
            Segment {
                from_lane: MAX_LANES - 1,
                to_lane: 40,
                half: Half::Bottom,
                color: 2,
            },
            // Enters from a hidden lane: same.
            Segment {
                from_lane: 30,
                to_lane: 0,
                half: Half::Top,
                color: 3,
            },
        ];
        let lines = linework(&owned(0, 40, &passes, &edges), MAX_LANES);

        assert_eq!(
            lines.straight.len(),
            1,
            "the only visible pass is the one inside the cap"
        );
        assert_eq!(lines.straight[0].1.x, lane_offset(MAX_LANES - 1));
        assert_eq!(lines.curves.len(), 2, "crossing edges are painted");
        assert_eq!(lines.curves[0].0, 2);
        assert_eq!(lines.curves[1].0, 3);
    }

    #[test]
    fn uncommitted_hollow_node_builds_six_dashed_arcs_around_node_radius() {
        let arcs = dashed_circle_arcs(lane_offset(1), 0.0, NODE_RADIUS - STROKE_WIDTH * 0.5);
        assert_eq!(arcs.len(), 6);
        let expected_r = NODE_RADIUS - STROKE_WIDTH * 0.5;
        for arc in &arcs {
            assert!(arc.len() >= 2);
            for &(x, y) in arc {
                let dx = x - lane_offset(1);
                let dist = (dx * dx + y * y).sqrt();
                assert!((dist - expected_r).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn uncommitted_merge_curve_splits_into_three_bottom_half_dashes() {
        let curve = Curve {
            from: Pt {
                x: lane_offset(0),
                y: 0.5,
            },
            control_a: Pt {
                x: lane_offset(0),
                y: 0.75,
            },
            control_b: Pt {
                x: lane_offset(2),
                y: 0.75,
            },
            to: Pt {
                x: lane_offset(2),
                y: 1.0,
            },
        };
        let dashes = dashed_curve_segments(curve);
        assert_eq!(dashes.len(), 3);
        let first_pt = dashes.first().unwrap().first().unwrap();
        let last_pt = dashes.last().unwrap().last().unwrap();
        assert!(
            first_pt.y > 0.5,
            "curve starts below the hollow circle center"
        );
        assert!((last_pt.x - lane_offset(2)).abs() < 1e-4);
        assert!((last_pt.y - 1.0).abs() < 1e-4);
    }

    #[test]
    fn the_merge_edge_passes_through_every_row_up_to_the_target() {
        let graph = UncommittedGraph {
            head_lane: 0,
            head_color: 0,
            has_head_edge: true,
            merge_target: Some((2, 5)),
            merge_target_index: Some(3),
            lanes: 3,
        };
        for index in 0..3 {
            assert_eq!(
                graph.merge_edge_at(index),
                Some(MergeEdge {
                    lane: 2,
                    color: 5,
                    to_node: false,
                }),
                "row {index} is between the uncommitted row and MERGE_HEAD"
            );
        }
        assert_eq!(
            graph.merge_edge_at(3),
            Some(MergeEdge {
                lane: 2,
                color: 5,
                to_node: true,
            }),
            "the target row ends the edge at its node"
        );
        assert_eq!(graph.merge_edge_at(4), None, "nothing is drawn below it");
    }

    #[test]
    fn a_hidden_merge_head_leaves_no_dangling_edge() {
        let graph = UncommittedGraph {
            head_lane: 0,
            head_color: 0,
            has_head_edge: true,
            merge_target: Some((2, 5)),
            merge_target_index: None,
            lanes: 3,
        };
        assert_eq!(graph.merge_edge_at(0), None);
        assert_eq!(
            UncommittedGraph {
                merge_target_index: Some(1),
                merge_target: None,
                ..graph
            }
            .merge_edge_at(0),
            None,
            "a lane without a target row is not drawn either"
        );
    }
}
