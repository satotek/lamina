//! The keyboard shortcut sheet: every action, its current keys, one dialog.
//!
//! The sheet is a view of the registry for the same reason the menu bar is: it
//! reads [`crate::keymap::ACTIONS`], the action's localized title, and the keys
//! the live keymap answers to — defaults the user kept plus their own entries,
//! without the ones they removed. Nothing in the sheet is written by hand, so a
//! new action or a rebinding cannot leave it stale.
//!
//! It opens from the View and Help menus and from `secondary-/` in every
//! window, as a modal over the active one. The filter field narrows by title or
//! by the keys themselves; `Esc` closes it.

use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, actions, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogEntrance;
use gpui_component::input::{Input, InputEvent, InputState};

use crate::keymap::{self, GROUP_ORDER, Group, KeymapState};
use crate::settings::Settings;
use crate::style::Design;
use crate::ui;

/// Key context of the sheet's card. Its `Esc` binding only applies while the
/// field below it has the keyboard.
pub const DIALOG: &str = "ShortcutsDialog";
/// The card's width; the list scrolls inside it.
const WIDTH: f32 = 660.0;
/// How tall the list may grow before it scrolls.
const LIST_HEIGHT: f32 = 520.0;

actions!(
    strata,
    [
        /// Opens the keyboard shortcut sheet.
        ShowKeyboardShortcuts,
        /// Closes the keyboard shortcut sheet.
        DismissShortcuts,
    ]
);

/// Registers the sheet's keys and the action that opens it. Call once at
/// startup.
pub fn bind(cx: &mut App) {
    keymap::bind_group(cx, Group::Shortcuts);
    cx.on_action(|_: &ShowKeyboardShortcuts, cx: &mut App| {
        let Some(handle) = cx.active_window() else {
            return;
        };
        // The keystroke is dispatched inside that window's own update, so the
        // window cannot be borrowed again here; open once it is back in the app.
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| open(window, cx));
        });
    });
}

struct Sheet {
    focus: FocusHandle,
    input: Entity<InputState>,
    _input_events: Subscription,
}

impl Sheet {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx
            .new(|cx| InputState::new(window, cx).placeholder(t!("shortcuts.filter").to_string()));
        let focus = input.focus_handle(cx);
        let _input_events = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            focus,
            input,
            _input_events,
        }
    }
}

/// One action's row: its title, then the caps of every key that runs it.
fn row(title: &str, caps: &[Vec<String>], design: &Design) -> impl IntoElement {
    let keys = if caps.is_empty() {
        div()
            .flex_none()
            .text_size(px(design.tiny()))
            .text_color(design.muted)
            .child(t!("shortcuts.no_keys").to_string())
            .into_any_element()
    } else {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(design.px(8.0))
            .children(caps.iter().map(|shortcut| {
                ui::keycaps(
                    shortcut
                        .iter()
                        .map(|cap| SharedString::from(cap.clone()))
                        .collect(),
                    design,
                )
            }))
            .into_any_element()
    };
    div()
        .flex()
        .items_center()
        .gap_3()
        .px_2()
        .py_1()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(design.ui))
                .child(title.to_owned()),
        )
        .child(keys)
}

impl Render for Sheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let os = cx.platform_info().os;
        let state = KeymapState::get(cx);
        let query = self.input.read(cx).value().to_string();

        let mut list = div().flex().flex_col().w_full().px_1().py_1();
        let mut shown = 0usize;
        for group in GROUP_ORDER {
            let mut rows: Vec<gpui::AnyElement> = Vec::new();
            for spec in keymap::ACTIONS.iter().filter(|spec| spec.group == group) {
                let title = keymap::display_title(spec);
                let caps: Vec<Vec<String>> = keymap::effective_keystrokes(state, spec, os)
                    .iter()
                    .map(|keystroke| ui::shortcut_caps(keystroke, os))
                    .collect();
                if !matches(query.as_str(), &title, &caps) {
                    continue;
                }
                rows.push(row(&title, &caps, &design).into_any_element());
            }
            if rows.is_empty() {
                continue;
            }
            shown += rows.len();
            list = list
                .child(
                    div()
                        .px_2()
                        .pt_2()
                        .pb_1()
                        .text_size(px(design.tiny()))
                        .text_color(design.muted)
                        .child(group_title(group)),
                )
                .child(div().flex().flex_col().gap(design.px(1.0)).children(rows));
        }
        if shown == 0 {
            list = list.child(ui::empty_state(
                t!("shortcuts.no_match").to_string(),
                &design,
            ));
        }

        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .key_context(DIALOG)
            .on_action(cx.listener(|_, _: &DismissShortcuts, window, cx| {
                window.close_dialog(cx);
                cx.stop_propagation();
            }))
            .child(Input::new(&self.input))
            .child(
                div()
                    .id("shortcuts-list")
                    .max_h(design.px(LIST_HEIGHT))
                    .overflow_y_scroll()
                    .child(list),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_1()
                    .text_size(px(design.tiny()))
                    .text_color(design.muted)
                    .child(t!("shortcuts.hint").to_string())
                    .child(if shown == 1 {
                        t!("shortcuts.actions.one", count = shown).to_string()
                    } else {
                        t!("shortcuts.actions.other", count = shown).to_string()
                    }),
            )
    }
}

