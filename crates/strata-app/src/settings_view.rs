//! The settings window: one page per section of [`Settings`], opened with
//! `Ctrl+,` (`Cmd+,` on macOS).
//!
//! It edits the same [`Settings`] global the rest of the app reads, so a change
//! is stored, saved in the background and visible in every window on the next
//! frame; there is no Save button. The pages themselves are built in
//! [`pages`], and the value rules each control applies live in [`change`].
//!
//! The frame follows the main window: the same 44 px chrome title bar with the
//! app mark, the same [`Design`] sidebar surface behind the page list, and the
//! same selected-row tint. The page widget is gpui-component's, so the rest of
//! the palette arrives through [`crate::component_theme`].

mod change;
mod pages;

use std::path::Path;

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, FocusHandle, FontWeight, Global,
    InteractiveElement as _, IntoElement, ParentElement, Render, StyleRefinement, Styled,
    Subscription, Window, WindowBounds, WindowDecorations, WindowOptions, actions, div, px,
};
use gpui_component::group_box::GroupBoxVariant;
use gpui_component::setting::Settings as SettingsPages;
use gpui_component::{ActiveTheme as _, TitleBar, h_flex, v_flex};

use crate::actions::OpenSettings;
use crate::menus::AboutStrata;
use crate::settings::{self, Appearance, Settings};
use crate::style::{self, Design};

/// Key context of the settings window, so its close shortcuts do not fire in
/// the main window's views.
const SETTINGS_VIEW: &str = "SettingsView";

actions!(strata, [CloseSettings]);

/// Size the window opens at, and the smallest it can be dragged to.
const WIDTH: f32 = 900.0;
const HEIGHT: f32 = 640.0;
const MIN_WIDTH: f32 = 640.0;
const MIN_HEIGHT: f32 = 460.0;

/// The page list's width, and how far the divider can be dragged either way.
const SIDEBAR_WIDTH: f32 = 200.0;
const SIDEBAR_MIN: f32 = 176.0;
const SIDEBAR_MAX: f32 = 280.0;

/// Registers the window and the action that opens it. Call once at startup.
pub fn bind(cx: &mut App) {
    cx.set_global(OpenSettingsWindow::default());
    cx.on_action(|_: &OpenSettings, cx: &mut App| open(cx));
    crate::keymap::bind_group(cx, crate::keymap::Group::Settings);
}

/// Which window is showing the settings, so the shortcut raises it instead of
/// opening a second one. A closed window leaves a stale handle behind, which
/// [`open`] detects by trying to use it.
#[derive(Default)]
struct OpenSettingsWindow(Option<AnyWindowHandle>);

impl Global for OpenSettingsWindow {}

fn open(cx: &mut App) {
    if let Some(handle) = cx.global::<OpenSettingsWindow>().0
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }

    // This window is where the user looks after installing a program, and the
    // detection thread keeps the previous answer until the new one arrives.
    crate::tools::redetect(cx);
    crate::style::theme::reload(cx);

    let bounds = Bounds::centered(None, crate::zoom::window_size(WIDTH, HEIGHT, cx), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // The same client-side title bar as the main window, so both read
            // as one app on every platform.
            titlebar: Some(crate::style::window_titlebar(
                t!("app.settings_title").to_string(),
            )),
            window_decorations: Some(WindowDecorations::Client),
            window_min_size: Some(crate::zoom::window_size(MIN_WIDTH, MIN_HEIGHT, cx)),
            focus: true,
            ..TitleBar::window_options()
        },
        |window, cx| {
            settings::apply_theme(Some(window), cx);
            let view = cx.new(|cx| SettingsView::new(window, cx));
            // `Root` hosts gpui-component's popup, dialog and tooltip layers,
            // which every dropdown on the pages needs.
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        },
    );

    match opened {
        Ok(handle) => {
            crate::window_registry::track(handle.window_id(), cx);
            cx.global_mut::<OpenSettingsWindow>().0 = Some(handle.into());
            cx.activate(true);
        }
        Err(error) => log::warn!("cannot open the settings window: {error}"),
    }
}

