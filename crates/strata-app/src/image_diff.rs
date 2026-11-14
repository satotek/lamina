//! The image diff view, shared by every pane that shows a diff.
//!
//! A raster image has no lines: the diff worker hands over both sides' bytes
//! and this module draws them. The mode (side by side, swipe, onion skin) is a
//! setting so every pane and every repository opens the way the user last
//! left it; the zoom, swipe position and onion opacity are transient state of
//! the view on screen.
//!
//! Decoding is left to GPUI: [`gpui::Image`] goes through its asset system,
//! which decodes on the background executor, so a large picture never stalls a
//! frame. This module only builds the [`Image`] once per content and lays the
//! pictures out.
//!
//! SVGs are text as well, so their diff stays available: [`svg_toggle`] puts
//! the text/rendered switch in the pane header and [`body`] returns `None`
//! while the text diff is asked for.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, DragMoveEvent, Entity, Hsla, Image, ImageFormat, IntoElement,
    ObjectFit, Pixels, Render, Size, Window, canvas, div, fill, img, point, px, relative, size,
};
use gpui_component::Selectable as _;
use strata_core::image::{ImageDiff, ImageKind, ImageSide};
use strata_core::lfs::format_size;
use strata_core::session::DiffView;

use crate::repo_view::RepoView;
use crate::settings::{self, ImageDiffMode, Settings};
use crate::style::Design;
use crate::ui::{self, ButtonSize};

/// The `Image` sources built so far, with the content they were built from.
///
/// The content is held beside the source: an allocation address is only a
/// stable identity while the bytes are alive, and a stale entry must never
/// stand in for a newer picture that happens to reuse the address.
type SourceCache = Rc<RefCell<Vec<(Arc<[u8]>, Arc<Image>)>>>;

/// The size of one checkerboard square. Transparency must read as
/// transparency; 16 px keeps the number of painted quads small on a wide pane.
const CHECKER_CELL: f32 = 16.0;

