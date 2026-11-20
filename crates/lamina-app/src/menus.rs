//! The native application menu bar.
//!
//! The bar is a view of the action registry, not a second command list: every
//! item names an action from [`crate::keymap::ACTIONS`] and shows the title and
//! the key the registry and the live keymap already carry for it. A new action
//! therefore becomes reachable from the menu by adding one row to
//! [`PLACEMENTS`], and a rebinding or a language change only has to rebuild the
//! menu (`install`).
//!
//! GPUI draws the bar itself on macOS and Windows and ignores it elsewhere, so
//! nothing here paints anything: the items dispatch the same actions the
//! keyboard and the palette dispatch. An item whose action the focused window
//! does not handle — a repository command on the welcome screen — is disabled
//! by the platform's own validation, and dispatching it would be a no-op
//! anyway.
//!
//! The standard Cut/Copy/Paste/Select All items are the component library's
//! input actions with their OS selectors, so a focused text field keeps its
//! native editing menu entries.

use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, Window, actions};
use gpui_component::input::{Copy, Cut, Paste, SelectAll};

use crate::keymap::{self, ActionSpec};

actions!(
    lamina,
    [
        /// Opens the About window: the version, the build revision, the
        /// licences and the third-party notices.
        AboutLamina,
    ]
);

// The standard macOS window commands. They live outside the `lamina`
// namespace on purpose: a keymap reload keeps every binding whose action is
// not the app's own, so these keys survive a rebinding without the registry
// having to list commands the user should never rebind.
actions!(
    macos_window,
    [
        /// Hides the application.
        HideApp,
        /// Hides every other application.
        HideOtherApps,
        /// Shows every hidden application again.
        ShowAllApps,
        /// Minimizes the focused window into the Dock.
        MinimizeWindow,
        /// Toggles the focused window between its size and the screen.
        ZoomWindow,
    ]
);

/// One menu of the bar, in the order the bar lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKind {
    Lamina,
    File,
    Edit,
    View,
    Repository,
    Navigate,
    Window,
    Help,
}

impl MenuKind {
    pub const ALL: [MenuKind; 8] = [
        MenuKind::Lamina,
        MenuKind::File,
        MenuKind::Edit,
        MenuKind::View,
        MenuKind::Repository,
        MenuKind::Navigate,
        MenuKind::Window,
        MenuKind::Help,
    ];

    /// The suffix of the `menu.*` key that names the menu.
    #[cfg(test)]
    pub fn id(self) -> &'static str {
        match self {
            MenuKind::Lamina => "lamina",
            MenuKind::File => "file",
            MenuKind::Edit => "edit",
            MenuKind::View => "view",
            MenuKind::Repository => "repository",
            MenuKind::Navigate => "navigate",
            MenuKind::Window => "window",
            MenuKind::Help => "help",
        }
    }

    fn title(self) -> String {
        match self {
            MenuKind::Lamina => t!("menu.lamina"),
            MenuKind::File => t!("menu.file"),
            MenuKind::Edit => t!("menu.edit"),
            MenuKind::View => t!("menu.view"),
            MenuKind::Repository => t!("menu.repository"),
            MenuKind::Navigate => t!("menu.navigate"),
            MenuKind::Window => t!("menu.window"),
            MenuKind::Help => t!("menu.help"),
        }
        .to_string()
    }
}

/// Where one registry action sits in the bar.
pub struct Placement {
    /// The action's short registry name, e.g. `SwitchBranch`.
    pub action: &'static str,
    pub menu: MenuKind,
    /// Items of one section run together; a separator stands between sections.
    pub section: u8,
    /// The action may be missing from the registry: it is feature-gated, or it
    /// is being added by another change. The item is left out until it exists.
    pub optional: bool,
}

const fn item(action: &'static str, menu: MenuKind, section: u8) -> Placement {
    Placement {
        action,
        menu,
        section,
        optional: false,
    }
}

const fn optional(action: &'static str, menu: MenuKind, section: u8) -> Placement {
    Placement {
        action,
        menu,
        section,
        optional: true,
    }
}