struct SettingsView {
    focus: FocusHandle,
    /// The appearance the theme was last applied for, so an unrelated settings
    /// change does not reload the palette.
    appearance: Appearance,
    /// `System` follows the OS; this window has no other reason to hear about
    /// an appearance change, so it keeps the subscription alive.
    _appearance: Subscription,
    /// Another window can change the theme (this one included). Observing the
    /// settings is what repaints this window against the new palette; the
    /// global theme refresh alone left it light until the next settings change.
    _settings: Subscription,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let appearance =
            window.observe_window_appearance(|window, cx| settings::apply_theme(Some(window), cx));
        let current_appearance = Settings::get(cx).appearance.clone();
        let settings = cx.observe_global::<Settings>(|this, cx| {
            let next = Settings::get(cx).appearance.clone();
            if next != this.appearance {
                this.appearance = next;
                settings::apply_theme(None, cx);
            }
            cx.notify();
        });
        Self {
            focus,
            appearance: current_appearance,
            _appearance: appearance,
            _settings: settings,
        }
    }

    /// The window's title bar, built like the main window's: the app mark and
    /// wordmark, a divider, then the name of the window.
    fn title_bar(&self, design: &Design) -> impl IntoElement {
        TitleBar::new()
            .h(design.px(crate::style::TITLE_BAR_HEIGHT))
            .bg(design.chrome)
            .border_color(design.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_none()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(design.px(7.0))
                            .child(style::app_mark(design.scale(style::BRAND_MARK)))
                            .child(style::wordmark(
                                design.scale(style::BRAND_WORDMARK),
                                design.text,
                            )),
                    )
                    .child(div().w(px(1.0)).h(design.px(18.0)).mx_1().bg(design.border))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("settings.window_name").to_string()),
                    ),
            )
    }

    fn footer(&self, design: &Design) -> impl IntoElement {
        let path = settings::settings_file();
        let label = path.as_ref().map_or_else(
            || t!("settings.file_unknown").to_string(),
            |path| path.display().to_string(),
        );

        h_flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap_3()
            .h(design.px(40.0))
            .px_3()
            .border_t_1()
            .border_color(design.border)
            .bg(design.chrome)
            .text_size(px(design.tiny()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(design.muted)
                    .child(label),
            )
            // Off macOS the menu bar is never drawn, so the Help menu's About
            // entry is unreachable; this is the one place every platform shows.
            .child(
                crate::ui::button("open-about", t!("action.about_strata").to_string())
                    .size(crate::ui::ButtonSize::Tiny)
                    .on_click(|_, window, cx| open_about(window, cx)),
            )
            .child(
                crate::ui::button(
                    "open-settings-folder",
                    t!("settings.open_folder").to_string(),
                )
                .size(crate::ui::ButtonSize::Tiny)
                .disabled(path.is_none())
                .on_click(move |_, _, cx| {
                    if let Some(folder) =
                        settings::settings_file().as_deref().and_then(Path::parent)
                    {
                        cx.open_with_system(folder);
                    }
                }),
            )
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        // `Sidebar` clears its own border and reads its colours from the theme
        // tokens, so the style set here is the one that makes the page list the
        // same surface as the main window's sidebar.
        let sidebar_style = StyleRefinement::default()
            .bg(design.sidebar)
            .border_r_1()
            .border_color(design.border);

        let mut root = v_flex()
            .size_full()
            .key_context(SETTINGS_VIEW)
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &CloseSettings, window, _| window.remove_window()))
            .bg(design.background)
            .text_color(design.text)
            .text_size(px(design.ui))
            .child(self.title_bar(&design))
            .child(
                div().flex_1().min_h_0().child(
                    SettingsPages::new("settings")
                        .sidebar_width(design.px(SIDEBAR_WIDTH))
                        .sidebar_size_range(design.px(SIDEBAR_MIN)..design.px(SIDEBAR_MAX))
                        .sidebar_style(&sidebar_style)
                        // Each group is a bordered card on the page surface,
                        // with its heading and description above it.
                        .with_group_variant(GroupBoxVariant::Outline)
                        .pages(pages::pages(window.viewport_size().width.into(), &design)),
                ),
            )
            .child(self.footer(&design));
        // This window is a root of its own, so the CJK chain has to be set
        // here as well as on the main window's.
        root.text_style().font_fallbacks = Some(style::font_fallbacks());
        root
    }
}

/// Dispatches About through this window. `App::dispatch_action` would route
/// it to the active window, which is this one, and GPUI takes a window out of
/// the app while it runs that window's click handler, so the action was
/// dropped with only a log line.
fn open_about(window: &mut Window, cx: &mut App) {
    window.dispatch_action(Box::new(AboutStrata), cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ThemeMode;
    use gpui::{BorrowAppContext as _, VisualTestContext};

    #[gpui::test]
    fn the_settings_window_applies_a_theme_change(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
        });

        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    let view = cx.new(|cx| SettingsView::new(window, cx));
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                })
            })
            .expect("open the settings window");
        let mut visual_cx = VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();

        // The theme field writes the setting and the window's observer applies
        // it. Updating only the global is what a change made in another window
        // looks like, and this window has to turn dark without another edit.
        visual_cx.update(|_, cx| {
            cx.update_global::<Settings, _>(|settings, _| {
                settings.appearance.theme = ThemeMode::Dark;
            });
        });
        visual_cx.run_until_parked();

        assert!(
            visual_cx.update(|_, cx| cx.theme().is_dark()),
            "the settings window has to repaint against the new theme"
        );
    }

    #[gpui::test]
    fn the_about_button_opens_the_about_window(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
            crate::about::bind(cx);
        });

        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), |window, cx| {
                    let view = cx.new(|cx| SettingsView::new(window, cx));
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                })
            })
            .expect("open the settings window");
        window
            .update(cx, |_, window, _| window.activate_window())
            .expect("activate the settings window");
        let mut visual_cx = VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();

        // A click handler runs inside an update of its own window, which is
        // exactly where the button calls this.
        visual_cx.update(open_about);
        visual_cx.run_until_parked();

        assert!(visual_cx.update(|_, cx| crate::about::is_open(cx)));
    }
}
