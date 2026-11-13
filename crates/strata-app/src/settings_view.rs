//! The settings window: one page per section of [`Settings`], opened with
//! `Ctrl+,` (`Cmd+,` on macOS).
//!
//! It edits the same [`Settings`] global the rest of the app reads, so a change
//! is stored, saved in the background and visible in every window on the next
//! frame; there is no Save button. The pages themselves are built in
//! [`pages`], and the value rules each control applies live in [`change`].

mod change;
mod pages;

use std::path::Path;

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, FocusHandle, Global,
    InteractiveElement as _, IntoElement, ParentElement, Render, Styled, TitlebarOptions, Window,
    WindowBounds, WindowDecorations, WindowOptions, div, px, size,
};
use gpui_component::Disableable as _;
use gpui_component::button::Button;
use gpui_component::setting::Settings as SettingsPages;
use gpui_component::{ActiveTheme as _, Sizable as _, TitleBar, h_flex, v_flex};

use crate::actions::OpenSettings;
use crate::settings;

/// Size the window opens at, and the smallest it can be dragged to.
const WIDTH: f32 = 820.0;
const HEIGHT: f32 = 560.0;
const MIN_WIDTH: f32 = 560.0;
const MIN_HEIGHT: f32 = 420.0;

/// Registers the window and the action that opens it. Call once at startup.
pub fn bind(cx: &mut App) {
    cx.set_global(OpenSettingsWindow::default());
    cx.on_action(|_: &OpenSettings, cx: &mut App| open(cx));
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

    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // The same client-side title bar as the main window, so both read
            // as one app on every platform.
            titlebar: Some(TitlebarOptions {
                title: Some("Strata Settings".into()),
                ..TitleBar::title_bar_options()
            }),
            window_decorations: Some(WindowDecorations::Client),
            window_min_size: Some(size(px(MIN_WIDTH), px(MIN_HEIGHT))),
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
            cx.global_mut::<OpenSettingsWindow>().0 = Some(handle.into());
            cx.activate(true);
        }
        Err(error) => log::warn!("cannot open the settings window: {error}"),
    }
}

struct SettingsView {
    focus: FocusHandle,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self { focus }
    }

    fn footer(&self, cx: &Context<Self>) -> impl IntoElement {
        let path = settings::settings_file();
        let label = path.as_ref().map_or_else(
            || "No settings file: the configuration directory is unknown".to_owned(),
            |path| path.display().to_string(),
        );

        h_flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap_3()
            .h(px(34.0))
            .px_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().title_bar)
            .text_size(px(11.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(
                Button::new("open-settings-folder")
                    .xsmall()
                    .outline()
                    .label("Open folder")
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_family(cx.theme().font_family.clone())
                                .child("Strata"),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(11.0))
                                .text_color(cx.theme().muted_foreground)
                                .child("Settings"),
                        ),
                ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    SettingsPages::new("settings")
                        .sidebar_width(px(180.0))
                        .pages(pages::pages()),
                ),
            )
            .child(self.footer(cx))
    }
}