/// Every menu item that comes from the registry, in bar order.
pub static PLACEMENTS: &[Placement] = &[
    item("AboutLamina", MenuKind::Lamina, 0),
    item("CheckForUpdates", MenuKind::Lamina, 0),
    item("OpenSettings", MenuKind::Lamina, 1),
    item("Quit", MenuKind::Lamina, 2),
    item("NewTab", MenuKind::File, 0),
    item("OpenRepository", MenuKind::File, 1),
    item("CloneRepository", MenuKind::File, 1),
    item("NewRepository", MenuKind::File, 1),
    item("OpenCompare", MenuKind::File, 1),
    // The reopened tab is offered by the tab module when it gains the action.
    optional("ReopenClosedTab", MenuKind::File, 2),
    item("CloseTab", MenuKind::File, 2),
    item("UndoLastAction", MenuKind::Edit, 0),
    item("RedoLastAction", MenuKind::Edit, 0),
    item("ToggleSidebar", MenuKind::View, 0),
    // Built only with the `terminal` feature; the item goes away with it.
    optional("ToggleTerminal", MenuKind::View, 1),
    item("ToggleDetailsMaximized", MenuKind::View, 2),
    item("ZoomIn", MenuKind::View, 3),
    item("ZoomOut", MenuKind::View, 3),
    item("ResetZoom", MenuKind::View, 3),
    item("ShowKeyboardShortcuts", MenuKind::View, 4),
    item("FetchRemotes", MenuKind::Repository, 0),
    item("PullBranch", MenuKind::Repository, 0),
    item("PushBranch", MenuKind::Repository, 0),
    item("StashChanges", MenuKind::Repository, 0),
    item("SwitchBranch", MenuKind::Repository, 1),
    item("CreateBranchFromHead", MenuKind::Repository, 1),
    item("OpenTerminalHere", MenuKind::Repository, 2),
    item("OpenInEditor", MenuKind::Repository, 2),
    item("ApplyPatch", MenuKind::Repository, 2),
    item("OpenGitConfig", MenuKind::Repository, 3),
    item("NavigateBack", MenuKind::Navigate, 0),
    item("NavigateForward", MenuKind::Navigate, 0),
    item("GoToParent", MenuKind::Navigate, 1),
    item("GoToSecondParent", MenuKind::Navigate, 1),
    item("GoToChild", MenuKind::Navigate, 1),
    item("GoToHead", MenuKind::Navigate, 1),
    item("FocusSearch", MenuKind::Navigate, 2),
    item("TogglePalette", MenuKind::Navigate, 3),
    item("NextTab", MenuKind::Window, 0),
    item("PreviousTab", MenuKind::Window, 0),
    item("ShowKeyboardShortcuts", MenuKind::Help, 0),
];

/// Registers the application menu's own actions. Call once at startup.
pub fn bind(cx: &mut App) {
    if cfg!(target_os = "macos") {
        cx.bind_keys([
            KeyBinding::new("cmd-h", HideApp, None),
            KeyBinding::new("cmd-alt-h", HideOtherApps, None),
            KeyBinding::new("cmd-m", MinimizeWindow, None),
        ]);
    }
    cx.on_action(|_: &HideApp, cx: &mut App| cx.hide());
    cx.on_action(|_: &HideOtherApps, cx: &mut App| cx.hide_other_apps());
    cx.on_action(|_: &ShowAllApps, cx: &mut App| cx.unhide_other_apps());
    cx.on_action(|_: &MinimizeWindow, cx: &mut App| {
        with_active_window(cx, |window| window.minimize_window());
    });
    cx.on_action(|_: &ZoomWindow, cx: &mut App| {
        with_active_window(cx, |window| window.zoom_window());
    });
}

/// Runs `f` on the focused window. Deferred because a menu item dispatches
/// while the window is already borrowed.
fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window) + 'static) {
    let Some(handle) = cx.active_window() else {
        return;
    };
    cx.defer(move |cx| {
        let _ = handle.update(cx, |_, window, _| f(window));
    });
}

/// Sets the whole bar. GPUI builds every item's key equivalent from the live
/// keymap, so this has to run again after the keymap or the language changes.
pub fn install(cx: &App) {
    cx.set_menus(menus());
}

/// The bar, built without an `App` so a test can read it.
pub fn menus() -> Vec<Menu> {
    MenuKind::ALL
        .into_iter()
        .map(|kind| Menu::new(kind.title()).items(items(kind)))
        .collect()
}

