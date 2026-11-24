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
//! A dirty working tree draws a hollow circle above HEAD. That connector is
//! not part of the core layout: dirtiness flips on every edit, and pushing a
//! synthetic node through the layout would rebuild the whole history. When
//! HEAD's own lane is not empty all the way up, every row shifts right by one
//! (or two, while a merge also needs a private column) and the dashes occupy
//! the inserted lane. The shift is the same on every row, including rows below
//! HEAD, so a line that continues past HEAD does not kink.
//!
//! Rows wider than [`MAX_LANES`] reserve no more than the cap: their extra
//! lanes are clipped, a gradient fades them out, and a node beyond the cap
//! becomes a small marker on the edge so the row still shows its commit.

use gpui::prelude::*;
use gpui::{
    Bounds, ContentMask, Hsla, PathBuilder, Pixels, Point, Window, canvas, div, fill,
    linear_color_stop, linear_gradient, point, px, size,
};
use lamina_core::graph::{GraphRow, Half, PassThrough, Segment};

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

/// [`lane_offset`] scaled by `zoom`.
pub fn lane_offset_at(lane: u16, zoom: f32) -> f32 {
    lane_offset(lane) * zoom
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

/// [`indent`] scaled by `zoom`.
pub fn indent_at(lanes: u16, zoom: f32) -> f32 {
    indent(lanes) * zoom
}

/// Indent for a row without a graph, such as a search result, so its text does
/// not jump to the left edge of the list.
#[cfg_attr(not(test), allow(dead_code))]
pub fn search_indent() -> f32 {
    indent(1)
}

/// [`search_indent`] scaled by `zoom`.
pub fn search_indent_at(zoom: f32) -> f32 {
    indent_at(1, zoom)
}

/// Where a clipped row's lanes stop and the fade begins.
fn clip_edge() -> f32 {
    lane_offset(MAX_LANES - 1) + NODE_RADIUS
}

fn clip_edge_at(zoom: f32) -> f32 {
    clip_edge() * zoom
}

/// The marker that replaces a node beyond the cap, as an x offset, or `None`
/// when the node is inside the row's visible lanes.
pub fn node_marker(lane: u16, lanes: u16) -> Option<f32> {
    let visible = lanes.clamp(1, MAX_LANES);
    (lane >= visible).then_some(clip_edge())
}

fn node_marker_at(lane: u16, lanes: u16, zoom: f32) -> Option<f32> {
    node_marker(lane, lanes).map(|x| x * zoom)
}

/// Lanes the row paints: its layout width, widened if a line reaches further.
///
/// [`GraphRow::width`] already covers passes, edges and the node for rows from
/// `lamina-core`; hand-built rows (the synthetic benchmark) may leave a gap, so
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

/// Lanes a commit row reserves once `overlay` has shifted the core layout.
///
/// Every row grows by the same shift, so lane centres stay aligned down the
/// list. A dashed run that reaches further than the shifted row still widens
/// the rows it is painted on; otherwise the line would fall past the indent
/// and the summary would cover it.
pub fn column_lanes(row: GraphRow<'_>, overlay: GraphOverlay) -> u16 {
    let mut lanes = row_lanes(row).saturating_add(overlay.shift);
    if let Some(edge) = overlay.worktree {
        lanes = lanes
            .max(edge.lane.saturating_add(1))
            .max(edge.node_lane.saturating_add(1));
    }
    if let Some(edge) = overlay.merge {
        lanes = lanes
            .max(edge.lane.saturating_add(1))
            .max(edge.node_lane.saturating_add(1));
    }
    lanes.max(1)
}

/// A row's graph, sized to that row's own indent.
///
/// `fade` is the colour behind the row, used by the gradient at a clipped edge.
/// `boundary` draws a dashed stub under the node for a shallow clone's last
/// fetched commit, where the line to the parents was never fetched; `muted`
/// paints that stub and a working-tree connector that has a lane of its own.
/// `overlay` is the render-time shift plus the dashed runs a dirty tree or a
/// merge in progress add on top of the core layout.
pub fn graph_column(
    row: GraphRow<'_>,
    boundary: bool,
    overlay: GraphOverlay,
    colors: [Hsla; LANES],
    fade: Hsla,
    muted: Hsla,
    zoom: f32,
) -> impl IntoElement {
    let lanes = column_lanes(row, overlay);
    let width = px(indent_at(lanes, zoom));
    // The paint closure must own its data; only visible rows are copied.
    let row = OwnedRow {
        node_lane: row.node_lane,
        node_color: row.node_color,
        lanes,
        shift: overlay.shift,
        passes: row.passes.to_vec(),
        edges: row.edges.to_vec(),
        boundary,
        worktree: overlay.worktree,
        merge_edge: overlay.merge,
        muted,
    };

    div().w(width).h_full().flex_none().child(
        canvas(
            move |bounds, _window, _cx| bounds,
            move |bounds, _, window, _cx| paint_row(&row, &colors, fade, zoom, bounds, window),
        )
        .size_full(),
    )
}

/// Graph geometry for the synthetic uncommitted row at the top of the list.
///
/// Lane numbers on the connector and the merge edge are where the row paints
/// them, after [`Self::lane_shift`]. `head_lane` and [`Self::merge_target`] stay
/// in the core layout's coordinates so callers can compare them with a
/// [`GraphRow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UncommittedGraph {
    /// HEAD's node lane in the core layout, before [`Self::lane_shift`].
    pub head_lane: u16,
    pub head_color: u16,
    /// HEAD names a commit. Its row may still be unloaded.
    pub has_head_edge: bool,
    /// Loaded row of HEAD. `None` while that commit is not in the list.
    pub head_index: Option<usize>,
    /// Core lane and colour of `MERGE_HEAD`.
    pub merge_target: Option<(u16, u16)>,
    /// Row of the `MERGE_HEAD` commit in the commit list, when the graph shows
    /// it. The target's lane alone cannot say how far down to draw.
    pub merge_target_index: Option<usize>,
    /// Lanes the uncommitted row reserves, including any private lane.
    pub lanes: u16,
    /// Lanes inserted on the left of every commit row. Private connectors
    /// occupy `0..lane_shift`; core lane `n` paints at `n + lane_shift`. Zero
    /// when each connector can ride a lane the rows above its target leave
    /// empty, which is the case when HEAD is the first row.
    pub lane_shift: u16,
    /// Painted lane of the hollow circle and the dashes above HEAD.
    /// `None` when HEAD is not loaded, so nothing is drawn towards a node the
    /// list does not show.
    pub connector_lane: Option<u16>,
    /// The connector has a lane of its own, so it is not painted in HEAD's
    /// branch colour.
    pub connector_muted: bool,
    /// Painted lane the merge dashes follow.
    pub merge_lane: Option<u16>,
}