/// Opens the sheet over the active window.
fn open(window: &mut Window, cx: &mut App) {
    // A modal already owns the keyboard; opening a second one over it would
    // close the first instead of the sheet's own card.
    if window.has_active_dialog(cx) {
        return;
    }
    let sheet = cx.new(|cx| Sheet::new(window, cx));
    let focus = sheet.read(cx).focus.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let content = sheet.clone();
        let zoom = Settings::get(cx).appearance.zoom();
        dialog
            .title(t!("shortcuts.title").to_string())
            .entrance(DialogEntrance::None)
            .centered()
            .w(px(WIDTH * zoom))
            .close_button(false)
            // The field owns Enter, and the sheet has nothing to confirm.
            .on_ok(|_, _, _| false)
            .content(move |body, _, _| body.child(content.clone()))
    });
    // The card takes the focus when it opens; the field needs it for both the
    // filter text and the `Esc` binding.
    window.defer(cx, move |window, cx| focus.focus(window, cx));
}

/// Whether `query` leaves one action in the list.
///
/// A title matches as a subsequence, the way the palette matches commands; a
/// key matches as a plain substring, so typing `⇧` or `shift` finds every
/// shortcut that uses it.
fn matches(query: &str, title: &str, caps: &[Vec<String>]) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    if crate::palette::score(query, title).is_some() {
        return true;
    }
    let lowered = query.to_lowercase();
    caps.iter()
        .flatten()
        .any(|cap| cap.to_lowercase().contains(&lowered))
}

/// The suffix of the `shortcuts.group.*` key that names one group, for the
/// test that checks the catalogue covers every group.
#[cfg(test)]
fn group_key(group: Group) -> &'static str {
    match group {
        Group::Actions => "actions",
        Group::Settings => "settings",
        Group::GitConfig => "git_config",
        Group::Worktree => "worktree",
        Group::Branches => "branches",
        Group::BranchSwitcher => "branch_switcher",
        Group::Conflicts => "conflicts",
        Group::Remote => "remote",
        Group::Search => "search",
        Group::Picker => "picker",
        Group::Palette => "palette",
        Group::Application => "application",
        Group::Zoom => "zoom",
        Group::Shortcuts => "shortcuts",
    }
}

fn group_title(group: Group) -> String {
    match group {
        Group::Actions => t!("shortcuts.group.actions"),
        Group::Settings => t!("shortcuts.group.settings"),
        Group::GitConfig => t!("shortcuts.group.git_config"),
        Group::Worktree => t!("shortcuts.group.worktree"),
        Group::Branches => t!("shortcuts.group.branches"),
        Group::BranchSwitcher => t!("shortcuts.group.branch_switcher"),
        Group::Conflicts => t!("shortcuts.group.conflicts"),
        Group::Remote => t!("shortcuts.group.remote"),
        Group::Search => t!("shortcuts.group.search"),
        Group::Picker => t!("shortcuts.group.picker"),
        Group::Palette => t!("shortcuts.group.palette"),
        Group::Application => t!("shortcuts.group.application"),
        Group::Zoom => t!("shortcuts.group.zoom"),
        Group::Shortcuts => t!("shortcuts.group.shortcuts"),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n;
    use gpui::OperatingSystem;

    #[test]
    fn every_group_has_a_heading_in_both_locales() {
        for group in GROUP_ORDER {
            let key = format!("shortcuts.group.{}", group_key(group));
            for locale in ["en", "ja"] {
                assert!(
                    i18n::lookup(locale, &key).is_some(),
                    "{locale} has no `{key}`"
                );
            }
        }
    }

    #[test]
    fn the_filter_reads_titles_and_keys() {
        let caps = vec![
            vec!["⌘".to_owned(), "⇧".to_owned(), "P".to_owned()],
            vec!["F1".to_owned()],
        ];
        assert!(matches("", "Toggle command palette", &caps));
        assert!(matches("pal", "Toggle command palette", &caps));
        assert!(matches("⇧", "Toggle command palette", &caps));
        assert!(matches("f1", "Toggle command palette", &caps));
        assert!(!matches("zz", "Toggle command palette", &caps));
    }

    #[test]
    fn the_sheet_is_opened_by_its_own_key_everywhere() {
        let spec = keymap::resolve("ShowKeyboardShortcuts").expect("a registry action");
        assert_eq!(spec.context, None);
        assert!(
            spec.defaults
                .iter()
                .any(|binding| binding.key == "secondary-/")
        );
        let state = KeymapState::default();
        let keys = keymap::effective_keystrokes(&state, spec, OperatingSystem::Mac);
        assert_eq!(
            ui::shortcut_caps(&keys[0], OperatingSystem::Mac),
            ["⌘", "/"]
        );
    }

    #[test]
    fn every_action_appears_under_one_heading() {
        let mut count = 0;
        for group in GROUP_ORDER {
            count += keymap::ACTIONS
                .iter()
                .filter(|spec| spec.group == group)
                .count();
        }
        assert_eq!(count, keymap::ACTIONS.len());
    }
}
