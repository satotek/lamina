//! Repository tabs: the strip and each tab item inside it.
//!
//! Styled from [`Design`] tokens so the strip reads as an extension of the
//! 44 px chrome title bar above it. The strip paints the bottom rule itself
//! rather than as a border, which lets the active tab's fill cover the pixel
//! under it and meet the content below with no seam. Each tab renders the
//! repository name, an optional accent dot while an operation is in progress,
//! a hover-revealed close button, middle-click close, and drag-and-drop
//! reordering with a drop indicator line.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, ClickEvent, Context, Div, ElementId, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render, RenderOnce,
    ScrollHandle, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::tooltip::Tooltip;

use crate::settings::Settings;
use crate::style::Design;

/// Height of the repository tab strip when two or more tabs are open. Every
/// tab is the same height and runs the full strip, so the active one reaches
/// the content below.
pub const TAB_STRIP_HEIGHT: f32 = 32.0;

/// Slimmer height for secondary tab strips such as the embedded terminal panel.
pub const COMPACT_TAB_STRIP_HEIGHT: f32 = 28.0;

#[derive(Clone)]
struct DraggedTab {
    index: usize,
    label: SharedString,
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        div()
            .px_3()
            .py_1()
            .rounded(design.px(6.0))
            .bg(design.panel)
            .border_1()
            .border_color(design.accent)
            .text_size(px(design.small() + design.scale(0.5)))
            .font_weight(FontWeight::MEDIUM)
            .text_color(design.text)
            .shadow_md()
            .child(self.label.clone())
    }
}

/// The horizontal container sitting directly below the title bar when
/// multiple repository tabs are open. `scroll` lets the caller reveal the
/// active tab and survives re-renders, so the strip keeps its offset.
pub fn tab_strip(
    id: impl Into<ElementId>,
    design: &Design,
    scroll: &ScrollHandle,
) -> Stateful<Div> {
    tab_strip_sized(id, TAB_STRIP_HEIGHT, design, scroll).px_2()
}

/// A slimmer horizontal tab strip for panel headers such as the terminal panel.
#[cfg_attr(not(feature = "terminal"), allow(dead_code))]
pub fn compact_tab_strip(
    id: impl Into<ElementId>,
    design: &Design,
    scroll: &ScrollHandle,
) -> Stateful<Div> {
    tab_strip_sized(id, COMPACT_TAB_STRIP_HEIGHT, design, scroll).px_1()
}

fn tab_strip_sized(
    id: impl Into<ElementId>,
    height: f32,
    design: &Design,
    scroll: &ScrollHandle,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .flex()
        .flex_none()
        .items_end()
        .h(design.px(height))
        .w_full()
        .overflow_x_scroll()
        .track_scroll(scroll)
        .bg(design.chrome)
        // Drawn before the tabs, so an opaque tab fill hides the rule under
        // itself while the transparent inactive ones show it through.
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(1.0))
                .bg(design.border),
        )
}

/// Scrolls the tab at `index` into view.
///
/// [`tab_strip`] paints its bottom rule as the first child of the scroll
/// container, so a tab's index in the handle's child list is one past its
/// deck index.
pub fn reveal_tab(scroll: &ScrollHandle, index: usize) {
    scroll.scroll_to_item(index + 1);
}

#[derive(Clone, Copy)]
struct TabColors {
    background: Hsla,
    field: Hsla,
    border: Hsla,
    text: Hsla,
    muted: Hsla,
    accent: Hsla,
    font_size: f32,
    zoom: f32,
}

impl TabColors {
    fn px(self, value: f32) -> gpui::Pixels {
        px(value * self.zoom)
    }
}

impl From<&Design> for TabColors {
    fn from(design: &Design) -> Self {
        Self {
            background: design.background,
            field: design.field,
            border: design.border,
            text: design.text,
            muted: design.muted,
            accent: design.accent,
            font_size: design.small() + design.scale(0.5),
            zoom: design.zoom,
        }
    }
}

type SelectHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type CloseHandler = Rc<dyn Fn(&mut Window, &mut App)>;
type ReorderHandler = Rc<dyn Fn(usize, usize, &mut Window, &mut App)>;