/// The dashed line a merge in progress draws on its way to `MERGE_HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeEdge {
    /// Lane the dashes follow. This is `MERGE_HEAD`'s own lane when that column
    /// stays empty above the target, and a private lane when it does not.
    pub lane: u16,
    /// `MERGE_HEAD`'s node colour.
    pub color: u16,
    /// This row holds the target node, so the edge stops at the node instead of
    /// running through the row.
    pub to_node: bool,
    /// Painted lane of the target node. Differs from [`Self::lane`] when the
    /// dashes had to take a private column and curve in.
    pub node_lane: u16,
}

/// The dashed line from the working-tree circle down to HEAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorktreeEdge {
    /// Lane the dashes follow.
    pub lane: u16,
    /// Painted lane of HEAD's node.
    pub node_lane: u16,
    /// HEAD's node colour. Ignored when [`Self::muted`] is set.
    pub color: u16,
    /// Paint with the row's muted colour. A private lane is not a branch.
    pub muted: bool,
    /// This row holds HEAD, so the dashes end at its node.
    pub to_node: bool,
}

/// Render-time addition to one commit row: a uniform lane shift, plus whatever
/// dashed runs pass through this row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphOverlay {
    pub shift: u16,
    pub worktree: Option<WorktreeEdge>,
    pub merge: Option<MergeEdge>,
}