/// Transient state of the image view: one per repository view, so every pane
/// (history, compare, file history, worktree) shares the zoom and the split
/// position.
///
/// Cloning is cheap and shares the cells: a pane reached through an `Entity`
/// clones the handle before it needs `&mut App` for the rendering call.
#[derive(Clone)]
pub(crate) struct State {
    /// The image area's measured size, filled after layout. A `Cell` shared
    /// with the element callbacks, like the split extents, so a drag outlives
    /// the frame that created it.
    area: Rc<Cell<Option<Size<Pixels>>>>,
    /// Where the swipe divider sits, as a fraction of the width.
    swipe: Rc<Cell<f32>>,
    /// Opacity of the new side over the old one.
    onion: Rc<Cell<f32>>,
    /// Draw pictures at their own pixel size instead of fitting the pane.
    actual: Rc<Cell<bool>>,
    /// Show an SVG's text diff rather than its rendering.
    svg_text: Rc<Cell<bool>>,
    /// `Image::from_bytes` hashes and copies the whole file, so one source is
    /// built per content and kept for the next frame.
    sources: SourceCache,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub(crate) fn new() -> Self {
        Self {
            area: Rc::new(Cell::new(None)),
            swipe: Rc::new(Cell::new(0.5)),
            onion: Rc::new(Cell::new(0.5)),
            actual: Rc::new(Cell::new(false)),
            svg_text: Rc::new(Cell::new(true)),
            sources: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

/// The image body for `view`, or `None` when the pane should draw its text
/// diff (an SVG the user switched to text, or a file with no image data).
pub(crate) fn body(
    state: &State,
    view: &DiffView,
    design: &Design,
    entity: Entity<RepoView>,
    cx: &App,
) -> Option<AnyElement> {
    let diff = view.image.as_ref()?;
    if diff.kind == ImageKind::Svg && state.svg_text.get() {
        return None;
    }
    let mode = Settings::get(cx).diff.image_mode;
    let missing = missing_object(view);

    let mut column = div().flex().flex_col().flex_1().min_h_0().w_full();
    if let Some(summary) = view.lfs.clone() {
        column = column.child(ui::lfs_card(summary, design));
    }

    // One side means the file was added or deleted: there is nothing to
    // compare, so no mode applies.
    let single = match (&diff.old, &diff.new) {
        (Some(side), None) => Some(("Deleted", side)),
        (None, Some(side)) => Some(("Added", side)),
        _ => None,
    };
    if let Some((label, side)) = single {
        return Some(
            column
                .child(state.switcher(design, entity.clone(), cx, true))
                .child(single_image(state, label, side, design, missing))
                .into_any_element(),
        );
    }

    Some(
        column
            .child(state.switcher(design, entity.clone(), cx, false))
            .child(match mode {
                ImageDiffMode::SideBySide => side_by_side(state, diff, design, missing),
                ImageDiffMode::Swipe => swipe(state, diff, design, missing, entity),
                ImageDiffMode::OnionSkin => onion(state, diff, design, missing),
            })
            .into_any_element(),
    )
}

/// The text/rendered switch for an SVG, for a pane header. `None` for every
/// other file, so a pane can add it unconditionally.
pub(crate) fn svg_toggle(
    state: &State,
    view: &DiffView,
    entity: Entity<RepoView>,
) -> Option<AnyElement> {
    let diff = view.image.as_ref()?;
    if diff.kind != ImageKind::Svg {
        return None;
    }
    let rendered = !state.svg_text.get();
    let cell = state.svg_text.clone();
    Some(
        ui::button("toggle-svg-rendered", "Rendered")
            .size(ButtonSize::Tiny)
            .ghost()
            .selected(rendered)
            .tooltip(if rendered {
                "Show the text diff"
            } else {
                "Show the drawing"
            })
            .on_click(move |_event, _window, cx| {
                let cell = cell.clone();
                entity.update(cx, |_this, cx| {
                    cell.set(!cell.get());
                    cx.notify();
                });
            })
            .into_any_element(),
    )
}

/// Whether the file's bytes are present but too large to hand to the view.
fn missing_object(view: &DiffView) -> bool {
    view.lfs.as_ref().is_some_and(|summary| {
        summary.old.as_ref().is_some_and(|side| !side.present)
            || summary.new.as_ref().is_some_and(|side| !side.present)
    })
}

impl State {
    /// The mode and zoom controls. An added or deleted picture has no second
    /// side, so it only gets the zoom control.
    fn switcher(
        &self,
        design: &Design,
        entity: Entity<RepoView>,
        cx: &App,
        single: bool,
    ) -> AnyElement {
        let mode = Settings::get(cx).diff.image_mode;
        let settings_entity = entity.clone();
        let mut row = div()
            .flex()
            .items_center()
            .gap_1()
            .h(px(34.0))
            .flex_none()
            .px_3p5()
            .bg(design.background)
            .border_b_1()
            .border_color(design.border);
        if !single {
            for option in ImageDiffMode::ALL {
                let id = format!("image-mode-{}", option.id());
                let entity = settings_entity.clone();
                row = row.child(
                    ui::button(id, option.label())
                        .size(ButtonSize::Tiny)
                        .ghost()
                        .selected(mode == option)
                        .on_click(move |_event, _window, cx| {
                            settings::update(cx, |settings| settings.diff.image_mode = option);
                            entity.update(cx, |_this, cx| cx.notify());
                        }),
                );
            }
        }
        row = row.child(div().flex_1());

        if !single && mode == ImageDiffMode::OnionSkin {
            row = row.child(self.opacity_slider(design, entity.clone()));
        }

        let actual = self.actual.clone();
        row = row.child(
            ui::button("image-zoom", if self.actual.get() { "1:1" } else { "Fit" })
                .size(ButtonSize::Tiny)
                .ghost()
                .selected(self.actual.get())
                .tooltip(if self.actual.get() {
                    "Fit the picture to the pane"
                } else {
                    "Show the picture at its own pixel size"
                })
                .on_click(move |_event, _window, cx| {
                    let actual = actual.clone();
                    entity.update(cx, |_this, cx| {
                        actual.set(!actual.get());
                        cx.notify();
                    });
                }),
        );
        row.into_any_element()
    }

    fn opacity_slider(&self, design: &Design, entity: Entity<RepoView>) -> AnyElement {
        let value = self.onion.get();
        let onion = self.onion.clone();
        let drag_entity = entity.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(design.muted)
                    .child(format!("Opacity {:.0}%", value * 100.0)),
            )
            .child(
                div()
                    .relative()
                    .w(px(120.0))
                    .h(px(14.0))
                    .cursor_ew_resize()
                    .id("image-onion-slider")
                    .on_drag(OnionDrag, |drag, _, _, cx| cx.new(|_| *drag))
                    .on_drag_move(move |event: &DragMoveEvent<OnionDrag>, _window, cx| {
                        let _ = event.drag(cx);
                        onion.set(fraction_at(event.event.position.x, event.bounds));
                        drag_entity.update(cx, |_this, cx| cx.notify());
                    })
                    .child(
                        div()
                            .absolute()
                            .top(px(5.0))
                            .left_0()
                            .right_0()
                            .h(px(4.0))
                            .rounded_full()
                            .bg(design.field),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(5.0))
                            .left_0()
                            .w(relative(value))
                            .h(px(4.0))
                            .rounded_full()
                            .bg(design.accent),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(1.0))
                            .left(relative(value))
                            .ml(px(-6.0))
                            .size(px(12.0))
                            .rounded_full()
                            .bg(design.accent)
                            .border_1()
                            .border_color(design.background),
                    ),
            )
            .into_any_element()
    }
}

