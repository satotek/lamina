//! Points gpui-component's global [`Theme`] at the app's [`Design`] palette.
//!
//! Everything the app paints itself reads [`Design`] directly, but the parts it
//! borrows from gpui-component — the settings pages, title bars, inputs, lists,
//! selects, switches, popovers, menus, scrollbars and dialogs — paint from the
//! global theme. Left alone, those parts keep the library's default grey
//! palette and the settings window and the picker stop looking like the main
//! window. Writing the tokens is the supported way to change them.
//!
//! [`Theme::update`] refreshes every window, so [`sync`] first compares the
//! tokens it would write with the ones in place and returns when they already
//! match. A write is expensive enough that a repeat call in the same frame —
//! several windows apply the theme as they open — must cost only the
//! comparison.

use gpui::{App, Hsla};
use gpui_component::{ActiveTheme as _, Theme};

use crate::settings::Settings;
use crate::style::Design;

/// Writes the [`Design`] palette onto gpui-component's theme tokens.
pub fn sync(cx: &mut App) {
    let design = Design::new(cx.theme(), Settings::get(cx));
    let wanted = Palette::from_design(&design);
    if Palette::from_theme(cx.theme()) == wanted {
        return;
    }
    Theme::update(cx, |theme| wanted.write(theme));
}

/// Defines the tokens this module owns once, so the comparison and the write
/// cannot drift apart.
///
/// The design and the two button palettes are passed in by name: macro hygiene
/// would otherwise keep the values written at the call site from seeing the
/// bindings the generated code defines.
macro_rules! palette {
    ($design:ident, $primary:ident, $danger:ident; $($field:ident: $value:expr),+ $(,)?) => {
        /// One value per gpui-component theme token this module writes.
        #[derive(Clone, Copy, PartialEq, Debug)]
        struct Palette {
            $($field: Hsla,)+
        }

        impl Palette {
            fn from_design($design: &Design) -> Self {
                let $primary = $design.primary_button();
                let $danger = $design.danger_button();
                Self {
                    $($field: $value,)+
                }
            }

            fn from_theme(theme: &Theme) -> Self {
                Self {
                    $($field: theme.colors.$field,)+
                }
            }

            fn write(self, theme: &mut Theme) {
                $(theme.colors.$field = self.$field;)+
            }
        }
    };
}

palette! {
    design, primary, danger;
    // Surfaces and text.
    background: design.background,
    foreground: design.text,
    border: design.border,
    muted: design.field,
    muted_foreground: design.muted,
    accent: design.selected,
    accent_foreground: design.text,
    input: design.border,
    ring: design.accent,
    caret: design.accent,
    selection: design.selected,
    popover: design.panel,
    popover_foreground: design.text,
    group_box: design.panel,
    group_box_foreground: design.text,
    accordion: design.panel,
    skeleton: design.field,
    window_border: design.border,

    // Links.
    link: design.accent_text,
    link_hover: design.accent,
    link_active: design.accent_text,

    // Secondary controls: the app's subtle raised surface.
    secondary: design.field,
    secondary_hover: design.selected,
    secondary_active: design.border,
    secondary_foreground: design.text,
    button_secondary: design.field,
    button_secondary_hover: design.selected,
    button_secondary_active: design.border,
    button_secondary_foreground: design.text,

    // Filled buttons, from the same [`ButtonColors`](crate::style::ButtonColors)
    // the app's own buttons use.
    primary: primary.background,
    primary_hover: primary.hover,
    primary_active: primary.active,
    primary_foreground: primary.foreground,
    button_primary: primary.background,
    button_primary_hover: primary.hover,
    button_primary_active: primary.active,
    button_primary_foreground: primary.foreground,
    danger: danger.background,
    danger_hover: danger.hover,
    danger_active: danger.active,
    danger_foreground: danger.foreground,
    button_danger: danger.background,
    button_danger_hover: danger.hover,
    button_danger_active: danger.active,
    button_danger_foreground: danger.foreground,

    // Lists: the same fills [`crate::ui::list_row`] uses for its rows.
    list: design.background,
    list_active: design.row_selected,
    list_active_border: design.accent,
    list_even: design.panel,
    list_head: design.chrome,
    list_hover: design.field,

    // The settings sidebar and the app's sidebar are the same surface.
    sidebar: design.sidebar,
    sidebar_foreground: design.text,
    sidebar_accent: design.selected,
    sidebar_accent_foreground: design.text,
    sidebar_border: design.border,
    sidebar_primary: design.accent,
    sidebar_primary_foreground: design.on_accent,

    // Window chrome.
    title_bar: design.chrome,
    title_bar_border: design.border,
    status_bar: design.chrome,
    status_bar_border: design.border,

    // Scrollbars, switches and tabs.
    scrollbar: design.background,
    scrollbar_thumb: design.border,
    scrollbar_thumb_hover: design.muted,
    switch: design.border,
    switch_thumb: design.background,
    tab: design.chrome,
    tab_active: design.background,
    tab_foreground: design.muted,
    tab_active_foreground: design.text,
    tab_bar: design.chrome,
    tab_bar_segmented: design.field,
    slider_bar: design.field,
    slider_thumb: design.accent,
    progress_bar: design.field,
    drag_border: design.accent,
    drop_target: design.selected,
    description_list_label: design.panel,
    description_list_label_foreground: design.muted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::ThemeMode;

    fn prepare(cx: &mut App) {
        gpui_component::init(cx);
        cx.set_global(Settings::default());
    }

    #[gpui::test]
    fn the_component_theme_follows_the_design_in_both_modes(cx: &mut TestAppContext) {
        cx.update(prepare);

        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            cx.update(|cx| {
                Theme::change(mode, None, cx);
                sync(cx);
            });

            cx.update(|cx| {
                let design = Design::new(cx.theme(), Settings::get(cx));
                let theme = cx.theme();
                assert_eq!(theme.background, design.background, "{mode:?}");
                assert_eq!(theme.foreground, design.text, "{mode:?}");
                assert_eq!(theme.sidebar, design.sidebar, "{mode:?}");
                assert_eq!(theme.title_bar, design.chrome, "{mode:?}");
                assert_eq!(theme.popover, design.panel, "{mode:?}");
                assert_eq!(theme.input, design.border, "{mode:?}");
                assert_eq!(theme.list_active, design.row_selected, "{mode:?}");
                assert_eq!(theme.muted_foreground, design.muted, "{mode:?}");
                assert_eq!(
                    theme.button_primary,
                    design.primary_button().background,
                    "{mode:?}"
                );
                assert_eq!(
                    theme.button_danger,
                    design.danger_button().background,
                    "{mode:?}"
                );
                assert_eq!(
                    Palette::from_theme(theme),
                    Palette::from_design(&design),
                    "every token is written, not only the sampled ones"
                );
            });
        }
    }

    #[gpui::test]
    fn a_synced_palette_is_left_alone(cx: &mut TestAppContext) {
        cx.update(prepare);
        cx.update(|cx| {
            Theme::change(ThemeMode::Light, None, cx);
            sync(cx);
        });

        // A second run must find the theme already in sync. It is observable
        // through the write itself: replacing one token by hand and syncing
        // restores it, so the comparison is reading the live values.
        cx.update(|cx| {
            let wanted = Palette::from_design(&Design::new(cx.theme(), Settings::get(cx)));
            assert_eq!(Palette::from_theme(cx.theme()), wanted);

            Theme::update(cx, |theme| theme.background = gpui::black());
            assert_ne!(Palette::from_theme(cx.theme()), wanted);

            sync(cx);
            assert_eq!(Palette::from_theme(cx.theme()), wanted);
        });
    }
}