impl GraphOverlay {
    pub fn none() -> Self {
        Self {
            shift: 0,
            worktree: None,
            merge: None,
        }
    }
}

/// Inputs for [`plan_worktree_connector`]. Lanes are core-layout lanes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnectorEnds {
    pub head_index: Option<usize>,
    pub head_lane: u16,
    pub head_color: u16,
    /// An object id for HEAD exists, even when its row has not loaded.
    pub head_present: bool,
    pub merge: Option<MergeEnd>,
}

/// `MERGE_HEAD` when that commit is loaded and a merge is in progress.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MergeEnd {
    pub index: usize,
    pub lane: u16,
    pub color: u16,
}

/// Decides the working-tree connector from rows the core layout already built.
///
/// `row_at` is only called for rows above each target, and only while building
/// the plan. Painting a frame does not walk them again.
pub(crate) fn plan_worktree_connector<'a>(
    len: usize,
    row_at: impl Fn(usize) -> Option<GraphRow<'a>>,
    ends: ConnectorEnds,
) -> UncommittedGraph {
    let head_index = ends
        .head_index
        .filter(|index| *index < len && row_at(*index).is_some());
    // HEAD on the first row has nothing above it. The circle's own row already
    // meets that node, and inserting a lane there would move a graph that
    // already reads as connected.
    let head_private = head_index
        .is_some_and(|index| index > 0 && !lane_clear_until(&row_at, index, ends.head_lane));

    let merge = ends
        .merge
        .filter(|end| end.index < len && row_at(end.index).is_some());
    let merge_blocked = merge.is_some_and(|end| !lane_clear_until(&row_at, end.index, end.lane));
    // Two dashes on one column paint on top of each other. A shared core lane
    // takes a second private column even when the history itself leaves it
    // empty between the circle and the nearer target.
    let merge_collides = merge.is_some_and(|end| {
        !head_private
            && head_index.is_some_and(|head| head != end.index)
            && ends.head_present
            && end.lane == ends.head_lane
    });
    let merge_private = merge.is_some() && (merge_blocked || merge_collides);

    let shift = u16::from(head_private) + u16::from(merge_private);
    let connector_lane = if ends.head_present {
        head_index.map(|_| {
            if head_private {
                // The worktree circle is the leftmost private lane. A merge
                // that also needs one sits on the next column.
                0
            } else {
                ends.head_lane.saturating_add(u16::from(merge_private))
            }
        })
    } else {
        None
    };
    let merge_lane = merge.map(|end| {
        if merge_private {
            u16::from(head_private)
        } else {
            end.lane.saturating_add(shift)
        }
    });

    // The uncommitted row's text lines up with HEAD's node column, not only
    // with the circle: the rows below reserve that column too.
    let mut lanes = ends
        .head_lane
        .saturating_add(shift)
        .saturating_add(1)
        .max(1);
    if let Some(lane) = connector_lane {
        lanes = lanes.max(lane.saturating_add(1));
    }
    if let Some(lane) = merge_lane {
        lanes = lanes.max(lane.saturating_add(1));
    }

    UncommittedGraph {
        head_lane: ends.head_lane,
        head_color: ends.head_color,
        has_head_edge: ends.head_present,
        head_index,
        merge_target: merge.map(|end| (end.lane, end.color)),
        merge_target_index: merge.map(|end| end.index),
        lanes,
        lane_shift: shift,
        connector_lane,
        connector_muted: head_private,
        merge_lane,
    }
}

/// True when `lane` has no node, pass, or edge — including a curve that only
/// crosses it — on rows `0..until`.
fn lane_clear_until<'a>(
    row_at: &impl Fn(usize) -> Option<GraphRow<'a>>,
    until: usize,
    lane: u16,
) -> bool {
    (0..until).all(|index| row_at(index).is_some_and(|row| !lane_blocked(row, lane)))
}