/// A drag payload for the swipe divider. The type only exists to route the
/// drag to the image area that draws the pictures.
#[derive(Clone, Copy)]
struct SwipeDrag;

impl Render for SwipeDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// A drag payload for the onion opacity track.
#[derive(Clone, Copy)]
struct OnionDrag;

impl Render for OnionDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// How far into `bounds` a pointer at `position` sits, as a fraction.
pub(crate) fn fraction_at(position: Pixels, bounds: Bounds<Pixels>) -> f32 {
    let width = f32::from(bounds.size.width);
    if !(width.is_finite() && width > 0.0) {
        return 0.5;
    }
    ((f32::from(position) - f32::from(bounds.origin.x)) / width).clamp(0.0, 1.0)
}

/// The opacity the onion skin draws the new side with.
pub(crate) fn onion_alpha(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

/// The scale that fits `image` inside `area` without cropping it.
pub(crate) fn fit_scale(image: (u32, u32), area: (f32, f32)) -> f32 {
    let (width, height) = (image.0 as f32, image.1 as f32);
    if width <= 0.0 || height <= 0.0 || area.0 <= 0.0 || area.1 <= 0.0 {
        return 1.0;
    }
    (area.0 / width).min(area.1 / height).max(0.0)
}

/// The drawn size of `image` fitted into `area`.
pub(crate) fn fitted_size(image: (u32, u32), area: (f32, f32)) -> (f32, f32) {
    let scale = fit_scale(image, area);
    (image.0 as f32 * scale, image.1 as f32 * scale)
}

/// One labelled side, with the checkerboard behind the picture.
fn picture(
    state: &State,
    side: &ImageSide,
    label: &str,
    design: &Design,
    missing: bool,
) -> AnyElement {
    let mut frame = div()
        .relative()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_hidden()
        .bg(design.background);
    if let Some(source) = source_for(state, side) {
        frame = frame.child(checkerboard(design));
        frame = frame.child(drawn_image(state, source, side, design));
    } else {
        frame = frame.child(centered_message(placeholder(side, missing), design));
    }
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(side_label(label, side, design))
        .child(frame)
        .into_any_element()
}

fn side_by_side(state: &State, diff: &ImageDiff, design: &Design, missing: bool) -> AnyElement {
    let old = diff.old.as_ref();
    let new = diff.new.as_ref();
    let mut row = div().flex().flex_1().min_h_0().w_full();
    if let Some(old) = old {
        row = row.child(picture(state, old, "Old", design, missing));
    }
    if old.is_some() && new.is_some() {
        row = row.child(div().flex_none().w(px(1.0)).h_full().bg(design.border));
    }
    if let Some(new) = new {
        row = row.child(picture(state, new, "New", design, missing));
    }
    row.into_any_element()
}

fn single_image(
    state: &State,
    label: &str,
    side: &ImageSide,
    design: &Design,
    missing: bool,
) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(picture(state, side, label, design, missing))
        .into_any_element()
}

