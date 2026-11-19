//! The settings dialog: one page per section of [`Settings`] plus the About
//! page, over the window it was opened from (`Ctrl+,`, `Cmd+,` on macOS).
//!
//! It edits the same [`Settings`] global the rest of the app reads, so a change
//! is stored, saved in the background and visible behind the dialog on the
//! same frame; there is no Save button. Being a dialog rather than a window of
//! its own is what lets a theme or zoom change be judged against the real
//! window, and it leaves nothing to raise, track or lose behind another window.
//!
//! The pages are built in [`pages`] from the parts in [`kit`], and the value
//! rules each control applies live in [`change`].

mod change;
mod kit;
mod pages;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight, Global,
    InteractiveElement as _, IntoElement, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, WeakEntity, Window, actions, div, px,
};
use gpui_component::dialog::DialogEntrance;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit_assets::IconName;

use crate::about::AboutView;
use crate::actions::OpenSettings;
use crate::settings::Settings;
use crate::style::{self, Design};

pub(crate) use change::Section;
// The About page hosts the update switch, but it writes through the same
// value rule the Behaviour page's controls use.
pub(crate) use change::auto_check_updates;

/// Key context of the dialog, so its close shortcuts do not fire in the views
/// underneath.
const SETTINGS_VIEW: &str = "SettingsView";

actions!(strata, [CloseSettings]);

/// The largest the dialog grows, and the margin it keeps from the window's
/// edges when the window is smaller than that.
const MAX_WIDTH: f32 = 880.0;
const MAX_HEIGHT: f32 = 640.0;
const EDGE: f32 = 14.0;

/// Below this dialog width the page list shows only icons.
const NARROW: f32 = 640.0;

const SIDEBAR_WIDTH: f32 = 196.0;
const SIDEBAR_NARROW: f32 = 48.0;
const CONTENT_PADDING_X: f32 = 28.0;
const CONTENT_PADDING_Y: f32 = 20.0;

/// What the dialog shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    Section(Section),
    About,
}

/// The page the dialog was last left on, so `Cmd+,` returns to it, and the
/// dialog that is open, so a second request switches its page instead of
/// stacking another.
#[derive(Default)]
struct DialogState {
    last: Option<Destination>,
    open: Option<WeakEntity<SettingsDialog>>,
}

impl Global for DialogState {}

/// Registers the action that opens the dialog. Call once at startup.
pub fn bind(cx: &mut App) {
    cx.set_global(DialogState::default());
    cx.on_action(|_: &OpenSettings, cx: &mut App| show(None, cx));
    crate::keymap::bind_group(cx, crate::keymap::Group::Settings);
}

/// Opens the dialog over the active window, on `destination` or on the page it
/// was last left on.
pub(crate) fn show(destination: Option<Destination>, cx: &mut App) {
    let Some(handle) = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())
    else {
        return;
    };
    // The request arrives from a menu, a keystroke or a click, all dispatched
    // inside that window's own update, so the window cannot be borrowed again
    // until it ends.
    cx.defer(move |cx| {
        let _ = handle.update(cx, |_, window, cx| open(destination, window, cx));
    });
}

fn open(destination: Option<Destination>, window: &mut Window, cx: &mut App) {
    if let Some(dialog) = cx
        .try_global::<DialogState>()
        .and_then(|state| state.open.as_ref())
        .and_then(WeakEntity::upgrade)
    {
        if let Some(destination) = destination {
            dialog.update(cx, |dialog, cx| dialog.go(destination, cx));
        }
        return;
    }
    // Another modal already has the keyboard; opening over it would close it.
    if window.has_active_dialog(cx) {
        return;
    }

    // This is where the user looks after installing a program or dropping a
    // theme file in, and both probes keep their previous answer until the new
    // one arrives.
    crate::tools::redetect(cx);
    crate::style::theme::reload(cx);

    let destination = destination
        .or_else(|| cx.try_global::<DialogState>().and_then(|state| state.last))
        .unwrap_or(Destination::Section(Section::Appearance));
    let view = cx.new(|cx| SettingsDialog::new(destination, window, cx));
    if cx.has_global::<DialogState>() {
        cx.global_mut::<DialogState>().open = Some(view.downgrade());
    }
    let focus = view.read(cx).focus.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        let content = view.clone();
        let (width, height) = dialog_size(window, cx);
        let top = ((f32::from(window.viewport_size().height) - height) / 2.0).max(EDGE);
        dialog
            .entrance(DialogEntrance::None)
            .p_0()
            .w(px(width))
            .h(px(height))
            .margin_top(px(top))
            .close_button(false)
            .on_ok(|_, _, _| false)
            .on_close(|_, _, cx| {
                if cx.has_global::<DialogState>() {
                    cx.global_mut::<DialogState>().open = None;
                }
            })
            .content(move |body, _, _| body.size_full().child(content.clone()))
    });
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