fn items(kind: MenuKind) -> Vec<MenuItem> {
    items_for(kind, cfg!(target_os = "macos"))
}

/// One menu's items for a platform, built without an `App` so a test can read
/// it. macOS keeps About and Check for Updates in the application menu; the
/// other platforms put them under Help, where their menu guidelines expect them.
fn items_for(kind: MenuKind, macos: bool) -> Vec<MenuItem> {
    let mut items = Vec::new();
    let mut section = None;
    for placement in PLACEMENTS.iter().filter(|item| item.menu == kind) {
        if !macos
            && kind == MenuKind::Lamina
            && matches!(placement.action, "AboutLamina" | "CheckForUpdates")
        {
            continue;
        }
        let Some(spec) = keymap::resolve(placement.action) else {
            debug_assert!(placement.optional, "{} is missing", placement.action);
            continue;
        };
        if section != Some(placement.section) && !items.is_empty() {
            items.push(MenuItem::separator());
        }
        section = Some(placement.section);
        items.push(action_item(spec));
    }
    if macos && kind == MenuKind::Lamina {
        // macOS puts Hide between Settings and Quit; Quit is the last section.
        let quit = items
            .iter()
            .rposition(|item| matches!(item, MenuItem::Separator));
        let hide = [
            MenuItem::action(t!("menu.lamina.hide").to_string(), HideApp),
            MenuItem::action(t!("menu.lamina.hide_others").to_string(), HideOtherApps),
            MenuItem::action(t!("menu.lamina.show_all").to_string(), ShowAllApps),
            MenuItem::separator(),
        ];
        let at = quit.map_or(items.len(), |index| index + 1);
        items.splice(at..at, hide);
    }
    if macos && kind == MenuKind::Window {
        let mut window = vec![
            MenuItem::action(t!("menu.window.minimize").to_string(), MinimizeWindow),
            MenuItem::action(t!("menu.window.zoom").to_string(), ZoomWindow),
        ];
        if !items.is_empty() {
            window.push(MenuItem::separator());
        }
        items.splice(0..0, window);
    }
    // Editing a text field is the component library's job and its own actions
    // already exist; the OS selectors keep the native menu behaviour.
    if kind == MenuKind::Edit {
        items.push(MenuItem::separator());
        items.extend([
            MenuItem::os_action(t!("menu.edit.cut").to_string(), Cut, OsAction::Cut),
            MenuItem::os_action(t!("menu.edit.copy").to_string(), Copy, OsAction::Copy),
            MenuItem::os_action(t!("menu.edit.paste").to_string(), Paste, OsAction::Paste),
            MenuItem::os_action(
                t!("menu.edit.select_all").to_string(),
                SelectAll,
                OsAction::SelectAll,
            ),
        ]);
    }
    if !macos && kind == MenuKind::Help {
        let extras: Vec<_> = ["CheckForUpdates", "AboutLamina"]
            .into_iter()
            .filter_map(keymap::resolve)
            .collect();
        if !extras.is_empty() {
            if !items.is_empty() {
                items.push(MenuItem::separator());
            }
            for spec in extras {
                items.push(action_item(spec));
            }
        }
    }
    items
}