fn swipe(
    state: &State,
    diff: &ImageDiff,
    design: &Design,
    missing: bool,
    entity: Entity<RepoView>,
) -> AnyElement {
    let (Some(old), Some(new)) = (diff.old.as_ref(), diff.new.as_ref()) else {
        let (label, side) = match (&diff.old, &diff.new) {
            (Some(side), None) => ("Deleted", side),
            (None, Some(side)) => ("Added", side),
            _ => return div().into_any_element(),
        };
        return single_image(state, label, side, design, missing);
    };
    let fraction = clamped_fraction(state.swipe.get());
    let old_source = source_for(state, old);
    let new_source = source_for(state, new);

    // The old picture is the overlay clipped from the area's left edge, so the
    // divider's left reads "Old" and its right "New", like the header labels.
    // Both pictures are positioned against the same origin. Without the measured
    // area the first frame draws a plain overlay and the next one is exact.
    let area = state.area.clone();
    let overlay_width = area
        .get()
        .map(|size| f32::from(size.width) * fraction)
        .unwrap_or(0.0);
    let swipe_cell = state.swipe.clone();
    let drag_entity = entity.clone();

    let mut area_element = div()
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_hidden()
        .on_drag_move(move |event: &DragMoveEvent<SwipeDrag>, _window, cx| {
            let _ = event.drag(cx);
            swipe_cell.set(fraction_at(event.event.position.x, event.bounds));
            drag_entity.update(cx, |_this, cx| cx.notify());
        })
        .on_children_prepainted({
            let area = area.clone();
            move |bounds: Vec<Bounds<Pixels>>, _window, _cx| {
                if let Some(bounds) = bounds.first() {
                    area.set(Some(bounds.size));
                }
            }
        })
        .child(checkerboard(design));

    let area_size = area.get();
    if let Some(source) = new_source {
        area_element = area_element.child(positioned_image(state, source, new, area_size, design));
    } else {
        area_element = area_element.child(centered_message(placeholder(new, missing), design));
    }

    if let Some(source) = old_source {
        let mut overlay = div()
            .absolute()
            .left_0()
            .top_0()
            .bottom_0()
            .w(px(overlay_width))
            .overflow_hidden()
            .child(positioned_image(state, source, old, area_size, design));
        // Until the area is known the divider cannot be placed: show the full
        // old picture for one frame rather than a zero-width sliver.
        if area_size.is_none() {
            overlay = overlay.w_full();
        }
        area_element = area_element.child(overlay);
    }

    // The divider and its grab band. The band is wider than the line so a
    // pointer finds it, and it is the only thing that starts a swipe drag.
    area_element = area_element.child(
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(relative(fraction))
            .ml(px(-6.0))
            .w(px(12.0))
            .cursor_ew_resize()
            .id("image-swipe-handle")
            .on_drag(SwipeDrag, |drag, _, _, cx| cx.new(|_| *drag))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(5.0))
                    .w(px(2.0))
                    .bg(design.accent),
            )
            .child(
                div()
                    .absolute()
                    .top(relative(0.5))
                    .left(px(1.0))
                    .size(px(10.0))
                    .mt(px(-5.0))
                    .rounded_full()
                    .bg(design.accent),
            ),
    );

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3p5()
                .h(px(24.0))
                .flex_none()
                .text_size(px(11.0))
                .text_color(design.muted)
                .child(side_label("Old", old, design))
                .child("Drag the divider")
                .child(side_label("New", new, design)),
        )
        .child(area_element)
        .into_any_element()
}