/// The dialog's size: its largest, or the window less a margin on each side.
fn dialog_size(window: &Window, cx: &App) -> (f32, f32) {
    let zoom = Settings::get(cx).appearance.zoom();
    let viewport = window.viewport_size();
    let width = (MAX_WIDTH * zoom).min(f32::from(viewport.width) - 2.0 * EDGE);
    let height = (MAX_HEIGHT * zoom).min(f32::from(viewport.height) - 2.0 * EDGE);
    (width.max(0.0), height.max(0.0))
}

struct SettingsDialog {
    focus: FocusHandle,
    current: Destination,
    search: Entity<InputState>,
    query: String,
    about: Entity<AboutView>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SettingsDialog {
    fn new(destination: Destination, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("settings.search_placeholder").to_string())
        });
        let subscriptions = vec![
            cx.subscribe(&search, |this, search, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query = search.read(cx).value().trim().to_lowercase();
                    this.scroll.set_offset(Default::default());
                    cx.notify();
                }
            }),
            // Another window can change a setting this dialog shows.
            cx.observe_global::<Settings>(|_, cx| cx.notify()),
        ];
        if cx.has_global::<DialogState>() {
            cx.global_mut::<DialogState>().last = Some(destination);
        }
        Self {
            focus: cx.focus_handle(),
            current: destination,
            search,
            query: String::new(),
            about: cx.new(AboutView::new),
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    fn go(&mut self, destination: Destination, cx: &mut Context<Self>) {
        self.current = destination;
        self.scroll.set_offset(Default::default());
        if cx.has_global::<DialogState>() {
            cx.global_mut::<DialogState>().last = Some(destination);
        }
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn nav_item(
        &self,
        id: SharedString,
        label: String,
        icon: AnyElement,
        destination: Destination,
        narrow: bool,
        design: &Design,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.query.is_empty() && self.current == destination;
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(design.px(9.0))
            .px(design.px(8.0))
            .py(design.px(6.0))
            .when(narrow, |this| this.justify_center())
            .rounded(design.px(6.0))
            .cursor_pointer()
            .text_color(if selected { design.text } else { design.muted })
            .when(selected, |this| this.bg(design.row_selected))
            .when(!selected, |this| {
                this.hover(|this| this.bg(design.selected))
            })
            .child(icon)
            .when(!narrow, |this| {
                this.child(div().truncate().child(label.clone()))
            })
            .when(narrow, |this| {
                this.tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(label.clone()).build(window, cx)
                })
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if !this.query.is_empty() {
                    this.search
                        .update(cx, |search, cx| search.set_value("", window, cx));
                    this.query.clear();
                }
                this.go(destination, cx);
            }))
            .into_any_element()
    }

    fn sidebar(&self, narrow: bool, design: &Design, cx: &mut Context<Self>) -> AnyElement {
        let icon = |name: IconName| Icon::new(name).size(design.px(15.0)).into_any_element();
        let mut list = v_flex()
            .flex_none()
            .h_full()
            .w(design.px(if narrow {
                SIDEBAR_NARROW
            } else {
                SIDEBAR_WIDTH
            }))
            .gap(design.px(2.0))
            .px(design.px(if narrow { 6.0 } else { 10.0 }))
            .py(design.px(14.0))
            .bg(design.sidebar)
            .border_r_1()
            .border_color(design.border);
        if !narrow {
            list = list
                .child(
                    div()
                        .px(design.px(8.0))
                        .pb(design.px(10.0))
                        .text_size(px(design.ui + design.scale(2.0)))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(t!("settings.window_name").to_string()),
                )
                .child(
                    div().pb(design.px(10.0)).child(
                        Input::new(&self.search)
                            .small()
                            .prefix(Icon::new(IconName::Search).small())
                            .cleanable(true),
                    ),
                );
        }
        for section in Section::ALL {
            list = list.child(self.nav_item(
                SharedString::from(format!("settings-nav-{}", section.id())),
                section.title(),
                icon(pages::icon(section)),
                Destination::Section(section),
                narrow,
                design,
                cx,
            ));
        }
        list.child(div().flex_1())
            .child(
                div()
                    .h(px(1.0))
                    .mx(design.px(4.0))
                    .my(design.px(6.0))
                    .bg(design.border),
            )
            .child(self.nav_item(
                SharedString::from("settings-nav-about"),
                t!("action.about_strata").to_string(),
                style::app_mark(design.scale(15.0)).into_any_element(),
                Destination::About,
                narrow,
                design,
                cx,
            ))
            .into_any_element()
    }

    fn title_row(
        &self,
        page: &kit::Page,
        design: &Design,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section = page.section;
        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_3()
            .pb(design.px(14.0))
            .child(
                h_flex()
                    .items_center()
                    .gap(design.px(6.0))
                    .child(
                        div()
                            .text_size(px(design.ui + design.scale(4.0)))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(section.title()),
                    )
                    .children(page.info_icon(design, window, cx)),
            )
            .when(page.is_resettable(), |this| {
                this.child(
                    crate::ui::button(
                        SharedString::from(format!("reset-{}", section.id())),
                        t!("settings.keyboard.reset_section").to_string(),
                    )
                    .size(crate::ui::ButtonSize::Small)
                    .ghost()
                    .on_click(move |_, _, cx| pages::reset(section, cx)),
                )
            })
            .into_any_element()
    }

    fn body(
        &self,
        content_width: f32,
        design: &Design,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.query.is_empty() {
            let query = self.query.clone();
            let mut results: Vec<AnyElement> = pages::pages(content_width, design, cx)
                .iter()
                .filter_map(|page| {
                    let body = page.render_body(Some(&query), design, window, cx)?;
                    Some(
                        v_flex()
                            .gap(design.px(10.0))
                            .child(
                                div()
                                    .text_size(px(design.ui + design.scale(2.0)))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(page.section.title()),
                            )
                            .child(body)
                            .into_any_element(),
                    )
                })
                .collect();
            if about_matches(&query) {
                results.push(self.about_result(design, cx));
            }
            if results.is_empty() {
                return div()
                    .pt(design.px(40.0))
                    .w_full()
                    .flex()
                    .justify_center()
                    .text_color(design.muted)
                    .child(t!("settings.no_results").to_string())
                    .into_any_element();
            }
            return v_flex()
                .w_full()
                .gap(design.px(32.0))
                .children(results)
                .into_any_element();
        }
        match self.current {
            Destination::About => self.about.clone().into_any_element(),
            Destination::Section(section) => {
                let pages = pages::pages(content_width, design, cx);
                let Some(page) = pages.iter().find(|page| page.section == section) else {
                    return div().into_any_element();
                };
                v_flex()
                    .w_full()
                    .child(self.title_row(page, design, window, cx))
                    .children(page.render_body(None, design, window, cx))
                    .into_any_element()
            }
        }
    }
}