/// One registry action as a menu item.
///
/// The variant is built directly because the constructors take a concrete
/// action type and the registry only knows `Box<dyn Action>`.
fn action_item(spec: &'static ActionSpec) -> MenuItem {
    MenuItem::Action {
        name: keymap::display_title(spec).into(),
        action: (spec.build)(),
        os_action: None,
        checked: false,
        disabled: false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::i18n;

    fn key_for(the_action: &str) -> String {
        format!(
            "action.{}",
            i18n::snake_case(the_action.rsplit("::").next().unwrap_or(the_action))
        )
    }

    #[test]
    fn every_required_menu_item_is_a_registry_action() {
        for placement in PLACEMENTS.iter().filter(|item| !item.optional) {
            assert!(
                keymap::resolve(placement.action).is_some(),
                "{} is in {:?} but not in the registry",
                placement.action,
                placement.menu
            );
        }
    }

    #[test]
    fn a_feature_gated_item_appears_when_its_action_does() {
        if cfg!(feature = "terminal") {
            assert!(
                keymap::resolve("ToggleTerminal").is_some(),
                "the terminal action is built, so its menu item has to be too"
            );
        }
    }

    #[test]
    fn no_action_is_listed_twice() {
        let mut seen = BTreeSet::new();
        for placement in PLACEMENTS {
            assert!(
                seen.insert((placement.action, format!("{:?}", placement.menu))),
                "{} is listed twice in {:?}",
                placement.action,
                placement.menu
            );
        }
    }

    #[test]
    fn every_menu_has_a_title_and_at_least_one_item() {
        for kind in MenuKind::ALL {
            for locale in ["en", "ja"] {
                let key = format!("menu.{}", kind.id());
                assert!(
                    i18n::lookup(locale, &key).is_some(),
                    "{locale} has no title for the {:?} menu (`{key}`)",
                    kind
                );
            }
            assert!(
                !items(kind).is_empty(),
                "the {:?} menu would draw empty",
                kind
            );
        }
    }

    #[test]
    fn every_menu_item_has_a_title_in_both_locales() {
        for placement in PLACEMENTS.iter().filter(|item| !item.optional) {
            let spec = keymap::resolve(placement.action).expect("a registry action");
            let key = key_for(spec.name);
            for locale in ["en", "ja"] {
                assert!(
                    i18n::lookup(locale, &key).is_some(),
                    "{locale} has no menu title for {} (`{key}`)",
                    spec.name
                );
            }
        }
        for key in [
            "edit.cut",
            "edit.copy",
            "edit.paste",
            "edit.select_all",
            "lamina.hide",
            "lamina.hide_others",
            "lamina.show_all",
            "window.minimize",
            "window.zoom",
        ] {
            let key = format!("menu.{key}");
            for locale in ["en", "ja"] {
                assert!(
                    i18n::lookup(locale, &key).is_some(),
                    "{locale} has no `{key}`"
                );
            }
        }
        for key in ["action.about_lamina", "about.description"] {
            for locale in ["en", "ja"] {
                assert!(
                    i18n::lookup(locale, key).is_some(),
                    "{locale} has no `{key}`"
                );
            }
        }
    }

    #[test]
    fn the_bar_lists_the_menus_in_order() {
        let titles: Vec<String> = menus()
            .into_iter()
            .map(|menu| menu.name.to_string())
            .collect();
        let expected: Vec<String> = MenuKind::ALL.into_iter().map(|kind| kind.title()).collect();
        assert_eq!(titles, expected);
    }

    fn has_action<A: gpui::Action>(items: &[MenuItem]) -> bool {
        items.iter().any(|item| {
            matches!(
                item,
                MenuItem::Action { action, .. }
                    if action.as_any().is::<A>()
            )
        })
    }

    #[test]
    fn about_sits_in_the_application_menu_on_macos_and_under_help_elsewhere() {
        assert!(has_action::<AboutLamina>(&items_for(
            MenuKind::Lamina,
            true
        )));
        assert!(!has_action::<AboutLamina>(&items_for(MenuKind::Help, true)));

        assert!(!has_action::<AboutLamina>(&items_for(
            MenuKind::Lamina,
            false
        )));
        assert!(has_action::<AboutLamina>(&items_for(MenuKind::Help, false)));

        assert!(has_action::<crate::updater::CheckForUpdates>(&items_for(
            MenuKind::Lamina,
            true
        )));
        assert!(!has_action::<crate::updater::CheckForUpdates>(&items_for(
            MenuKind::Help,
            true
        )));

        assert!(!has_action::<crate::updater::CheckForUpdates>(&items_for(
            MenuKind::Lamina,
            false
        )));
        assert!(has_action::<crate::updater::CheckForUpdates>(&items_for(
            MenuKind::Help,
            false
        )));
    }

    #[test]
    fn sections_are_separated_by_one_separator() {
        let items = items(MenuKind::Repository);
        let separators: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| matches!(item, MenuItem::Separator))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(separators.len(), 3, "four sections need three separators");
        assert!(
            !items
                .windows(2)
                .any(|pair| pair.iter().all(|item| matches!(item, MenuItem::Separator))),
            "two separators would draw an empty section"
        );
    }
}