fn onion(state: &State, diff: &ImageDiff, design: &Design, missing: bool) -> AnyElement {
    let (Some(old), Some(new)) = (diff.old.as_ref(), diff.new.as_ref()) else {
        let (label, side) = match (&diff.old, &diff.new) {
            (Some(side), None) => ("Deleted", side),
            (None, Some(side)) => ("Added", side),
            _ => return div().into_any_element(),
        };
        return single_image(state, label, side, design, missing);
    };
    let alpha = onion_alpha(state.onion.get());

    let mut stack = div()
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_hidden()
        .child(checkerboard(design));
    match source_for(state, old) {
        Some(source) => {
            stack = stack.child(
                div()
                    .absolute()
                    .inset_0()
                    .child(drawn_image(state, source, old, design)),
            );
        }
        None => {
            stack = stack.child(centered_message(placeholder(old, missing), design));
        }
    }
    if let Some(source) = source_for(state, new) {
        stack = stack.child(
            div()
                .absolute()
                .inset_0()
                .opacity(alpha)
                .child(drawn_image(state, source, new, design)),
        );
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3p5()
                .h(px(24.0))
                .flex_none()
                .text_size(px(11.0))
                .text_color(design.muted)
                .child(side_label("Old", old, design))
                .child(format!("New at {:.0}%", alpha * 100.0))
                .child(side_label("New", new, design)),
        )
        .child(stack)
        .into_any_element()
}

fn clamped_fraction(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

/// The area-relative box one side draws in, centred in `area`.
///
/// Both sides of a swipe share the same box: the clipped side must not be
/// scaled into its clip, or the two pictures would not line up.
fn picture_box(state: &State, side: &ImageSide, area: Size<Pixels>) -> (f32, f32, f32, f32) {
    let (area_width, area_height) = (f32::from(area.width), f32::from(area.height));
    let (width, height) = match (side.width, side.height) {
        (Some(width), Some(height)) if state.actual.get() => (width as f32, height as f32),
        (Some(width), Some(height)) => fitted_size((width, height), (area_width, area_height)),
        _ => (area_width, area_height),
    };
    (
        (area_width - width) / 2.0,
        (area_height - height) / 2.0,
        width,
        height,
    )
}

/// A swipe-side picture at the shared `area` coordinates.
fn positioned_image(
    state: &State,
    source: Arc<Image>,
    side: &ImageSide,
    area: Option<Size<Pixels>>,
    design: &Design,
) -> AnyElement {
    let Some(area) = area else {
        return div()
            .absolute()
            .inset_0()
            .child(drawn_image(state, source, side, design))
            .into_any_element();
    };
    let (left, top, width, height) = picture_box(state, side, area);
    div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .h(px(height))
        .child(
            img(source)
                .size_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback({
                    let color = design.muted;
                    move || centered_fallback("Cannot decode this image", color)
                }),
        )
        .into_any_element()
}

/// The picture element for one side, fitted or at its own pixel size.
fn drawn_image(state: &State, source: Arc<Image>, side: &ImageSide, design: &Design) -> AnyElement {
    if state.actual.get()
        && let (Some(width), Some(height)) = (side.width, side.height)
    {
        return div()
            .flex()
            .size_full()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .child(
                img(source)
                    .w(px(width as f32))
                    .h(px(height as f32))
                    .object_fit(ObjectFit::Contain),
            )
            .into_any_element();
    }
    img(source)
        .size_full()
        .object_fit(ObjectFit::Contain)
        .with_fallback({
            let color = design.muted;
            move || centered_fallback("Cannot decode this image", color)
        })
        .into_any_element()
}

fn centered_fallback(text: &'static str, color: Hsla) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn centered_message(text: &'static str, design: &Design) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .p_4()
        .text_size(px(12.0))
        .text_color(design.muted)
        .child(text)
        .into_any_element()
}

fn placeholder(side: &ImageSide, missing: bool) -> &'static str {
    if missing {
        "The object is not downloaded"
    } else if side.bytes.is_none() {
        "This image is too large to preview (20 MiB limit)."
    } else {
        "This image cannot be shown here."
    }
}

fn side_label(label: &str, side: &ImageSide, design: &Design) -> AnyElement {
    let mut text = String::from(label);
    if let (Some(width), Some(height)) = (side.width, side.height) {
        text.push_str(&format!(" · {width} × {height}"));
    }
    text.push_str(&format!(" · {}", format_size(side.size)));
    div()
        .flex_none()
        .h(px(24.0))
        .flex()
        .items_center()
        .px_3p5()
        .text_size(px(11.0))
        .text_color(design.muted)
        .child(text)
        .into_any_element()
}