/// One tab inside [`tab_strip`] or [`compact_tab_strip`].
#[derive(IntoElement)]
pub struct UiTab {
    id: ElementId,
    index: usize,
    label: SharedString,
    active: bool,
    compact: bool,
    busy: bool,
    separator: bool,
    always_show_close: bool,
    active_bg: Option<Hsla>,
    tooltip: Option<SharedString>,
    hint: Option<SharedString>,
    on_select: Option<SelectHandler>,
    on_close: Option<CloseHandler>,
    on_reorder: Option<ReorderHandler>,
    colors: TabColors,
}

/// Builds a repository tab at position `index`.
pub fn tab(
    id: impl Into<ElementId>,
    index: usize,
    label: impl Into<SharedString>,
    active: bool,
    design: &Design,
) -> UiTab {
    UiTab {
        id: id.into(),
        index,
        label: label.into(),
        active,
        compact: false,
        busy: false,
        separator: false,
        always_show_close: false,
        active_bg: None,
        tooltip: None,
        hint: None,
        on_select: None,
        on_close: None,
        on_reorder: None,
        colors: TabColors::from(design),
    }
}

impl UiTab {
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub fn compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }

    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub fn active_bg(mut self, bg: Hsla) -> Self {
        self.active_bg = Some(bg);
        self
    }

    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub fn always_show_close(mut self, show: bool) -> Self {
        self.always_show_close = show;
        self
    }

    pub fn busy(mut self, busy: bool) -> Self {
        self.busy = busy;
        self
    }

    /// Draws a 1 px divider on the tab's leading edge. The caller turns this
    /// on only between two inactive tabs, where the strip would otherwise run
    /// them together.
    pub fn separator(mut self, separator: bool) -> Self {
        self.separator = separator;
        self
    }

    /// The tip's first line, usually the repository name.
    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    /// Adds the muted second line of the tip: the repository's path with the
    /// home directory folded to `~` and long paths shortened from the front.
    pub fn hint(mut self, path: &Path) -> Self {
        self.hint = Some(path_hint(path).into());
        self
    }

    pub fn on_select(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }

    pub fn on_close(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(handler));
        self
    }

    pub fn on_reorder(
        mut self,
        handler: impl Fn(usize, usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_reorder = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for UiTab {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let colors = self.colors;
        let index = self.index;
        let scope = if self.compact {
            "compact-tab"
        } else {
            "ui-tab"
        };
        let group: SharedString = format!("{scope}-group-{index}").into();
        let label = self.label.clone();

        // A tab shrinks before the strip scrolls, down to a width that still
        // shows a few characters and the close button; the label ellipsizes
        // in between. `flex_none` made eight tabs wider than the minimum
        // window, so the last ones were unreachable.
        let (height, min_w, max_w, gap, font_size, close_box) = if self.compact {
            (
                COMPACT_TAB_STRIP_HEIGHT,
                64.0,
                180.0,
                4.0,
                colors.font_size - 0.5 * colors.zoom,
                16.0,
            )
        } else {
            (TAB_STRIP_HEIGHT, 80.0, 220.0, 6.0, colors.font_size, 18.0)
        };

        // The transparent top border reserves the accent line's 2 px so the
        // label does not move when the tab becomes active.
        let mut tab = div()
            .id(self.id)
            .debug_selector({
                let name = format!("{scope}-tab-{index}");
                move || name.clone()
            })
            .group(group.clone())
            .relative()
            .flex()
            .flex_shrink(1.0)
            .items_center()
            .gap(colors.px(gap))
            .h(colors.px(height))
            .min_w(colors.px(min_w))
            .max_w(colors.px(max_w))
            .px(colors.px(if self.compact { 8.0 } else { 12.0 }))
            .border_t(px(2.0))
            .border_color(Hsla::transparent_black())
            .cursor_pointer()
            .text_size(px(font_size));

        tab = if self.active {
            tab.bg(self.active_bg.unwrap_or(colors.background))
                .border_color(colors.accent)
                .text_color(colors.text)
                .font_weight(FontWeight::MEDIUM)
        } else {
            tab.text_color(colors.muted)
                .font_weight(FontWeight::NORMAL)
                .hover(move |style| style.bg(colors.field).text_color(colors.text))
        };

        if self.separator {
            let inset = if self.compact { 6.0 } else { 8.0 };
            tab = tab.child(
                div()
                    .absolute()
                    .left_0()
                    .top(colors.px(inset))
                    .bottom(colors.px(inset))
                    .w(px(1.0))
                    .bg(colors.border),
            );
        }

        let hint = self.hint.clone();
        if let Some(tip) = self.tooltip {
            tab = if hint.is_some() {
                tab.tooltip(move |window, cx| {
                    let tip = tip.clone();
                    let hint = hint.clone();
                    Tooltip::element(move |_, _| tab_tip(tip.clone(), hint.clone(), colors))
                        .build(window, cx)
                })
            } else {
                tab.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            };
        }

        if let Some(on_select) = self.on_select {
            tab = tab.on_click(move |event, window, cx| on_select(event, window, cx));
        }

        if let Some(on_close) = self.on_close.clone() {
            tab = tab.on_mouse_down(MouseButton::Middle, move |_, window, cx| {
                cx.stop_propagation();
                on_close(window, cx);
            });
        }

        if let Some(on_reorder) = self.on_reorder {
            let drag = DraggedTab {
                index,
                label: label.clone(),
            };
            let accent = colors.accent;
            let active = self.active;
            tab = tab
                .on_drag(drag, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
                // A 2 px accent line on the edge the tab would land against,
                // rather than a tint over the whole target, so the pointer
                // shows the new order instead of only which tab is under it.
                // The colour applies to every border, so an inactive target
                // drops its reserved top border here; only the active tab
                // keeps its accent line there.
                .drag_over::<DraggedTab>(move |style, dragged, _, _| {
                    if dragged.index == index {
                        return style;
                    }
                    let style = if active {
                        style
                    } else {
                        style.border_t(px(0.0))
                    };
                    if dragged.index < index {
                        style
                            .border_r(px(2.0))
                            .border_l(px(0.0))
                            .border_color(accent)
                    } else {
                        style
                            .border_l(px(2.0))
                            .border_r(px(0.0))
                            .border_color(accent)
                    }
                })
                .on_drop(move |dragged: &DraggedTab, window, cx| {
                    on_reorder(dragged.index, index, window, cx);
                });
        }

        tab = tab.child(div().flex_1().min_w_0().truncate().child(label));

        // The dot follows the name so the label keeps its leading edge and
        // only the tab's tail moves when an operation starts.
        if self.busy {
            tab = tab.child(
                div()
                    .id((SharedString::from(format!("{scope}-busy")), index))
                    .flex_none()
                    .w(colors.px(6.0))
                    .h(colors.px(6.0))
                    .rounded_full()
                    .bg(colors.accent),
            );
        }

        if let Some(on_close) = self.on_close {
            let close = div()
                .id((SharedString::from(format!("{scope}-close")), index))
                .debug_selector({
                    let name = format!("{scope}-close-{index}");
                    move || name.clone()
                })
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(colors.px(close_box))
                .h(colors.px(close_box))
                .rounded(colors.px(4.0))
                .text_size(px(font_size + colors.zoom))
                .text_color(colors.muted)
                .when(!self.active && !self.always_show_close, |close| {
                    close
                        .opacity(0.0)
                        .group_hover(group, |style| style.opacity(1.0))
                })
                .hover(move |style| style.bg(colors.field).text_color(colors.text))
                .cursor_pointer()
                // Do not let pressing the close button start a tab drag or select the tab.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_close(window, cx);
                })
                .child("×");
            tab = tab.child(close);
        }

        tab
    }
}