/// A curve occupies every lane between its ends. A dash drawn on a lane a
/// curve merely crosses would still cut through that curve.
fn lane_blocked(row: GraphRow<'_>, lane: u16) -> bool {
    if row.node_lane == lane || row.passes.iter().any(|pass| pass.lane == lane) {
        return true;
    }
    row.edges.iter().any(|edge| {
        let low = edge.from_lane.min(edge.to_lane);
        let high = edge.from_lane.max(edge.to_lane);
        (low..=high).contains(&lane)
    })
}

impl UncommittedGraph {
    /// Circle on lane 0 and no connector. Used when the row is shown before a
    /// snapshot has published a plan.
    pub fn placeholder() -> Self {
        Self {
            head_lane: 0,
            head_color: 0,
            has_head_edge: false,
            head_index: None,
            merge_target: None,
            merge_target_index: None,
            lanes: 1,
            lane_shift: 0,
            connector_lane: None,
            connector_muted: false,
            merge_lane: None,
        }
    }

    /// Painted lane of the hollow circle.
    pub fn circle_lane(self) -> u16 {
        self.connector_lane
            .unwrap_or_else(|| self.head_lane.saturating_add(self.lane_shift))
    }

    /// Shift and dashed runs for commit row `index`.
    ///
    /// Rows below both targets still carry the shift. Their lines continue
    /// in the core layout and have to stay on the same x as the rows above.
    pub fn overlay_at(self, index: usize) -> GraphOverlay {
        GraphOverlay {
            shift: self.lane_shift,
            worktree: self.worktree_edge_at(index),
            merge: self.merge_edge_at(index),
        }
    }

    /// The working-tree dash on commit row `index`.
    ///
    /// `None` on HEAD's own row when HEAD is the first commit: the circle's
    /// row already draws the dash up to that boundary, and a second one would
    /// lengthen a line that already reads as connected. Also `None` when HEAD
    /// is not loaded.
    pub fn worktree_edge_at(self, index: usize) -> Option<WorktreeEdge> {
        let lane = self.connector_lane?;
        let head_index = self.head_index?;
        if head_index == 0 || index > head_index {
            return None;
        }
        Some(WorktreeEdge {
            lane,
            node_lane: self.head_lane.saturating_add(self.lane_shift),
            color: self.head_color,
            muted: self.connector_muted,
            to_node: index == head_index,
        })
    }

    /// The dashed run this merge edge contributes to commit row `index`.
    ///
    /// `None` once the target is behind us, or when the graph does not show the
    /// `MERGE_HEAD` commit at all (its branch is hidden), so nothing is drawn
    /// towards a node that is not on screen.
    pub fn merge_edge_at(self, index: usize) -> Option<MergeEdge> {
        let (core_lane, color) = self.merge_target?;
        let target = self.merge_target_index?;
        if index > target {
            return None;
        }
        let node_lane = core_lane.saturating_add(self.lane_shift);
        Some(MergeEdge {
            lane: self.merge_lane.unwrap_or(node_lane),
            color,
            to_node: index == target,
            node_lane,
        })
    }
}

/// Graph column for the synthetic uncommitted row: a hollow dashed circle, a
/// dashed edge down toward `HEAD`, and an optional dashed curve toward
/// `MERGE_HEAD` while merging.
pub fn uncommitted_graph_column(
    graph: UncommittedGraph,
    colors: [Hsla; LANES],
    fade: Hsla,
    muted: Hsla,
    zoom: f32,
) -> impl IntoElement {
    let lanes = graph.lanes.max(1);
    let width = px(indent_at(lanes, zoom));
    div().w(width).h_full().flex_none().child(
        canvas(
            move |bounds, _window, _cx| bounds,
            move |bounds, _, window, _cx| {
                paint_uncommitted_row(graph, &colors, fade, muted, zoom, bounds, window)
            },
        )
        .size_full(),
    )
}