/// The `Image` for one side, built once per content.
fn source_for(state: &State, side: &ImageSide) -> Option<Arc<Image>> {
    let bytes = side.bytes.as_ref()?;
    let format = match side.format? {
        strata_core::image::ImageFormat::Png => ImageFormat::Png,
        strata_core::image::ImageFormat::Jpeg => ImageFormat::Jpeg,
        strata_core::image::ImageFormat::Gif => ImageFormat::Gif,
        strata_core::image::ImageFormat::Webp => ImageFormat::Webp,
        strata_core::image::ImageFormat::Bmp => ImageFormat::Bmp,
        strata_core::image::ImageFormat::Ico => ImageFormat::Ico,
        strata_core::image::ImageFormat::Svg => ImageFormat::Svg,
    };
    if let Some((_, source)) = state
        .sources
        .borrow()
        .iter()
        .find(|(cached, _)| Arc::ptr_eq(cached, bytes))
    {
        return Some(Arc::clone(source));
    }
    let source = Arc::new(Image::from_bytes(format, bytes.to_vec()));
    let mut sources = state.sources.borrow_mut();
    // A view shows one file at a time; keeping a few entries covers a quick
    // back-and-forth without holding every picture ever opened.
    if sources.len() >= 4 {
        sources.clear();
    }
    sources.push((Arc::clone(bytes), Arc::clone(&source)));
    Some(source)
}

/// The transparency grid behind a picture.
fn checkerboard(design: &Design) -> AnyElement {
    let light = design.panel;
    let dark = design.background;
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            window.paint_quad(fill(bounds, light));
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            let mut row = 0;
            let mut y = 0.0;
            while y < height {
                let mut column = 0;
                let mut x = 0.0;
                while x < width {
                    if (row + column) % 2 == 0 {
                        let cell = size(
                            px(CHECKER_CELL.min(width - x)),
                            px(CHECKER_CELL.min(height - y)),
                        );
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.origin.x + px(x), bounds.origin.y + px(y)),
                                cell,
                            ),
                            dark,
                        ));
                    }
                    x += CHECKER_CELL;
                    column += 1;
                }
                y += CHECKER_CELL;
                row += 1;
            }
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(width: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(100.0), px(0.0)), size(px(width), px(200.0)))
    }

    #[test]
    fn a_swipe_pointer_maps_to_a_clamped_fraction() {
        let area = bounds(400.0);
        assert_eq!(fraction_at(px(100.0), area), 0.0);
        assert_eq!(fraction_at(px(300.0), area), 0.5);
        assert_eq!(fraction_at(px(10_000.0), area), 1.0);
        assert_eq!(fraction_at(px(0.0), area), 0.0);
    }

    #[test]
    fn an_unmeasured_swipe_area_stays_centred() {
        let empty = Bounds::new(point(px(10.0), px(10.0)), size(px(0.0), px(0.0)));
        assert_eq!(fraction_at(px(500.0), empty), 0.5);
    }

    #[test]
    fn onion_opacity_is_kept_between_zero_and_one() {
        assert_eq!(onion_alpha(0.0), 0.0);
        assert_eq!(onion_alpha(0.25), 0.25);
        assert_eq!(onion_alpha(1.0), 1.0);
        assert_eq!(onion_alpha(-3.0), 0.0);
        assert_eq!(onion_alpha(9.0), 1.0);
        assert_eq!(onion_alpha(f32::NAN), 0.5);
    }

    #[test]
    fn a_wide_image_fits_by_width_and_a_tall_one_by_height() {
        assert_eq!(fitted_size((100, 50), (400.0, 400.0)), (400.0, 200.0));
        assert_eq!(fitted_size((50, 100), (400.0, 400.0)), (200.0, 400.0));
        assert_eq!(fitted_size((200, 200), (300.0, 150.0)), (150.0, 150.0));
        assert_eq!(fit_scale((10, 10), (0.0, 0.0)), 1.0);
    }

    #[test]
    fn every_mode_has_an_id_and_a_label() {
        let ids: Vec<&str> = ImageDiffMode::ALL.iter().map(|mode| mode.id()).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids, ["side-by-side", "swipe", "onion-skin"]);
        assert!(
            ImageDiffMode::ALL
                .iter()
                .all(|mode| !mode.label().is_empty())
        );
        assert_eq!(ImageDiffMode::default(), ImageDiffMode::SideBySide);
    }
}