impl SettingsDialog {
    /// The About page holds settings of its own (update checks), so a query
    /// for them leads there instead of finding nothing.
    fn about_result(&self, design: &Design, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap(design.px(10.0))
            .child(
                div()
                    .text_size(px(design.ui + design.scale(2.0)))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t!("action.about_strata").to_string()),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap(design.px(16.0))
                    .py(design.px(9.0))
                    .child(t!("about.updates").to_string())
                    .child(
                        crate::ui::button(
                            "settings-search-about",
                            t!("settings.open_page").to_string(),
                        )
                        .size(crate::ui::ButtonSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.search
                                .update(cx, |search, cx| search.set_value("", window, cx));
                            this.query.clear();
                            this.go(Destination::About, cx);
                        })),
                    ),
            )
            .into_any_element()
    }
}

/// Whether a search query is about something only the About page offers.
fn about_matches(query: &str) -> bool {
    [
        t!("about.updates"),
        t!("about.auto_check"),
        t!("about.auto_check_description"),
        t!("about.licenses"),
        t!("about.third_party"),
        t!("action.about_strata"),
    ]
    .iter()
    .any(|text| text.to_lowercase().contains(query))
}

impl Focusable for SettingsDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SettingsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let (width, _) = dialog_size(window, cx);
        let narrow = width < design.scale(NARROW);
        let sidebar = design.scale(if narrow {
            SIDEBAR_NARROW
        } else {
            SIDEBAR_WIDTH
        });
        let padding_x = design.scale(if narrow { 18.0 } else { CONTENT_PADDING_X });
        let content_width = (width - sidebar - 2.0 * padding_x).max(0.0);

        let search_on_top = narrow.then(|| {
            div().pb(design.px(12.0)).child(
                Input::new(&self.search)
                    .small()
                    .prefix(Icon::new(IconName::Search).small())
                    .cleanable(true),
            )
        });
        let body = self.body(content_width, &design, window, cx);

        h_flex()
            .size_full()
            .key_context(SETTINGS_VIEW)
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &CloseSettings, window, cx| window.close_dialog(cx)))
            .overflow_hidden()
            .rounded(design.px(10.0))
            .bg(design.background)
            .text_color(design.text)
            .text_size(px(design.ui))
            .child(self.sidebar(narrow, &design, cx))
            .child(
                div().flex_1().min_w_0().h_full().child(
                    div()
                        .id("settings-content")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .child(
                            v_flex()
                                .w_full()
                                .px(px(padding_x))
                                .py(design.px(CONTENT_PADDING_Y))
                                .children(search_on_top)
                                .child(body),
                        ),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::VisualTestContext;

    struct Probe;

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    fn open_window(cx: &mut gpui::TestAppContext) -> VisualTestContext {
        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
            // Opening runs the tool and theme probes the dialog shows.
            cx.set_global(crate::tools::Tooling::default());
            crate::style::theme::init(cx);
            bind(cx);
            crate::about::bind(cx);
        });
        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    let view = cx.new(|_| Probe);
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                })
            })
            .expect("open a window");
        window
            .update(cx, |_, window, _| window.activate_window())
            .expect("activate the window");
        let visual_cx = VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();
        visual_cx
    }

    fn current(cx: &mut VisualTestContext) -> Option<Destination> {
        cx.update(|_, cx| {
            cx.global::<DialogState>()
                .open
                .as_ref()
                .and_then(WeakEntity::upgrade)
                .map(|dialog| dialog.read(cx).current)
        })
    }

    #[gpui::test]
    fn the_settings_action_opens_the_dialog_on_the_last_page(cx: &mut gpui::TestAppContext) {
        let mut cx = open_window(cx);
        cx.dispatch_action(OpenSettings);
        cx.run_until_parked();
        assert_eq!(
            current(&mut cx),
            Some(Destination::Section(Section::Appearance))
        );
        assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
    }

    #[gpui::test]
    fn about_opens_the_same_dialog_on_its_page(cx: &mut gpui::TestAppContext) {
        let mut cx = open_window(cx);
        cx.dispatch_action(OpenSettings);
        cx.run_until_parked();

        // A second request switches the open dialog instead of stacking one.
        cx.dispatch_action(crate::menus::AboutStrata);
        cx.run_until_parked();
        assert_eq!(current(&mut cx), Some(Destination::About));
    }
}