struct OwnedRow {
    node_lane: u16,
    node_color: u16,
    lanes: u16,
    /// Added to every core lane while painting. See [`UncommittedGraph::lane_shift`].
    shift: u16,
    passes: Vec<PassThrough>,
    edges: Vec<Segment>,
    boundary: bool,
    worktree: Option<WorktreeEdge>,
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
#[cfg_attr(not(test), allow(dead_code))]
fn linework(row: &OwnedRow, visible: u16) -> Linework {
    linework_at(row, visible, 1.0)
}

fn linework_at(row: &OwnedRow, visible: u16, zoom: f32) -> Linework {
    // Every pass and edge contributes at most one run, so the buffers never
    // grow while they are filled.
    let capacity = row.passes.len() + row.edges.len();
    let mut straight = Vec::with_capacity(capacity);
    let mut curves = Vec::with_capacity(capacity);

    for pass in &row.passes {
        let lane = pass.lane.saturating_add(row.shift);
        if lane >= visible {
            continue;
        }
        straight.push((
            slot(pass.color),
            Straight {
                x: lane_offset_at(lane, zoom),
                from: 0.0,
                to: 1.0,
            },
        ));
    }

    for segment in &row.edges {
        let from_lane = segment.from_lane.saturating_add(row.shift);
        let to_lane = segment.to_lane.saturating_add(row.shift);
        if from_lane.min(to_lane) >= visible {
            continue;
        }
        let color = slot(segment.color);
        let from_x = lane_offset_at(from_lane, zoom);
        let to_x = lane_offset_at(to_lane, zoom);
        let (from_y, to_y, control_y) = match segment.half {
            Half::Top => (0.0, 0.5, 0.25),
            Half::Bottom => (0.5, 1.0, 0.75),
        };

        if from_lane == to_lane {
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
    zoom: f32,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    let top = bounds.origin.y;
    let height = bounds.size.height;
    let middle = top + height / 2.0;
    let left = bounds.origin.x;
    let stroke_width = STROKE_WIDTH * zoom;
    let fade_width = FADE_WIDTH * zoom;
    let marker_radius = MARKER_RADIUS * zoom;

    let clipped = row.lanes > MAX_LANES;
    let visible = row.lanes.min(MAX_LANES);
    // A clipped row paints inside the cap; its canvas is already only that
    // wide, so the mask also keeps lane changes from straddling the text.
    let clip = if clipped {
        Bounds::new(bounds.origin, size(px(clip_edge_at(zoom)), height))
    } else {
        bounds
    };

    let lines = linework_at(row, visible, zoom);

    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        // A straight run has the shape its stroke would have tessellated into,
        // so a quad paints the same pixels without touching the tessellator.
        for (color, run) in &lines.straight {
            let x = left + px(run.x) - px(stroke_width / 2.0);
            window.paint_quad(fill(
                Bounds::new(
                    point(x, top + height * run.from),
                    size(px(stroke_width), height * (run.to - run.from)),
                ),
                colors[*color],
            ));
        }

        // Curves are sorted by colour, so this builds at most one path per
        // palette slot per row. `PathBuilder` has no capacity to reserve; not
        // rebuilding one per segment is what keeps its buffers from growing.
        let mut paths: [Option<PathBuilder>; LANES] = std::array::from_fn(|_| None);
        for (color, curve) in &lines.curves {
            let path = paths[*color].get_or_insert_with(|| PathBuilder::stroke(px(stroke_width)));
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

        // Dashed runs are painted over the lanes. A private column has nothing
        // under it; a connector that reused an empty lane meets the node with
        // the same dash the circle's row started.
        if let Some(edge) = row.worktree {
            let color = if edge.muted {
                row.muted
            } else {
                colors[slot(edge.color)]
            };
            paint_connector(
                bounds,
                edge.lane,
                edge.node_lane,
                edge.to_node,
                color,
                zoom,
                visible,
                window,
            );
        }
        if let Some(edge) = row.merge_edge {
            paint_connector(
                bounds,
                edge.lane,
                edge.node_lane,
                edge.to_node,
                colors[slot(edge.color)],
                zoom,
                visible,
                window,
            );
        }
    });

    if clipped {
        let edge = bounds.origin.x + px(clip_edge_at(zoom));
        // Fading only the last four pixels keeps the cap's final lane crisp
        // while dissolving clipped lane ends into the row's fill.
        window.paint_quad(fill(
            Bounds::new(
                point(edge - px(fade_width), top),
                size(px(fade_width), height),
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
    // commit dot on the edge lane must stay crisp. The shift matches the lines
    // above, so a branch that started higher up still arrives at this dot.
    let node_lane = row.node_lane.saturating_add(row.shift);
    let node_x = if node_lane < visible {
        paint_node(
            bounds,
            middle,
            node_lane,
            colors[slot(row.node_color)],
            zoom,
            window,
        );
        Some(lane_offset_at(node_lane, zoom))
    } else if let Some(x) = node_marker_at(node_lane, row.lanes, zoom) {
        let color = colors[slot(row.node_color)];
        let diameter = marker_radius * 2.0;
        window.paint_quad(
            fill(
                Bounds::new(
                    point(
                        bounds.origin.x + px(x) - px(marker_radius),
                        middle - px(marker_radius),
                    ),
                    size(px(diameter), px(diameter)),
                ),
                color,
            )
            .corner_radii(px(marker_radius)),
        );
        Some(x)
    } else {
        None
    };

    if row.boundary
        && let Some(x) = node_x
    {
        paint_boundary_stub(bounds, middle, x, row.muted, zoom, window);
    }
}

/// Three short dashes below a shallow boundary's node: the parents exist on
/// the remote, but their side of the line was not fetched.
fn paint_boundary_stub(
    bounds: Bounds<Pixels>,
    middle: Pixels,
    lane: f32,
    color: Hsla,
    zoom: f32,
    window: &mut Window,
) {
    let dash = 1.5 * zoom;
    let gap = 2.0 * zoom;
    let x = bounds.origin.x + px(lane) - px(dash / 2.0);
    let mut y = middle + px((NODE_RADIUS + 1.0) * zoom);
    for _ in 0..3 {
        window.paint_quad(fill(
            Bounds::new(point(x, y), size(px(dash), px(dash))),
            color,
        ));
        y += px(dash + gap);
    }
}

/// Turns a lane-space point into a canvas coordinate.
fn lane_point(left: Pixels, top: Pixels, height: Pixels, at: Pt) -> Point<Pixels> {
    point(left + px(at.x), top + height * at.y)
}

fn paint_node(
    bounds: Bounds<Pixels>,
    middle: Pixels,
    lane: u16,
    color: Hsla,
    zoom: f32,
    window: &mut Window,
) {
    let node_radius = NODE_RADIUS * zoom;
    let diameter = node_radius * 2.0;
    window.paint_quad(
        fill(
            Bounds::new(
                point(
                    lane_x(bounds, lane, zoom) - px(node_radius),
                    middle - px(node_radius),
                ),
                size(px(diameter), px(diameter)),
            ),
            color,
        )
        .corner_radii(px(node_radius)),
    );
}

fn lane_x(bounds: Bounds<Pixels>, lane: u16, zoom: f32) -> Pixels {
    bounds.origin.x + px(lane_offset_at(lane, zoom))
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
    muted: Hsla,
    zoom: f32,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    let top = bounds.origin.y;
    let height = bounds.size.height;
    let middle = top + height / 2.0;
    let left = bounds.origin.x;
    let stroke_width = STROKE_WIDTH * zoom;
    let fade_width = FADE_WIDTH * zoom;
    let node_radius = NODE_RADIUS * zoom;
    let marker_radius = MARKER_RADIUS * zoom;

    let clipped = graph.lanes > MAX_LANES;
    let visible = graph.lanes.min(MAX_LANES);
    let clip = if clipped {
        Bounds::new(bounds.origin, size(px(clip_edge_at(zoom)), height))
    } else {
        bounds
    };

    let circle_lane = graph.circle_lane();
    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        // The merge fork leaves the circle and lands on the merge column at
        // the bottom of this row; commit rows carry it the rest of the way.
        if let Some(merge_lane) = graph.merge_lane {
            let merge_color = graph
                .merge_target
                .map(|(_, color)| colors[slot(color)])
                .unwrap_or(muted);
            if merge_lane != circle_lane && circle_lane.min(merge_lane) < visible {
                let curve = Curve {
                    from: Pt {
                        x: lane_offset_at(circle_lane, zoom),
                        y: 0.5,
                    },
                    control_a: Pt {
                        x: lane_offset_at(circle_lane, zoom),
                        y: 0.75,
                    },
                    control_b: Pt {
                        x: lane_offset_at(merge_lane, zoom),
                        y: 0.75,
                    },
                    to: Pt {
                        x: lane_offset_at(merge_lane, zoom),
                        y: 1.0,
                    },
                };
                paint_dashed_curve(left, top, height, curve, merge_color, zoom, window);
            } else if graph.connector_lane.is_none() && merge_lane < visible {
                paint_dashed_vertical(
                    left,
                    middle + px((NODE_RADIUS + 1.0) * zoom),
                    top + height,
                    merge_lane,
                    merge_color,
                    zoom,
                    window,
                );
            }
        }

        // No dash when HEAD is not loaded: a stub on lane 0 would claim a
        // connection the list cannot show, then jump once the commit arrives.
        if let Some(lane) = graph.connector_lane
            && lane < visible
        {
            let color = if graph.connector_muted {
                muted
            } else {
                colors[slot(graph.head_color)]
            };
            paint_dashed_vertical(
                left,
                middle + px((NODE_RADIUS + 1.0) * zoom),
                top + height,
                lane,
                color,
                zoom,
                window,
            );
        }
    });

    if clipped {
        let edge = bounds.origin.x + px(clip_edge_at(zoom));
        window.paint_quad(fill(
            Bounds::new(
                point(edge - px(fade_width), top),
                size(px(fade_width), height),
            ),
            linear_gradient(
                90.0,
                linear_color_stop(fade.opacity(0.0), 0.0),
                linear_color_stop(fade, 1.0),
            ),
        ));
    }

    let head_color = colors[slot(graph.head_color)];
    if circle_lane < visible {
        let center_x = lane_offset_at(circle_lane, zoom);
        let diameter = node_radius * 2.0;
        // Hollow interior masks the top of any downward edge under the ring.
        window.paint_quad(
            fill(
                Bounds::new(
                    point(left + px(center_x - node_radius), middle - px(node_radius)),
                    size(px(diameter), px(diameter)),
                ),
                fade,
            )
            .corner_radii(px(node_radius)),
        );
        let mut builder = PathBuilder::stroke(px(stroke_width));
        for arc in dashed_circle_arcs(center_x, 0.0, node_radius - stroke_width * 0.5) {
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
    } else if let Some(x) = node_marker_at(circle_lane, graph.lanes, zoom) {
        let diameter = marker_radius * 2.0;
        window.paint_quad(
            fill(
                Bounds::new(
                    point(left + px(x - marker_radius), middle - px(marker_radius)),
                    size(px(diameter), px(diameter)),
                ),
                fade,
            )
            .corner_radii(px(marker_radius)),
        );
        let mut builder = PathBuilder::stroke(px(stroke_width));
        for arc in dashed_circle_arcs(x, 0.0, marker_radius - stroke_width * 0.5) {
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

/// Curve from a node at mid-row up to a connector lane at the top edge.
///
/// `t = 0` is the node, so the shared dash gaps leave the dot's outline
/// alone. `t = 1` is the top of the row, where the dash from the row above
/// ends.
fn join_curve(node_x: f32, lane_x: f32) -> Curve {
    Curve {
        from: Pt { x: node_x, y: 0.5 },
        control_a: Pt { x: node_x, y: 0.25 },
        control_b: Pt { x: lane_x, y: 0.25 },
        to: Pt { x: lane_x, y: 0.0 },
    }
}

fn paint_dashed_curve(
    left: Pixels,
    top: Pixels,
    height: Pixels,
    curve: Curve,
    color: Hsla,
    zoom: f32,
    window: &mut Window,
) {
    let mut builder = PathBuilder::stroke(px(STROKE_WIDTH * zoom));
    for polyline in dashed_curve_segments(curve) {
        if let Some((first, rest)) = polyline.split_first() {
            builder.move_to(lane_point(left, top, height, *first));
            for point in rest {
                builder.line_to(lane_point(left, top, height, *point));
            }
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "paints one dashed connector with its lane, target and clip"
)]
fn paint_connector(
    bounds: Bounds<Pixels>,
    lane: u16,
    node_lane: u16,
    to_node: bool,
    color: Hsla,
    zoom: f32,
    visible: u16,
    window: &mut Window,
) {
    let top = bounds.origin.y;
    let height = bounds.size.height;
    let middle = top + height / 2.0;
    let left = bounds.origin.x;
    if to_node && lane != node_lane {
        if lane.min(node_lane) >= visible {
            return;
        }
        let curve = join_curve(lane_offset_at(node_lane, zoom), lane_offset_at(lane, zoom));
        paint_dashed_curve(left, top, height, curve, color, zoom, window);
        return;
    }
    if lane >= visible {
        return;
    }
    let end = if to_node {
        // Leave the node itself its solid outline.
        middle - px((NODE_RADIUS + 1.0) * zoom)
    } else {
        top + height
    };
    paint_dashed_vertical(left, top, end, lane, color, zoom, window);
}

#[allow(
    clippy::too_many_arguments,
    reason = "paints one dashed lane segment with explicit bounds and zoom"
)]
fn paint_dashed_vertical(
    left: Pixels,
    from: Pixels,
    to: Pixels,
    lane: u16,
    color: Hsla,
    zoom: f32,
    window: &mut Window,
) {
    let dash = 2.5 * zoom;
    let gap = 2.0 * zoom;
    let stroke_width = STROKE_WIDTH * zoom;
    let x = left + px(lane_offset_at(lane, zoom)) - px(stroke_width / 2.0);
    let mut y = from;
    while y < to {
        let dash_height = px(dash).min(to - y);
        window.paint_quad(fill(
            Bounds::new(point(x, y), size(px(stroke_width), dash_height)),
            color,
        ));
        y += px(dash + gap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamina_core::graph::{Half, Segment};

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
            shift: 0,
            passes: passes.to_vec(),
            edges: edges.to_vec(),
            boundary: false,
            worktree: None,
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
            head_index: Some(0),
            merge_target: Some((2, 5)),
            merge_target_index: Some(3),
            lanes: 3,
            lane_shift: 0,
            connector_lane: Some(0),
            connector_muted: false,
            merge_lane: Some(2),
        };
        for index in 0..3 {
            assert_eq!(
                graph.merge_edge_at(index),
                Some(MergeEdge {
                    lane: 2,
                    color: 5,
                    to_node: false,
                    node_lane: 2,
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
                node_lane: 2,
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
            head_index: Some(0),
            merge_target: Some((2, 5)),
            merge_target_index: None,
            lanes: 3,
            lane_shift: 0,
            connector_lane: Some(0),
            connector_muted: false,
            merge_lane: Some(2),
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

    #[test]
    fn graph_geometry_scales_proportionally_with_zoom() {
        assert_eq!(lane_offset_at(2, 1.0), lane_offset(2));
        assert_eq!(lane_offset_at(2, 1.5), lane_offset(2) * 1.5);
        assert_eq!(indent_at(4, 1.5), indent(4) * 1.5);
        assert_eq!(search_indent_at(1.5), search_indent() * 1.5);
        assert_eq!(node_marker_at(12, 13, 1.5), Some(clip_edge() * 1.5));

        let passes = [PassThrough { lane: 1, color: 0 }];
        let lines = linework_at(&owned(0, 2, &passes, &[]), 2, 1.5);
        assert_eq!(lines.straight[0].1.x, lane_offset(1) * 1.5);
    }
}