/// The two-line tab tip: the name, then the muted path hint.
fn tab_tip(name: SharedString, hint: Option<SharedString>, colors: TabColors) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(colors.px(2.0))
        .max_w(colors.px(360.0))
        .child(
            div()
                .truncate()
                .text_size(px(colors.font_size))
                .font_weight(FontWeight::MEDIUM)
                .text_color(colors.text)
                .child(name),
        )
        .when_some(hint, |tip, hint| {
            tip.child(
                div()
                    .truncate()
                    .text_size(px(colors.font_size - 0.5 * colors.zoom))
                    .text_color(colors.muted)
                    .child(hint),
            )
        })
}

/// The tip's path line: `~/src/lamina` for a path under the home directory,
/// with anything longer than a tip's width cut down to its last two parts.
fn path_hint(path: &Path) -> String {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    path_hint_in(path, home.as_deref())
}

fn path_hint_in(path: &Path, home: Option<&Path>) -> String {
    let folded = match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    const MAX_COLUMNS: usize = 48;
    if lamina_core::text::display_width(&folded) <= MAX_COLUMNS {
        return folded;
    }
    let tail = path
        .components()
        .rev()
        .take(2)
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let tail = tail.into_iter().rev().collect::<Vec<_>>().join("/");
    lamina_core::text::truncate_width(&format!("…/{tail}"), MAX_COLUMNS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_hint_folds_home_and_shortens_long_paths() {
        let home = Path::new("/home/mio");
        assert_eq!(
            path_hint_in(Path::new("/home/mio/src/lamina"), Some(home)),
            "~/src/lamina"
        );
        assert_eq!(
            path_hint_in(Path::new("/srv/lamina"), Some(home)),
            "/srv/lamina",
            "a path outside home keeps its full form"
        );
        assert_eq!(path_hint_in(Path::new("/home/mio"), Some(home)), "~");

        let long = Path::new("/home/mio/one/two/three/four/five/six/seven/eight/nine/lamina");
        assert_eq!(path_hint_in(long, Some(home)), "…/nine/lamina");
    }

    #[test]
    fn a_short_path_keeps_every_part() {
        let home = Path::new("/home/mio");
        let path = Path::new("/home/mio/nine/lamina");
        assert_eq!(
            path_hint_in(path, Some(home)),
            "~/nine/lamina",
            "a hint that already fits is not cut"
        );
    }

    struct StripHarness {
        scroll: ScrollHandle,
        design: Design,
        tabs: usize,
        active: usize,
    }

    impl Render for StripHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let tabs = (0..self.tabs)
                .map(|index| {
                    tab(
                        ("harness-tab", index),
                        index,
                        format!("repository with a long name {index}"),
                        index == self.active,
                        &self.design,
                    )
                    .always_show_close(true)
                    .on_close(|_, _| {})
                })
                .collect::<Vec<_>>();
            div()
                .debug_selector(|| "harness-root".to_string())
                .size_full()
                .child(tab_strip("harness-strip", &self.design, &self.scroll).children(tabs))
        }
    }

    #[gpui::test]
    fn the_strip_shrinks_tabs_and_reveals_the_active_one(cx: &mut gpui::TestAppContext) {
        use gpui::{Bounds, WindowBounds, WindowOptions, point, size};

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
        });

        let harness_init = std::rc::Rc::new(std::cell::RefCell::new(None));
        let harness_in_window = harness_init.clone();
        let window = cx
            .update(|cx| {
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(0.0), px(0.0)),
                            size(px(480.0), px(200.0)),
                        ))),
                        ..Default::default()
                    },
                    move |_window, cx| {
                        let design = Design::new(cx.theme(), Settings::get(cx));
                        let entity = cx.new(|_| StripHarness {
                            scroll: ScrollHandle::default(),
                            design,
                            tabs: 16,
                            active: 15,
                        });
                        *harness_in_window.borrow_mut() = Some(entity.clone());
                        entity
                    },
                )
            })
            .expect("open window");
        let mut visual_cx = gpui::VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();
        // The first frame lays the strip out; revealing afterwards is the same
        // path a tab switch or a newly opened tab takes.
        visual_cx.update(|_, cx| {
            let harness = harness_init.borrow().clone().expect("harness");
            let handle = harness.read(cx).scroll.clone();
            reveal_tab(&handle, 15);
        });
        visual_cx.run_until_parked();
        visual_cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });

        let tab_bounds = visual_cx
            .debug_bounds("ui-tab-tab-15")
            .expect("the active tab is painted");
        assert!(
            tab_bounds.size.width.as_f32() >= 80.0,
            "a tab stops at its minimum width before the strip scrolls: {tab_bounds:?}"
        );
        let close = visual_cx
            .debug_bounds("ui-tab-close-15")
            .expect("the active tab's close button is painted");
        let root = visual_cx
            .debug_bounds("harness-root")
            .expect("the harness root is painted");
        let scroll_state = visual_cx.update(|_, cx| {
            let harness = harness_init.borrow().clone().expect("harness");
            let handle = &harness.read(cx).scroll;
            (
                f32::from(handle.offset().x),
                f32::from(handle.max_offset().x),
                handle.children_count(),
            )
        });
        assert!(
            close.right() <= root.right() && close.left() >= root.left(),
            "the active tab's close button has to stay inside the strip: {close:?} in {root:?}, scroll {scroll_state:?}"
        );
    }
}
