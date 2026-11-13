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
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::tooltip::Tooltip;

use crate::settings::Settings;
use crate::style::Design;

/// Height of the repository tab strip when two or more tabs are open. Every
/// tab is the same height and runs the full strip, so the active one reaches
/// the content below.
pub const TAB_STRIP_HEIGHT: f32 = 32.0;

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
            .rounded(px(6.0))
            .bg(design.panel)
            .border_1()
            .border_color(design.accent)
            .text_size(px(design.small() + 0.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(design.text)
            .shadow_md()
            .child(self.label.clone())
    }
}

/// The horizontal container sitting directly below the title bar when
/// multiple repository tabs are open.
pub fn tab_strip(id: impl Into<ElementId>, design: &Design) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .flex()
        .flex_none()
        .items_end()
        .h(px(TAB_STRIP_HEIGHT))
        .w_full()
        .overflow_x_scroll()
        .px_2()
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

#[derive(Clone, Copy)]
struct TabColors {
    background: Hsla,
    field: Hsla,
    border: Hsla,
    text: Hsla,
    muted: Hsla,
    accent: Hsla,
    font_size: f32,
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
            font_size: design.small() + 0.5,
        }
    }
}

type SelectHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type CloseHandler = Rc<dyn Fn(&mut Window, &mut App)>;
type ReorderHandler = Rc<dyn Fn(usize, usize, &mut Window, &mut App)>;

/// One repository tab inside [`tab_strip`].
#[derive(IntoElement)]
pub struct UiTab {
    id: ElementId,
    index: usize,
    label: SharedString,
    active: bool,
    busy: bool,
    separator: bool,
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
        busy: false,
        separator: false,
        tooltip: None,
        hint: None,
        on_select: None,
        on_close: None,
        on_reorder: None,
        colors: TabColors::from(design),
    }
}

impl UiTab {
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
        let group: SharedString = format!("ui-tab-group-{index}").into();
        let label = self.label.clone();

        // The transparent top border reserves the accent line's 2 px so the
        // label does not move when the tab becomes active.
        let mut tab = div()
            .id(self.id)
            .group(group.clone())
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .h(px(TAB_STRIP_HEIGHT))
            .min_w(px(96.0))
            .max_w(px(220.0))
            .px_3()
            .border_t(px(2.0))
            .border_color(Hsla::transparent_black())
            .cursor_pointer()
            .text_size(px(colors.font_size));

        tab = if self.active {
            tab.bg(colors.background)
                .border_color(colors.accent)
                .text_color(colors.text)
                .font_weight(FontWeight::MEDIUM)
        } else {
            tab.text_color(colors.muted)
                .font_weight(FontWeight::NORMAL)
                .hover(move |style| style.bg(colors.field).text_color(colors.text))
        };

        if self.separator {
            tab = tab.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(8.0))
                    .bottom(px(8.0))
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
                    .id(("ui-tab-busy", index))
                    .flex_none()
                    .w(px(6.0))
                    .h(px(6.0))
                    .rounded_full()
                    .bg(colors.accent),
            );
        }

        if let Some(on_close) = self.on_close {
            let close = div()
                .id(("ui-tab-close", index))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(18.0))
                .h(px(18.0))
                .rounded(px(4.0))
                .text_size(px(colors.font_size + 1.0))
                .text_color(colors.muted)
                .when(!self.active, |close| {
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
        .gap(px(2.0))
        .max_w(px(360.0))
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
                    .text_size(px(colors.font_size - 0.5))
                    .text_color(colors.muted)
                    .child(hint),
            )
        })
}

/// The tip's path line: `~/src/strata` for a path under the home directory,
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
    const MAX_CHARS: usize = 48;
    if folded.chars().count() <= MAX_CHARS {
        return folded;
    }
    let tail = path
        .components()
        .rev()
        .take(2)
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let tail = tail.into_iter().rev().collect::<Vec<_>>().join("/");
    format!("…/{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_hint_folds_home_and_shortens_long_paths() {
        let home = Path::new("/home/mio");
        assert_eq!(
            path_hint_in(Path::new("/home/mio/src/strata"), Some(home)),
            "~/src/strata"
        );
        assert_eq!(
            path_hint_in(Path::new("/srv/strata"), Some(home)),
            "/srv/strata",
            "a path outside home keeps its full form"
        );
        assert_eq!(path_hint_in(Path::new("/home/mio"), Some(home)), "~");

        let long = Path::new("/home/mio/one/two/three/four/five/six/seven/eight/nine/strata");
        assert_eq!(path_hint_in(long, Some(home)), "…/nine/strata");
    }

    #[test]
    fn a_short_path_keeps_every_part() {
        let home = Path::new("/home/mio");
        let path = Path::new("/home/mio/nine/strata");
        assert_eq!(
            path_hint_in(path, Some(home)),
            "~/nine/strata",
            "a hint that already fits is not cut"
        );
    }
}
