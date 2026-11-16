//! The action registry and the user keymap.
//!
//! Every command the app can run is described once, here: its action name, the
//! title the palette shows, the key context it belongs to, and its default
//! bindings. The palette reads the titles, the settings window reads the whole
//! table, and startup binds the defaults from it, so those three cannot drift
//! apart.
//!
//! Defaults use `secondary-` for the platform's primary modifier (`cmd` on
//! macOS, `ctrl` elsewhere), which is resolved when the bindings are built.
//! They are normally registered through the owning module's `bind` function,
//! which calls [`bind_group`] in the same order `main` calls those functions;
//! that order is what settles ties between two bindings for the same keystroke
//! in the same context. [`install_defaults`] repeats that sequence for a
//! reload.
//!
//! A user `keymap.json` next to `settings.json` adds or removes bindings, and
//! is applied after every default so its entries win. The file is watched by a
//! small poller, because a config file changes far less often than a
//! repository and does not need the native watcher's threads.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use gpui::{
    Action, App, Global, KeyBinding, KeyBindingContextPredicate, Keystroke, OperatingSystem,
    SharedString, Unbind,
};
use gpui_component::WindowExt as _;
use serde::{Deserialize, Serialize};

use crate::settings;

/// A group of bindings registered by one module. The order of the variants is
/// the order `main` binds them, which decides who wins a tied keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Actions,
    Settings,
    GitConfig,
    Worktree,
    Branches,
    BranchSwitcher,
    Conflicts,
    Remote,
    Search,
    Picker,
    Palette,
    Application,
    Zoom,
    Shortcuts,
    Terminal,
}

/// Every group, in registration order.
pub const GROUP_ORDER: [Group; 15] = [
    Group::Actions,
    Group::Settings,
    Group::GitConfig,
    Group::Worktree,
    Group::Branches,
    Group::BranchSwitcher,
    Group::Conflicts,
    Group::Remote,
    Group::Search,
    Group::Picker,
    Group::Palette,
    Group::Application,
    Group::Zoom,
    Group::Shortcuts,
    Group::Terminal,
];

/// One default binding of an action.
#[derive(Debug, Clone, Copy)]
pub struct DefaultBinding {
    /// Written with `secondary-` for the platform's primary modifier.
    pub key: &'static str,
    pub context: Option<&'static str>,
    /// macOS only. `Ctrl+[` is a readline key elsewhere, so the second way
    /// back to the parent repository stays where it was asked for.
    pub macos_only: bool,
    /// Registered at the end of its group instead of beside the action's other
    /// bindings. A focused field's own context is deeper, but a module used to
    /// add these last and the registration order stays as it was.
    pub deferred: bool,
}

/// Where an action appears in the command palette.
#[derive(Debug, Clone, Copy)]
pub struct PaletteSlot {
    /// Position among the palette's built-in commands.
    pub order: u16,
    /// Left out while no repository is open.
    pub needs_repo: bool,
}

/// One bindable command.
#[derive(Debug)]
pub struct ActionSpec {
    /// The action's own name, e.g. `strata::SwitchBranch`.
    pub name: &'static str,
    /// What the palette lists and the settings table shows.
    pub title: &'static str,
    /// The key context the command acts in, for the settings table. `None`
    /// means it works in every context.
    pub context: Option<&'static str>,
    pub defaults: &'static [DefaultBinding],
    pub build: fn() -> Box<dyn Action>,
    pub group: Group,
    pub palette: Option<PaletteSlot>,
}

const fn binding(key: &'static str, context: Option<&'static str>) -> DefaultBinding {
    DefaultBinding {
        key,
        context,
        macos_only: false,
        deferred: false,
    }
}

const fn deferred(key: &'static str, context: Option<&'static str>) -> DefaultBinding {
    DefaultBinding {
        key,
        context,
        macos_only: false,
        deferred: true,
    }
}

const fn mac_binding(key: &'static str, context: Option<&'static str>) -> DefaultBinding {
    DefaultBinding {
        key,
        context,
        macos_only: true,
        deferred: false,
    }
}

macro_rules! action {
    ($name:literal, $title:literal, $group:ident, $context:expr, $build:path, [$($binding:expr),* $(,)?]) => {
        ActionSpec {
            name: $name,
            title: $title,
            context: $context,
            defaults: &[$($binding),*],
            build: || Box::new($build),
            group: Group::$group,
            palette: None,
        }
    };
    ($name:literal, $title:literal, $group:ident, $context:expr, $build:path, [$($binding:expr),* $(,)?], $order:literal, $needs_repo:literal) => {
        ActionSpec {
            name: $name,
            title: $title,
            context: $context,
            defaults: &[$($binding),*],
            build: || Box::new($build),
            group: Group::$group,
            palette: Some(PaletteSlot { order: $order, needs_repo: $needs_repo }),
        }
    };
}

use crate::about::{ABOUT_VIEW, CloseAbout};
use crate::actions::{
    ApplyPatch, CancelRunningOperation, CloneRepository, CloseCompare, CloseTab,
    CompareBranchThreeWay, CompareCommitsThreeWay, ExtendSelectionNext, ExtendSelectionPrevious,
    GoToChild, GoToHead, GoToParent, GoToSecondParent, NavigateBack, NavigateForward,
    NewRepository, NewTab, NextTab, OpenCompare, OpenGitConfig, OpenInEditor, OpenRemoteRepository,
    OpenRepository, OpenSettings, OpenTerminalHere, PreviousTab, Quit, REPO_VIEW, REPO_VIEW_LIST,
    RedoLastAction, RefreshWorkingTree, ReopenClosedTab, SelectFirst, SelectLast, SelectNext,
    SelectPageDown, SelectPageUp, SelectPrevious, SelectTab1, SelectTab2, SelectTab3, SelectTab4,
    SelectTab5, SelectTab6, SelectTab7, SelectTab8, SelectTab9, StashChanges, SwitchBranch,
    ToggleDetailsMaximized, ToggleFrameStats, TogglePalette, ToggleSidebar, ToggleWholeFile,
    UndoLastAction, WORKSPACE,
};
#[cfg(feature = "terminal")]
use crate::actions::{
    CloseTerminalTab, NewTerminalTab, NextTerminalTab, PreviousTerminalTab, ToggleTerminal,
};
use crate::branch_switcher::{
    BranchSwitcherConfirm, BranchSwitcherDismiss, BranchSwitcherNext, BranchSwitcherPrevious,
};
use crate::branches::{CancelBranchDialog, ConfirmBranchDialog, CreateBranchFromHead};
use crate::conflicts::{
    MarkFileResolved, NextConflict, PreviousConflict, ResetConflict, TakeBase, TakeBothOursFirst,
    TakeBothTheirsFirst, TakeOurs, TakeTheirs, UseOurs, UseTheirs,
};
use crate::git_config_view::CloseGitConfig;
use crate::menus::AboutStrata;
use crate::palette::{PaletteDismiss, PaletteNext, PalettePrevious};
use crate::picker::{
    OpenSelectedInNewTab, OpenSelectedRepository, PickerEscape, PickerNext, PickerPrevious,
    RemoveSelectedRecent,
};
use crate::pull_requests::{CheckoutPullRequest, RefreshPullRequests};
use crate::remote::{FetchRemotes, PullBranch, PushBranch};
use crate::search::{FocusSearch, SearchDismiss, SearchNext, SearchPrevious, SearchSubmit};
use crate::settings_view::CloseSettings;
use crate::shortcuts::{DismissShortcuts, ShowKeyboardShortcuts};
use crate::worktree_panel::{
    CommitMessage, DiscardSelected, ExtendNextFile, ExtendPreviousFile, SelectAllFiles,
    SelectNextFile, SelectPreviousFile, ShowHistory, StageSelected, ToggleSelected,
    UnstageSelected,
};
use crate::zoom::{ResetZoom, ZoomIn, ZoomOut};

/// The worktree panel's context constants live with its actions; naming them
/// here keeps every context string visible in one table.
const WORKTREE_PANEL: &str = "WorktreePanel";
const WORKTREE_PANEL_LIST: &str = "WorktreePanel && !Input";
const BRANCH_DIALOG: &str = "BranchDialog";
const BRANCH_SWITCHER_FIELD: &str = "BranchSwitcher > Input";
const GIT_CONFIG_VIEW: &str = "GitConfigView";
const SETTINGS_VIEW: &str = "SettingsView";
const SEARCH_FIELD: &str = "CommitSearch > Input";
const PICKER_PAGE: &str = "PickerPage";
const PICKER_PAGE_LIST: &str = "PickerPage && !Input";
const PICKER_DIALOG: &str = "PickerDialog";
const PICKER_DIALOG_LIST: &str = "PickerDialog && !Input";
const PALETTE_FIELD: &str = "Palette > Input";
const SHORTCUTS_FIELD: &str = "ShortcutsDialog > Input";
#[cfg(feature = "terminal")]
const TERMINAL_PANEL: &str = "TerminalPanel";

/// Every bindable action, in registration order.
///
/// The order within a group is the order the group's module used to call
/// `bind_keys`, and the groups are in the order `main` calls the modules.
pub static ACTIONS: &[ActionSpec] = &[
    // --- actions::bind ---------------------------------------------------
    action!(
        "strata::SelectPrevious",
        "Select previous commit",
        Actions,
        Some(REPO_VIEW),
        SelectPrevious,
        [
            binding("up", Some(REPO_VIEW)),
            binding("k", Some(REPO_VIEW_LIST))
        ]
    ),
    action!(
        "strata::SelectNext",
        "Select next commit",
        Actions,
        Some(REPO_VIEW),
        SelectNext,
        [
            binding("down", Some(REPO_VIEW)),
            binding("j", Some(REPO_VIEW_LIST))
        ]
    ),
    action!(
        "strata::ToggleDetailsMaximized",
        "Toggle details maximized",
        Actions,
        Some(REPO_VIEW_LIST),
        ToggleDetailsMaximized,
        [binding("z", Some(REPO_VIEW_LIST))],
        30,
        true
    ),
    action!(
        "strata::ToggleWholeFile",
        "Toggle whole file",
        Actions,
        Some(REPO_VIEW_LIST),
        ToggleWholeFile,
        [binding("w", Some(REPO_VIEW_LIST))],
        31,
        true
    ),
    action!(
        "strata::ApplyPatch",
        "Apply Patch…",
        Actions,
        Some(REPO_VIEW),
        ApplyPatch,
        [],
        50,
        true
    ),
    action!(
        "strata::ToggleSidebar",
        "Toggle sidebar",
        Actions,
        Some(REPO_VIEW),
        ToggleSidebar,
        [binding("secondary-alt-s", Some(REPO_VIEW))],
        37,
        true
    ),
    action!(
        "strata::ExtendSelectionPrevious",
        "Extend selection up",
        Actions,
        Some(REPO_VIEW),
        ExtendSelectionPrevious,
        [binding("shift-up", Some(REPO_VIEW))]
    ),
    action!(
        "strata::ExtendSelectionNext",
        "Extend selection down",
        Actions,
        Some(REPO_VIEW),
        ExtendSelectionNext,
        [binding("shift-down", Some(REPO_VIEW))]
    ),
    action!(
        "strata::SelectPageUp",
        "Select page up",
        Actions,
        Some(REPO_VIEW),
        SelectPageUp,
        [binding("pageup", Some(REPO_VIEW))]
    ),
    action!(
        "strata::SelectPageDown",
        "Select page down",
        Actions,
        Some(REPO_VIEW),
        SelectPageDown,
        [binding("pagedown", Some(REPO_VIEW))]
    ),
    action!(
        "strata::SelectFirst",
        "Select first commit",
        Actions,
        Some(REPO_VIEW),
        SelectFirst,
        [binding("home", Some(REPO_VIEW))]
    ),
    action!(
        "strata::SelectLast",
        "Select last commit",
        Actions,
        Some(REPO_VIEW),
        SelectLast,
        [binding("end", Some(REPO_VIEW))]
    ),
    action!(
        "strata::ToggleFrameStats",
        "Toggle frame stats",
        Actions,
        Some(REPO_VIEW),
        ToggleFrameStats,
        [binding("f12", Some(REPO_VIEW))],
        32,
        true
    ),
    action!(
        "strata::StashChanges",
        "Stash changes",
        Actions,
        Some(REPO_VIEW),
        StashChanges,
        [binding("secondary-shift-s", Some(REPO_VIEW))],
        23,
        true
    ),
    action!(
        "strata::SwitchBranch",
        "Switch Branch…",
        Actions,
        Some(REPO_VIEW),
        SwitchBranch,
        [binding("secondary-b", Some(REPO_VIEW))],
        28,
        true
    ),
    action!(
        "strata::UndoLastAction",
        "Undo Last Action",
        Actions,
        Some(REPO_VIEW_LIST),
        UndoLastAction,
        [binding("secondary-z", Some(REPO_VIEW_LIST))],
        24,
        true
    ),
    action!(
        "strata::RedoLastAction",
        "Redo Last Action",
        Actions,
        Some(REPO_VIEW_LIST),
        RedoLastAction,
        [binding("secondary-shift-z", Some(REPO_VIEW_LIST))],
        44,
        true
    ),
    action!(
        "strata::NavigateBack",
        "Back",
        Actions,
        Some(REPO_VIEW),
        NavigateBack,
        [
            binding("alt-left", Some(REPO_VIEW)),
            mac_binding("secondary-[", Some(REPO_VIEW))
        ],
        16,
        true
    ),
    action!(
        "strata::NavigateForward",
        "Go forward",
        Actions,
        Some(REPO_VIEW),
        NavigateForward,
        [
            binding("alt-right", Some(REPO_VIEW)),
            mac_binding("secondary-]", Some(REPO_VIEW))
        ],
        45,
        true
    ),
    action!(
        "strata::GoToParent",
        "Go to parent commit",
        Actions,
        Some(REPO_VIEW_LIST),
        GoToParent,
        [binding("secondary-down", Some(REPO_VIEW_LIST))],
        46,
        true
    ),
    action!(
        "strata::GoToSecondParent",
        "Go to second parent",
        Actions,
        Some(REPO_VIEW_LIST),
        GoToSecondParent,
        [binding("secondary-shift-down", Some(REPO_VIEW_LIST))],
        47,
        true
    ),
    action!(
        "strata::GoToChild",
        "Go to child commit",
        Actions,
        Some(REPO_VIEW_LIST),
        GoToChild,
        [binding("secondary-up", Some(REPO_VIEW_LIST))],
        48,
        true
    ),
    action!(
        "strata::GoToHead",
        "Go to HEAD",
        Actions,
        Some(REPO_VIEW_LIST),
        GoToHead,
        [binding("secondary-shift-h", Some(REPO_VIEW_LIST))],
        49,
        true
    ),
    action!(
        "strata::NewTab",
        "Open in new tab…",
        Actions,
        Some(WORKSPACE),
        NewTab,
        [binding("secondary-t", Some(WORKSPACE))],
        1,
        false
    ),
    action!(
        "strata::CloseTab",
        "Close tab",
        Actions,
        Some(WORKSPACE),
        CloseTab,
        [binding("secondary-w", Some(WORKSPACE))],
        4,
        false
    ),
    action!(
        "strata::ReopenClosedTab",
        "Reopen closed tab",
        Actions,
        Some(WORKSPACE),
        ReopenClosedTab,
        [binding("secondary-shift-t", Some(WORKSPACE))]
    ),
    action!(
        "strata::NextTab",
        "Next tab",
        Actions,
        Some(WORKSPACE),
        NextTab,
        [binding("ctrl-tab", Some(WORKSPACE))],
        5,
        true
    ),
    action!(
        "strata::PreviousTab",
        "Previous tab",
        Actions,
        Some(WORKSPACE),
        PreviousTab,
        [binding("ctrl-shift-tab", Some(WORKSPACE))],
        6,
        true
    ),
    action!(
        "strata::SelectTab1",
        "Switch to tab 1",
        Actions,
        Some(WORKSPACE),
        SelectTab1,
        [binding("secondary-1", Some(WORKSPACE))],
        7,
        true
    ),
    action!(
        "strata::SelectTab2",
        "Switch to tab 2",
        Actions,
        Some(WORKSPACE),
        SelectTab2,
        [binding("secondary-2", Some(WORKSPACE))],
        8,
        true
    ),
    action!(
        "strata::SelectTab3",
        "Switch to tab 3",
        Actions,
        Some(WORKSPACE),
        SelectTab3,
        [binding("secondary-3", Some(WORKSPACE))],
        9,
        true
    ),
    action!(
        "strata::SelectTab4",
        "Switch to tab 4",
        Actions,
        Some(WORKSPACE),
        SelectTab4,
        [binding("secondary-4", Some(WORKSPACE))],
        10,
        true
    ),
    action!(
        "strata::SelectTab5",
        "Switch to tab 5",
        Actions,
        Some(WORKSPACE),
        SelectTab5,
        [binding("secondary-5", Some(WORKSPACE))],
        11,
        true
    ),
    action!(
        "strata::SelectTab6",
        "Switch to tab 6",
        Actions,
        Some(WORKSPACE),
        SelectTab6,
        [binding("secondary-6", Some(WORKSPACE))],
        12,
        true
    ),
    action!(
        "strata::SelectTab7",
        "Switch to tab 7",
        Actions,
        Some(WORKSPACE),
        SelectTab7,
        [binding("secondary-7", Some(WORKSPACE))],
        13,
        true
    ),
    action!(
        "strata::SelectTab8",
        "Switch to tab 8",
        Actions,
        Some(WORKSPACE),
        SelectTab8,
        [binding("secondary-8", Some(WORKSPACE))],
        14,
        true
    ),
    action!(
        "strata::SelectTab9",
        "Switch to tab 9",
        Actions,
        Some(WORKSPACE),
        SelectTab9,
        [binding("secondary-9", Some(WORKSPACE))],
        15,
        true
    ),
    action!(
        "strata::OpenSettings",
        "Open settings",
        Actions,
        None,
        OpenSettings,
        [binding("secondary-,", None)],
        17,
        false
    ),
    action!(
        "strata::OpenRepository",
        "Open repository…",
        Actions,
        None,
        OpenRepository,
        [binding("secondary-o", None)],
        0,
        false
    ),
    action!(
        "strata::OpenRemoteRepository",
        "Open Remote Repository…",
        Actions,
        None,
        OpenRemoteRepository,
        [],
        54,
        false
    ),
    action!(
        "strata::TogglePalette",
        "Toggle command palette",
        Actions,
        None,
        TogglePalette,
        [binding("secondary-shift-p", None), binding("f1", None)]
    ),
    action!(
        "strata::Quit",
        "Quit Strata",
        Actions,
        None,
        Quit,
        [binding("secondary-q", None)],
        33,
        false
    ),
    action!(
        "strata::CloseCompare",
        "Close comparison",
        Actions,
        Some(REPO_VIEW),
        CloseCompare,
        []
    ),
    action!(
        "strata::OpenTerminalHere",
        "Open terminal here",
        Actions,
        Some(REPO_VIEW),
        OpenTerminalHere,
        [],
        25,
        true
    ),
    action!(
        "strata::OpenInEditor",
        "Open in Editor",
        Actions,
        Some(REPO_VIEW),
        OpenInEditor,
        [],
        26,
        true
    ),
    action!(
        "strata::CloneRepository",
        "Clone Repository…",
        Actions,
        None,
        CloneRepository,
        [],
        2,
        false
    ),
    action!(
        "strata::NewRepository",
        "New Repository…",
        Actions,
        None,
        NewRepository,
        [],
        3,
        false
    ),
    action!(
        "strata::OpenCompare",
        "Compare Files or Folders…",
        Actions,
        Some(WORKSPACE),
        OpenCompare,
        [],
        51,
        false
    ),
    action!(
        "strata::CompareBranchThreeWay",
        "Three-way compare with current branch",
        Actions,
        Some(REPO_VIEW),
        CompareBranchThreeWay,
        [],
        52,
        true
    ),
    action!(
        "strata::CompareCommitsThreeWay",
        "Three-way compare commits",
        Actions,
        Some(REPO_VIEW),
        CompareCommitsThreeWay,
        [],
        53,
        true
    ),
    action!(
        "strata::OpenGitConfig",
        "Show Git Config",
        Actions,
        None,
        OpenGitConfig,
        [],
        18,
        true
    ),
    action!(
        "strata::CancelRunningOperation",
        "Cancel running operation",
        Actions,
        Some(REPO_VIEW),
        CancelRunningOperation,
        [],
        55,
        true
    ),
    action!(
        "strata::RefreshWorkingTree",
        "Refresh working tree",
        Actions,
        Some(REPO_VIEW),
        RefreshWorkingTree,
        [binding("f5", Some(REPO_VIEW))],
        56,
        true
    ),
    #[cfg(feature = "terminal")]
    action!(
        "strata::ToggleTerminal",
        "Toggle terminal",
        Actions,
        Some(REPO_VIEW),
        ToggleTerminal,
        [binding("ctrl-`", Some(REPO_VIEW))],
        36,
        true
    ),
    // --- settings_view::bind ---------------------------------------------
    action!(
        "strata::CloseSettings",
        "Close settings",
        Settings,
        Some(SETTINGS_VIEW),
        CloseSettings,
        [
            binding("escape", Some(SETTINGS_VIEW)),
            binding("secondary-w", Some(SETTINGS_VIEW))
        ]
    ),
    // --- git_config_view::bind -------------------------------------------
    action!(
        "strata_git_config::CloseGitConfig",
        "Close Git config",
        GitConfig,
        Some(GIT_CONFIG_VIEW),
        CloseGitConfig,
        [
            binding("escape", Some(GIT_CONFIG_VIEW)),
            binding("secondary-w", Some(GIT_CONFIG_VIEW))
        ]
    ),
    // --- worktree_panel::bind --------------------------------------------
    action!(
        "strata_worktree::StageSelected",
        "Stage selected file",
        Worktree,
        Some(WORKTREE_PANEL_LIST),
        StageSelected,
        [binding("s", Some(WORKTREE_PANEL_LIST))]
    ),
    action!(
        "strata_worktree::UnstageSelected",
        "Unstage selected file",
        Worktree,
        Some(WORKTREE_PANEL_LIST),
        UnstageSelected,
        [binding("u", Some(WORKTREE_PANEL_LIST))]
    ),
    action!(
        "strata_worktree::DiscardSelected",
        "Discard selected file",
        Worktree,
        Some(WORKTREE_PANEL_LIST),
        DiscardSelected,
        [binding("d", Some(WORKTREE_PANEL_LIST))]
    ),
    action!(
        "strata_worktree::ToggleSelected",
        "Toggle selected file",
        Worktree,
        Some(WORKTREE_PANEL),
        ToggleSelected,
        [binding("enter", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_worktree::SelectAllFiles",
        "Select all files",
        Worktree,
        Some(WORKTREE_PANEL),
        SelectAllFiles,
        [binding("secondary-a", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_worktree::SelectPreviousFile",
        "Select previous file",
        Worktree,
        Some(WORKTREE_PANEL),
        SelectPreviousFile,
        [binding("up", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_worktree::SelectNextFile",
        "Select next file",
        Worktree,
        Some(WORKTREE_PANEL),
        SelectNextFile,
        [binding("down", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_worktree::ExtendPreviousFile",
        "Extend file selection up",
        Worktree,
        Some(WORKTREE_PANEL),
        ExtendPreviousFile,
        [binding("shift-up", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_worktree::ExtendNextFile",
        "Extend file selection down",
        Worktree,
        Some(WORKTREE_PANEL),
        ExtendNextFile,
        [binding("shift-down", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_worktree::CommitMessage",
        "Focus the commit message",
        Worktree,
        Some(WORKTREE_PANEL),
        CommitMessage,
        [binding("secondary-enter", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_worktree::ShowHistory",
        "Show history",
        Worktree,
        Some(WORKTREE_PANEL),
        ShowHistory,
        [binding("escape", Some(WORKTREE_PANEL))],
        29,
        true
    ),
    // --- branches::bind --------------------------------------------------
    action!(
        "strata::CreateBranchFromHead",
        "Create branch from HEAD",
        Branches,
        Some(REPO_VIEW),
        CreateBranchFromHead,
        [binding("secondary-shift-b", Some(REPO_VIEW))],
        27,
        true
    ),
    action!(
        "strata::ConfirmBranchDialog",
        "Create the branch",
        Branches,
        Some(BRANCH_DIALOG),
        ConfirmBranchDialog,
        [binding("enter", Some(BRANCH_DIALOG))]
    ),
    action!(
        "strata::CancelBranchDialog",
        "Cancel the branch dialog",
        Branches,
        Some(BRANCH_DIALOG),
        CancelBranchDialog,
        [binding("escape", Some(BRANCH_DIALOG))]
    ),
    // --- branch_switcher::bind -------------------------------------------
    action!(
        "strata::BranchSwitcherPrevious",
        "Switch branch: previous",
        BranchSwitcher,
        Some(BRANCH_SWITCHER_FIELD),
        BranchSwitcherPrevious,
        [binding("up", Some(BRANCH_SWITCHER_FIELD))]
    ),
    action!(
        "strata::BranchSwitcherNext",
        "Switch branch: next",
        BranchSwitcher,
        Some(BRANCH_SWITCHER_FIELD),
        BranchSwitcherNext,
        [binding("down", Some(BRANCH_SWITCHER_FIELD))]
    ),
    action!(
        "strata::BranchSwitcherConfirm",
        "Switch to the highlighted branch",
        BranchSwitcher,
        Some(BRANCH_SWITCHER_FIELD),
        BranchSwitcherConfirm,
        [binding("enter", Some(BRANCH_SWITCHER_FIELD))]
    ),
    action!(
        "strata::BranchSwitcherDismiss",
        "Close the branch switcher",
        BranchSwitcher,
        Some(BRANCH_SWITCHER_FIELD),
        BranchSwitcherDismiss,
        [binding("escape", Some(BRANCH_SWITCHER_FIELD))]
    ),
    // --- conflicts::bind -------------------------------------------------
    action!(
        "strata_conflicts::UseOurs",
        "Use our side",
        Conflicts,
        Some(WORKTREE_PANEL_LIST),
        UseOurs,
        [binding("o", Some(WORKTREE_PANEL_LIST))]
    ),
    action!(
        "strata_conflicts::UseTheirs",
        "Use their side",
        Conflicts,
        Some(WORKTREE_PANEL_LIST),
        UseTheirs,
        [binding("t", Some(WORKTREE_PANEL_LIST))]
    ),
    action!(
        "strata_conflicts::MarkFileResolved",
        "Mark the file resolved",
        Conflicts,
        Some(WORKTREE_PANEL_LIST),
        MarkFileResolved,
        [binding("m", Some(WORKTREE_PANEL_LIST))]
    ),
    action!(
        "strata_conflicts::NextConflict",
        "Next conflict",
        Conflicts,
        Some(WORKTREE_PANEL),
        NextConflict,
        [
            binding("n", Some(WORKTREE_PANEL_LIST)),
            deferred("alt-n", Some(WORKTREE_PANEL))
        ]
    ),
    action!(
        "strata_conflicts::PreviousConflict",
        "Previous conflict",
        Conflicts,
        Some(WORKTREE_PANEL),
        PreviousConflict,
        [
            binding("p", Some(WORKTREE_PANEL_LIST)),
            deferred("alt-p", Some(WORKTREE_PANEL))
        ]
    ),
    action!(
        "strata_conflicts::TakeOurs",
        "Take our side of the hunk",
        Conflicts,
        Some(WORKTREE_PANEL),
        TakeOurs,
        [binding("alt-1", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_conflicts::TakeTheirs",
        "Take their side of the hunk",
        Conflicts,
        Some(WORKTREE_PANEL),
        TakeTheirs,
        [binding("alt-2", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_conflicts::TakeBothOursFirst",
        "Take both sides, ours first",
        Conflicts,
        Some(WORKTREE_PANEL),
        TakeBothOursFirst,
        [binding("alt-3", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_conflicts::TakeBothTheirsFirst",
        "Take both sides, theirs first",
        Conflicts,
        Some(WORKTREE_PANEL),
        TakeBothTheirsFirst,
        [binding("alt-4", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_conflicts::TakeBase",
        "Take the base version",
        Conflicts,
        Some(WORKTREE_PANEL),
        TakeBase,
        [binding("alt-5", Some(WORKTREE_PANEL))]
    ),
    action!(
        "strata_conflicts::ResetConflict",
        "Reset the conflict",
        Conflicts,
        Some(WORKTREE_PANEL),
        ResetConflict,
        [binding("alt-0", Some(WORKTREE_PANEL))]
    ),
    // --- remote::bind ----------------------------------------------------
    action!(
        "strata::FetchRemotes",
        "Fetch remotes",
        Remote,
        Some(REPO_VIEW),
        FetchRemotes,
        [binding("secondary-alt-f", Some(REPO_VIEW))],
        20,
        true
    ),
    action!(
        "strata::PullBranch",
        "Pull branch",
        Remote,
        Some(REPO_VIEW),
        PullBranch,
        [binding("secondary-alt-l", Some(REPO_VIEW))],
        21,
        true
    ),
    action!(
        "strata::PushBranch",
        "Push branch",
        Remote,
        Some(REPO_VIEW),
        PushBranch,
        [binding("secondary-alt-p", Some(REPO_VIEW))],
        22,
        true
    ),
    action!(
        "strata::RefreshPullRequests",
        "Refresh Pull Requests",
        Remote,
        Some(REPO_VIEW),
        RefreshPullRequests,
        [],
        34,
        true
    ),
    action!(
        "strata::CheckoutPullRequest",
        "Check out Pull Request…",
        Remote,
        Some(REPO_VIEW),
        CheckoutPullRequest,
        [],
        35,
        true
    ),
    // --- search::bind ----------------------------------------------------
    action!(
        "strata_search::FocusSearch",
        "Search commits",
        Search,
        Some(REPO_VIEW),
        FocusSearch,
        [binding("secondary-f", Some(REPO_VIEW))],
        19,
        true
    ),
    action!(
        "strata_search::SearchDismiss",
        "Close search",
        Search,
        Some(REPO_VIEW),
        SearchDismiss,
        [binding("escape", Some(REPO_VIEW))]
    ),
    action!(
        "strata_search::SearchNext",
        "Next search result",
        Search,
        Some(REPO_VIEW),
        SearchNext,
        [
            binding("down", Some(REPO_VIEW)),
            deferred("down", Some(SEARCH_FIELD))
        ]
    ),
    action!(
        "strata_search::SearchPrevious",
        "Previous search result",
        Search,
        Some(REPO_VIEW),
        SearchPrevious,
        [
            binding("up", Some(REPO_VIEW)),
            deferred("up", Some(SEARCH_FIELD))
        ]
    ),
    action!(
        "strata_search::SearchSubmit",
        "Run the search",
        Search,
        Some(REPO_VIEW),
        SearchSubmit,
        [binding("enter", Some(REPO_VIEW))]
    ),
    // --- picker::bind ----------------------------------------------------
    action!(
        "strata::PickerPrevious",
        "Picker: previous repository",
        Picker,
        Some(PICKER_PAGE),
        PickerPrevious,
        [
            binding("up", Some(PICKER_PAGE)),
            binding("k", Some(PICKER_PAGE_LIST)),
            deferred("up", Some(PICKER_DIALOG)),
            deferred("k", Some(PICKER_DIALOG_LIST))
        ]
    ),
    action!(
        "strata::PickerNext",
        "Picker: next repository",
        Picker,
        Some(PICKER_PAGE),
        PickerNext,
        [
            binding("down", Some(PICKER_PAGE)),
            binding("j", Some(PICKER_PAGE_LIST)),
            deferred("down", Some(PICKER_DIALOG)),
            deferred("j", Some(PICKER_DIALOG_LIST))
        ]
    ),
    action!(
        "strata::OpenSelectedRepository",
        "Open the selected repository",
        Picker,
        Some(PICKER_PAGE),
        OpenSelectedRepository,
        [binding("enter", Some(PICKER_PAGE))]
    ),
    action!(
        "strata::OpenSelectedInNewTab",
        "Open the selected repository in a new tab",
        Picker,
        Some(PICKER_PAGE),
        OpenSelectedInNewTab,
        [binding("secondary-enter", Some(PICKER_PAGE))]
    ),
    action!(
        "strata::PickerEscape",
        "Close the repository picker",
        Picker,
        Some(PICKER_PAGE),
        PickerEscape,
        [binding("escape", Some(PICKER_PAGE))]
    ),
    action!(
        "strata::RemoveSelectedRecent",
        "Remove selected recent repository",
        Picker,
        Some(PICKER_PAGE_LIST),
        RemoveSelectedRecent,
        [
            binding("backspace", Some(PICKER_PAGE_LIST)),
            binding("delete", Some(PICKER_PAGE_LIST)),
            deferred("backspace", Some(PICKER_DIALOG_LIST)),
            deferred("delete", Some(PICKER_DIALOG_LIST))
        ]
    ),
    // --- palette::bind ---------------------------------------------------
    action!(
        "strata_palette::PalettePrevious",
        "Palette: previous command",
        Palette,
        Some(PALETTE_FIELD),
        PalettePrevious,
        [binding("up", Some(PALETTE_FIELD))]
    ),
    action!(
        "strata_palette::PaletteNext",
        "Palette: next command",
        Palette,
        Some(PALETTE_FIELD),
        PaletteNext,
        [binding("down", Some(PALETTE_FIELD))]
    ),
    action!(
        "strata_palette::PaletteDismiss",
        "Close the command palette",
        Palette,
        Some(PALETTE_FIELD),
        PaletteDismiss,
        [binding("escape", Some(PALETTE_FIELD))]
    ),
    // --- the application menu, zoom and the shortcut sheet ---------------
    // These are their own groups rather than more `Actions` rows: the
    // shortcut sheet lists actions under their group, and the menu items and
    // window-wide commands belong together.
    action!(
        "strata::AboutStrata",
        "About Strata",
        Application,
        None,
        AboutStrata,
        [],
        42,
        false
    ),
    action!(
        "strata_about::CloseAbout",
        "Close About",
        Application,
        Some(ABOUT_VIEW),
        CloseAbout,
        [binding("escape", Some(ABOUT_VIEW))]
    ),
    action!(
        "strata::ZoomIn",
        "Zoom in",
        Zoom,
        None,
        ZoomIn,
        [binding("secondary-=", None), binding("secondary-+", None)],
        38,
        false
    ),
    action!(
        "strata::ZoomOut",
        "Zoom out",
        Zoom,
        None,
        ZoomOut,
        [binding("secondary--", None)],
        39,
        false
    ),
    action!(
        "strata::ResetZoom",
        "Reset zoom",
        Zoom,
        None,
        ResetZoom,
        [binding("secondary-0", None)],
        40,
        false
    ),
    action!(
        "strata::ShowKeyboardShortcuts",
        "Keyboard shortcuts",
        Shortcuts,
        None,
        ShowKeyboardShortcuts,
        [binding("secondary-/", None)],
        41,
        false
    ),
    action!(
        "strata::DismissShortcuts",
        "Close keyboard shortcuts",
        Shortcuts,
        Some(SHORTCUTS_FIELD),
        DismissShortcuts,
        [binding("escape", Some(SHORTCUTS_FIELD))]
    ),
    // --- terminal_panel::bind --------------------------------------------
    #[cfg(feature = "terminal")]
    action!(
        "strata::NewTerminalTab",
        "New terminal tab",
        Terminal,
        Some(TERMINAL_PANEL),
        NewTerminalTab,
        [binding("ctrl-shift-n", Some(TERMINAL_PANEL))]
    ),
    #[cfg(feature = "terminal")]
    action!(
        "strata::CloseTerminalTab",
        "Close terminal tab",
        Terminal,
        Some(TERMINAL_PANEL),
        CloseTerminalTab,
        [binding("ctrl-shift-w", Some(TERMINAL_PANEL))]
    ),
    #[cfg(feature = "terminal")]
    action!(
        "strata::NextTerminalTab",
        "Next terminal tab",
        Terminal,
        Some(TERMINAL_PANEL),
        NextTerminalTab,
        [binding("ctrl-shift-right", Some(TERMINAL_PANEL))]
    ),
    #[cfg(feature = "terminal")]
    action!(
        "strata::PreviousTerminalTab",
        "Previous terminal tab",
        Terminal,
        Some(TERMINAL_PANEL),
        PreviousTerminalTab,
        [binding("ctrl-shift-left", Some(TERMINAL_PANEL))]
    ),
];

/// Looks up an action by its full name or its short one, ignoring case.
pub fn resolve(name: &str) -> Option<&'static ActionSpec> {
    let wanted = name.trim();
    ACTIONS.iter().find(|spec| {
        spec.name.eq_ignore_ascii_case(wanted) || short_name(spec.name).eq_ignore_ascii_case(wanted)
    })
}

/// `strata::SwitchBranch` -> `SwitchBranch`.
pub fn short_name(full: &str) -> &str {
    full.rsplit("::").next().unwrap_or(full)
}

/// The action's title as the palette and the settings table show it. The
/// registry keeps the English title for the keymap snapshot and for
/// `commands.json`; the catalogue is consulted when the text is drawn.
pub fn display_title(spec: &ActionSpec) -> String {
    crate::i18n::action_title(spec.name, spec.title)
}

fn context_predicate(context: Option<&str>) -> Option<Rc<KeyBindingContextPredicate>> {
    context.map(|context| {
        KeyBindingContextPredicate::parse(context)
            .expect("registry context predicates are valid")
            .into()
    })
}

fn applies(binding: &DefaultBinding, os: OperatingSystem) -> bool {
    !binding.macos_only || os == OperatingSystem::Mac
}

/// Registers one group's defaults. Called by the owning module's `bind`.
pub fn bind_group(cx: &mut App, group: Group) {
    let os = cx.platform_info().os;
    let specs = || ACTIONS.iter().filter(move |spec| spec.group == group);
    let mut bindings: Vec<KeyBinding> = Vec::new();
    for spec in specs() {
        for binding in spec.defaults.iter().filter(|binding| applies(binding, os)) {
            if !binding.deferred {
                bindings.extend(default_binding(cx, spec, binding));
            }
        }
    }
    for spec in specs() {
        for binding in spec.defaults.iter().filter(|binding| applies(binding, os)) {
            if binding.deferred {
                bindings.extend(default_binding(cx, spec, binding));
            }
        }
    }
    cx.bind_keys(bindings);
}

fn default_binding(
    cx: &App,
    spec: &'static ActionSpec,
    binding: &DefaultBinding,
) -> Option<KeyBinding> {
    KeyBinding::load(
        binding.key,
        (spec.build)(),
        context_predicate(binding.context),
        false,
        None,
        cx,
    )
    .map_err(|error| log::warn!("bad default for {}: {error}", spec.name))
    .ok()
}

/// Registers every default again, in the original order.
pub fn install_defaults(cx: &mut App) {
    for group in GROUP_ORDER {
        bind_group(cx, group);
    }
}

/// The keymap file next to `settings.json`.
pub fn keymap_file() -> Option<PathBuf> {
    settings::config_file("keymap.json")
}

/// One entry of `keymap.json`. A missing or null action unbinds the keystroke.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserBinding {
    pub key: String,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
}

/// What a load of `keymap.json` found.
#[derive(Debug, Default, Clone)]
pub struct LoadedKeymap {
    pub entries: Vec<UserBinding>,
    /// Problems that keep an entry from being applied, or that name the line
    /// the file could not be parsed at.
    pub errors: Vec<String>,
    /// Entries that were understood but skipped, such as unknown actions.
    pub warnings: Vec<String>,
}

/// Parses `keymap.json`.
pub fn parse(content: &str) -> LoadedKeymap {
    let mut loaded = LoadedKeymap::default();
    let value: serde_json::Value = match serde_json::from_str(content) {
        Ok(value) => value,
        Err(error) => {
            loaded
                .errors
                .push(t!("keymap.parse_line", line = error.line(), error = error).to_string());
            return loaded;
        }
    };
    let Some(entries) = value.as_array() else {
        loaded.errors.push(t!("keymap.not_array").to_string());
        return loaded;
    };
    for (index, entry) in entries.iter().enumerate() {
        let number = index + 1;
        let Some(object) = entry.as_object() else {
            loaded
                .errors
                .push(t!("keymap.entry_object", number = number).to_string());
            continue;
        };
        let key = match object.get("key").and_then(|value| value.as_str()) {
            Some(key) => key.to_owned(),
            None => {
                loaded
                    .errors
                    .push(t!("keymap.entry_key", number = number).to_string());
                continue;
            }
        };
        let action = match object.get("action") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(action)) => Some(action.clone()),
            Some(_) => {
                loaded
                    .errors
                    .push(t!("keymap.entry_action", number = number).to_string());
                continue;
            }
        };
        let context = match object.get("context") {
            None => None,
            Some(serde_json::Value::String(context)) => Some(context.clone()),
            Some(_) => {
                loaded
                    .errors
                    .push(t!("keymap.entry_context", number = number).to_string());
                continue;
            }
        };
        loaded.entries.push(UserBinding {
            key,
            action,
            context,
        });
    }
    loaded
}

/// Keeps the entries the app can apply, naming the ones it cannot.
pub fn validate(loaded: &mut LoadedKeymap, os: OperatingSystem) {
    let entries = std::mem::take(&mut loaded.entries);
    for entry in entries {
        if Keystroke::parse_with_os(&entry.key, os).is_err() {
            loaded
                .warnings
                .push(t!("keymap.not_keystroke", key = entry.key).to_string());
            continue;
        }
        if let Some(context) = entry.context.as_deref()
            && KeyBindingContextPredicate::parse(context).is_err()
        {
            loaded
                .warnings
                .push(t!("keymap.not_context", context = context).to_string());
            continue;
        }
        match entry.action.as_deref() {
            // An unbind names no action, so nothing else has to resolve.
            None => loaded.entries.push(entry),
            Some(action) if resolve(action).is_some() => loaded.entries.push(entry),
            Some(action) => loaded
                .warnings
                .push(t!("keymap.unknown_action", action = action).to_string()),
        }
    }
}

/// The loaded user keymap and what the last load reported.
#[derive(Default)]
pub struct KeymapState {
    pub path: Option<PathBuf>,
    pub entries: Vec<UserBinding>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// Action whose next keystroke is being recorded, if any.
    pub pending_edit: Option<&'static ActionSpec>,
    /// Modified time the poller last applied.
    modified: Option<SystemTime>,
    /// The global keystroke interceptor for Edit; dropping it would stop the
    /// next keystroke from reaching the capture.
    _capture: Option<gpui::Subscription>,
}

impl Global for KeymapState {}

impl KeymapState {
    pub fn get(cx: &App) -> &KeymapState {
        cx.global::<KeymapState>()
    }

    pub fn get_mut(cx: &mut App) -> &mut KeymapState {
        cx.global_mut::<KeymapState>()
    }

    /// Every problem worth telling the user about, one line each.
    pub fn problems(&self) -> Vec<String> {
        self.errors
            .iter()
            .chain(self.warnings.iter())
            .cloned()
            .collect()
    }
}

/// Reads `keymap.json` into the global, without applying it.
pub fn init(cx: &mut App) {
    cx.set_global(KeymapState::default());
    load(cx, true);
    install_capture(cx);
}

fn load(cx: &mut App, report: bool) {
    let Some(path) = keymap_file() else {
        return;
    };
    let os = cx.platform_info().os;
    let modified = std::fs::metadata(&path)
        .ok()
        .and_then(|meta| meta.modified().ok());
    let mut loaded = match std::fs::read_to_string(&path) {
        Ok(content) => parse(&content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LoadedKeymap::default(),
        Err(error) => {
            let mut loaded = LoadedKeymap::default();
            loaded
                .errors
                .push(t!("keymap.read_failed", error = error).to_string());
            loaded
        }
    };
    validate(&mut loaded, os);
    if report {
        for problem in loaded.errors.iter().chain(loaded.warnings.iter()) {
            log::warn!("{}: {problem}", path.display());
        }
    }
    let state = cx.global_mut::<KeymapState>();
    state.path = Some(path);
    state.entries = loaded.entries;
    state.errors = loaded.errors;
    state.warnings = loaded.warnings;
    state.modified = modified;
}

/// The user's keys for `spec`, in file order.
pub fn user_keys(state: &KeymapState, spec: &ActionSpec, os: OperatingSystem) -> Vec<String> {
    state
        .entries
        .iter()
        .filter(|entry| {
            entry
                .action
                .as_deref()
                .is_some_and(|action| resolve(action).is_some_and(|found| found.name == spec.name))
        })
        .map(|entry| display_key(&entry.key, os))
        .collect()
}

/// The keystrokes that run `spec` right now: the defaults the user has not
/// removed, then the user's own, each key once.
///
/// The shortcut sheet draws one cap per keystroke, so a key a user added on
/// top of a default must not read as two shortcuts.
pub fn effective_keystrokes(
    state: &KeymapState,
    spec: &'static ActionSpec,
    os: OperatingSystem,
) -> Vec<Keystroke> {
    let unbound = |binding: &DefaultBinding| {
        state.entries.iter().any(|entry| {
            entry.action.is_none()
                && Keystroke::parse_with_os(&entry.key, os).ok()
                    == Keystroke::parse_with_os(binding.key, os).ok()
                && contexts_overlap(entry.context.as_deref(), binding.context)
        })
    };
    let mut keys: Vec<Keystroke> = Vec::new();
    let mut add = |key: &str| {
        if let Ok(keystroke) = Keystroke::parse_with_os(key, os)
            && !keys.contains(&keystroke)
        {
            keys.push(keystroke);
        }
    };
    for binding in spec.defaults.iter().filter(|binding| applies(binding, os)) {
        if !unbound(binding) {
            add(binding.key);
        }
    }
    for entry in &state.entries {
        if entry
            .action
            .as_deref()
            .and_then(resolve)
            .is_some_and(|found| found.name == spec.name)
        {
            add(&entry.key);
        }
    }
    keys
}

fn display_key(key: &str, os: OperatingSystem) -> String {
    Keystroke::parse_with_os(key, os)
        .map(|keystroke| keystroke.display(os).to_string())
        .unwrap_or_else(|_| key.to_owned())
}

/// A screen of the keymap table.
#[derive(Debug, Clone)]
pub struct ActionRow {
    /// The registry entry the row edits.
    pub spec: &'static ActionSpec,
    pub name: &'static str,
    pub title: &'static str,
    pub context: String,
    pub defaults: String,
    pub user: String,
    pub effective: String,
    pub changed: bool,
    /// The user removed every default binding for this action.
    pub unbound: bool,
    /// Another action that already uses one of these keys in an overlapping
    /// context.
    pub conflict: Option<&'static str>,
}

/// Builds the rows the Keyboard settings page shows, including conflicts
/// between the effective bindings.
pub fn rows(os: OperatingSystem, state: &KeymapState) -> Vec<ActionRow> {
    // A null entry removes whichever defaults share its keystroke and an
    // overlapping context, so the table shows them gone.
    let unbound = |binding: &DefaultBinding| {
        state.entries.iter().any(|entry| {
            entry.action.is_none()
                && Keystroke::parse_with_os(&entry.key, os).ok()
                    == Keystroke::parse_with_os(binding.key, os).ok()
                && contexts_overlap(entry.context.as_deref(), binding.context)
        })
    };
    let mut effective: Vec<Effective> = Vec::new();
    for spec in ACTIONS {
        for binding in spec.defaults.iter().filter(|binding| applies(binding, os)) {
            if unbound(binding) {
                continue;
            }
            effective.push(Effective {
                key: display_key(binding.key, os),
                context: binding.context,
                name: spec.name,
                from_user: false,
            });
        }
    }
    for entry in &state.entries {
        if let Some(action) = entry.action.as_deref()
            && let Some(spec) = resolve(action)
            && Keystroke::parse_with_os(&entry.key, os).is_ok()
        {
            effective.push(Effective {
                key: display_key(&entry.key, os),
                context: entry.context.as_deref(),
                name: spec.name,
                from_user: true,
            });
        }
    }

    ACTIONS
        .iter()
        .map(|spec| {
            let active_defaults: Vec<&DefaultBinding> = spec
                .defaults
                .iter()
                .filter(|binding| applies(binding, os))
                .collect();
            let unbound_all = !active_defaults.is_empty()
                && active_defaults.iter().all(|binding| unbound(binding));
            let defaults: Vec<String> = active_defaults
                .iter()
                .filter(|binding| !unbound(binding))
                .map(|binding| display_key(binding.key, os))
                .collect();
            let user = user_keys(state, spec, os);
            let keys: Vec<String> = defaults.iter().chain(user.iter()).cloned().collect();
            let conflict = effective
                .iter()
                .filter(|entry| entry.name == spec.name)
                .find_map(|mine| {
                    effective
                        .iter()
                        .find(|other| {
                            (mine.from_user || other.from_user)
                                && other.name != mine.name
                                && other.key == mine.key
                                && contexts_overlap(other.context, mine.context)
                        })
                        .map(|other| short_name(other.name))
                });
            ActionRow {
                spec,
                name: short_name(spec.name),
                title: spec.title,
                context: spec
                    .context
                    .map(str::to_owned)
                    .unwrap_or_else(|| t!("keymap.context_everywhere").to_string()),
                defaults: if defaults.is_empty() {
                    "—".to_owned()
                } else {
                    defaults.join(", ")
                },
                user: user.join(", "),
                effective: if keys.is_empty() {
                    "—".to_owned()
                } else {
                    keys.join(", ")
                },
                changed: !user.is_empty() || unbound_all,
                unbound: unbound_all && user.is_empty(),
                conflict,
            }
        })
        .collect()
}

struct Effective<'a> {
    key: String,
    context: Option<&'a str>,
    name: &'static str,
    /// A default colliding with another default is a deliberate layered
    /// binding (search takes `up`/`down` while a filter is open); only a
    /// collision the user introduced is worth a warning.
    from_user: bool,
}

/// Whether two key contexts can be active at the same time.
///
/// A binding with no context is always active. Otherwise the first element of
/// the predicate is the surface (`RepoView`, `Workspace`, …): a `RepoView`
/// binding and a `RepoView && !Input` binding can both fire, while a
/// `Workspace` binding cannot be active inside the settings window.
fn contexts_overlap(a: Option<&str>, b: Option<&str>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return true;
    };
    fn surface(context: &str) -> &str {
        context.split("&&").next().unwrap_or(context).trim()
    }
    surface(a) == surface(b)
}

/// Writes one binding for `spec` and applies the file.
pub fn record_binding(
    cx: &mut App,
    spec: &'static ActionSpec,
    key: &str,
    context: Option<String>,
) -> Result<(), String> {
    let path = keymap_file().ok_or_else(|| t!("keymap.no_config_dir").to_string())?;
    let mut entries = read_entries(&path)?;
    entries.retain(|entry| !same_action(entry, spec));
    entries.push(UserBinding {
        key: key.to_owned(),
        action: Some(short_name(spec.name).to_owned()),
        context,
    });
    write_entries(&path, &entries)?;
    reload(cx);
    Ok(())
}

/// Removes the user's bindings for `spec`, so its defaults apply again.
pub fn reset_binding(cx: &mut App, spec: &'static ActionSpec) -> Result<(), String> {
    let path = keymap_file().ok_or_else(|| t!("keymap.no_config_dir").to_string())?;
    let mut entries = read_entries(&path)?;
    entries.retain(|entry| !same_action(entry, spec));
    write_entries(&path, &entries)?;
    reload(cx);
    Ok(())
}

fn same_action(entry: &UserBinding, spec: &'static ActionSpec) -> bool {
    entry
        .action
        .as_deref()
        .and_then(resolve)
        .is_some_and(|found| found.name == spec.name)
}

fn read_entries(path: &Path) -> Result<Vec<UserBinding>, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let loaded = parse(&content);
            if let Some(error) = loaded.errors.first() {
                return Err(t!(
                    "keymap.file_has_error",
                    path = path.display(),
                    error = error
                )
                .to_string());
            }
            Ok(loaded.entries)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(t!(
            "keymap.read_path_failed",
            path = path.display(),
            error = error
        )
        .to_string()),
    }
}

fn write_entries(path: &Path, entries: &[UserBinding]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            t!(
                "keymap.create_failed",
                path = parent.display(),
                error = error
            )
            .to_string()
        })?;
    }
    let json = serde_json::to_vec_pretty(entries)
        .map_err(|error| t!("keymap.encode_failed", error = error).to_string())?;
    std::fs::write(path, json).map_err(|error| {
        t!("keymap.write_failed", path = path.display(), error = error).to_string()
    })
}

/// Rewrites the keystroke a captured event produced, using `secondary-` for
/// the platform's primary modifier so the file stays portable.
pub fn keystroke_text(keystroke: &Keystroke, os: OperatingSystem) -> Option<String> {
    if keystroke.is_modifier_key() {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    let secondary = if os == OperatingSystem::Mac {
        keystroke.modifiers.platform
    } else {
        keystroke.modifiers.control
    };
    if keystroke.modifiers.function {
        parts.push("fn");
    }
    if secondary {
        parts.push("secondary");
    }
    if keystroke.modifiers.control && !secondary {
        parts.push("ctrl");
    }
    if keystroke.modifiers.alt {
        parts.push("alt");
    }
    if keystroke.modifiers.shift {
        parts.push("shift");
    }
    if keystroke.modifiers.platform && !secondary {
        parts.push("cmd");
    }
    let mut text = parts.join("-");
    if !text.is_empty() {
        text.push('-');
    }
    text.push_str(&keystroke.key);
    Some(text)
}

/// Rebuilds the keymap: every binding that is not the app's own is kept,
/// the defaults are registered again, then the custom command keys, and the
/// user's file last so it wins a tie.
///
/// GPUI has no way to remove a binding, so the map is cleared and rebuilt. The
/// component library's bindings live in the same map and are copied through;
/// clearing them would silently break menus, dialogs and text fields after the
/// first keymap edit.
pub fn reload(cx: &mut App) {
    load(cx, false);
    let foreign: Vec<KeyBinding> = {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        keymap
            .bindings()
            .filter(|binding| !is_app_binding(binding.action().name()))
            .cloned()
            .collect()
    };
    cx.clear_key_bindings();
    cx.bind_keys(foreign);
    install_defaults(cx);
    crate::commands::bind_keys(cx);
    apply_user(cx);
    // The native menu bar shows its key equivalents from the keymap, so it has
    // to be rebuilt after every change or it would keep advertising the old
    // keys.
    crate::menus::install(cx);
}

/// Whether a binding belongs to this app (and is rebuilt from the registry)
/// rather than to GPUI or gpui-component.
fn is_app_binding(name: &str) -> bool {
    let namespace = name.split_once("::").map_or("", |(namespace, _)| namespace);
    namespace == "strata" || namespace.starts_with("strata_") || name == "zed::Unbind"
}

/// Applies the loaded entries on top of whatever is already bound.
pub fn apply_user(cx: &mut App) {
    let entries = cx.global::<KeymapState>().entries.clone();
    let bindings: Vec<KeyBinding> = entries
        .iter()
        .flat_map(|entry| user_bindings(cx, entry))
        .collect();
    cx.bind_keys(bindings);
}

fn user_bindings(cx: &App, entry: &UserBinding) -> Vec<KeyBinding> {
    let os = cx.platform_info().os;
    let Ok(keystroke) = Keystroke::parse_with_os(&entry.key, os) else {
        return Vec::new();
    };
    if keystroke.is_modifier_key() {
        return Vec::new();
    }
    let context = context_predicate(entry.context.as_deref());
    match entry.action.as_deref() {
        Some(action) => {
            let Some(spec) = resolve(action) else {
                return Vec::new();
            };
            KeyBinding::load(&entry.key, (spec.build)(), context, false, None, cx)
                .map_err(|error| log::warn!("bad user key {}: {error}", entry.key))
                .into_iter()
                .collect()
        }
        // A null action removes the defaults bound to that keystroke in an
        // overlapping context. Naming them lets the same file be reloaded
        // without leaving an unremovable wildcard in the keymap.
        None => ACTIONS
            .iter()
            .filter(|spec| {
                spec.defaults.iter().any(|binding| {
                    Keystroke::parse_with_os(binding.key, os).ok() == Some(keystroke.clone())
                        && contexts_overlap(entry.context.as_deref(), binding.context)
                })
            })
            .filter_map(|spec| {
                KeyBinding::load(
                    &entry.key,
                    Box::new(Unbind(SharedString::from(spec.name))),
                    context.clone(),
                    false,
                    None,
                    cx,
                )
                .ok()
            })
            .collect(),
    }
}

/// Watches `keymap.json` and reloads it after it settles.
pub fn watch(cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut last = cx.update(|cx| cx.global::<KeymapState>().modified);
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(700))
                .await;
            let Some(modified) = cx.update(|_| current_modified()) else {
                return;
            };
            if last == Some(modified) {
                continue;
            }
            last = Some(modified);
            // A save is several filesystem events; the poller only reacts to
            // the settled state, so an editor writing through a temp file
            // does not load a half-written keymap.
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            cx.update(|cx| {
                reload(cx);
                report_to_window(cx);
            });
        }
    })
    .detach();
}

fn current_modified() -> Option<SystemTime> {
    keymap_file()
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok())
}

/// Shows the load problems in the active window, if there is one.
pub fn report_to_window(cx: &mut App) {
    let problems = KeymapState::get(cx).problems();
    if problems.is_empty() {
        return;
    }
    let Some(handle) = cx.active_window() else {
        return;
    };
    let message = problems.join("\n");
    let _ = handle.update(cx, |_, window, cx| {
        window.push_notification(
            gpui_component::notification::Notification::warning(message),
            cx,
        );
    });
}

/// Records the next keystroke an action's Edit button asked for.
fn install_capture(cx: &mut App) {
    let capture = cx.intercept_keystrokes(move |event, window, cx| {
        let Some(spec) = cx.global_mut::<KeymapState>().pending_edit.take() else {
            return;
        };
        // Every keystroke belongs to the capture until it produces one.
        cx.stop_propagation();
        if event.keystroke.is_modifier_key() {
            cx.global_mut::<KeymapState>().pending_edit = Some(spec);
            return;
        }
        // An IME still has to compose: its keys are not bindings, and capturing
        // "k" from the preedit of "か" would bind a key the user never chose.
        if is_ime_composition(&event.keystroke) {
            cx.global_mut::<KeymapState>().pending_edit = Some(spec);
            return;
        }
        if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
            cx.refresh_windows();
            return;
        }
        let os = cx.platform_info().os;
        let Some(key) = keystroke_text(&event.keystroke, os) else {
            cx.global_mut::<KeymapState>().pending_edit = Some(spec);
            return;
        };
        let context = spec.context.map(str::to_owned);
        let result = record_binding(cx, spec, &key, context);
        if let Err(error) = result {
            cx.global_mut::<KeymapState>().errors.push(error.clone());
            window.push_notification(gpui_component::notification::Notification::error(error), cx);
        }
        cx.refresh_windows();
    });
    cx.global_mut::<KeymapState>()._capture = Some(capture);
}

/// Asks the settings page to record a key for `spec`.
pub fn begin_capture(cx: &mut App, spec: &'static ActionSpec) {
    cx.global_mut::<KeymapState>().pending_edit = Some(spec);
    cx.refresh_windows();
}

/// True when a keystroke belongs to an IME composition rather than a binding.
///
/// Platforms deliver the keys typed during a preedit as ordinary key events:
/// X11's XIM sends the key that started the composition with no character,
/// and the IME reports its own control keys by name. Accepting those would bind
/// the romaji "k" while the candidate window composes "か". Modifiers, Enter,
/// arrows and other named keys still bind, so every real shortcut stays
/// capturable.
fn is_ime_composition(keystroke: &Keystroke) -> bool {
    const IME_KEYS: [&str; 9] = [
        "process",
        "henkan",
        "muhenkan",
        "convert",
        "nonconvert",
        "kana_mode",
        "eisu_toggle",
        "romaji",
        "codeinput",
    ];
    if keystroke.is_modifier_key() {
        return false;
    }
    if IME_KEYS.contains(&keystroke.key.as_str()) {
        return true;
    }
    // A printable key without a character is still being composed. Function
    // keys and named keys ("escape", "f12") have multi-byte names, so they are
    // not caught here. A modified key is an explicit shortcut, not composition.
    !keystroke.modifiers.modified() && keystroke.key.len() == 1 && keystroke.key_char.is_none()
}

/// Creates `keymap.json` with an empty list when it does not exist yet.
pub fn ensure_file() -> Result<PathBuf, String> {
    let path = keymap_file().ok_or_else(|| t!("keymap.no_config_dir").to_string())?;
    if !path.exists() {
        write_entries(&path, &[])?;
    }
    Ok(path)
}

/// A keystroke as the palette spells it.
pub fn shortcut(key: &str, os: OperatingSystem) -> SharedString {
    SharedString::from(display_key(key, os))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os() -> OperatingSystem {
        OperatingSystem::Linux
    }

    fn load(content: &str) -> LoadedKeymap {
        let mut loaded = parse(content);
        validate(&mut loaded, os());
        loaded
    }

    /// The bindings that existed before the registry, as `(key, action,
    /// context)`. Any change here is a change to a default the user relies on.
    #[test]
    fn the_default_keymap_is_unchanged() {
        let expected: &[(&str, &str, &str)] = &[
            ("up", "SelectPrevious", "RepoView"),
            ("k", "SelectPrevious", "RepoView && !Input"),
            ("down", "SelectNext", "RepoView"),
            ("j", "SelectNext", "RepoView && !Input"),
            ("z", "ToggleDetailsMaximized", "RepoView && !Input"),
            ("w", "ToggleWholeFile", "RepoView && !Input"),
            ("secondary-alt-s", "ToggleSidebar", "RepoView"),
            ("shift-up", "ExtendSelectionPrevious", "RepoView"),
            ("shift-down", "ExtendSelectionNext", "RepoView"),
            ("pageup", "SelectPageUp", "RepoView"),
            ("pagedown", "SelectPageDown", "RepoView"),
            ("home", "SelectFirst", "RepoView"),
            ("end", "SelectLast", "RepoView"),
            ("f12", "ToggleFrameStats", "RepoView"),
            ("secondary-shift-s", "StashChanges", "RepoView"),
            ("secondary-b", "SwitchBranch", "RepoView"),
            ("secondary-z", "UndoLastAction", "RepoView && !Input"),
            ("secondary-shift-z", "RedoLastAction", "RepoView && !Input"),
            ("alt-left", "NavigateBack", "RepoView"),
            #[cfg(target_os = "macos")]
            ("secondary-[", "NavigateBack", "RepoView"),
            ("alt-right", "NavigateForward", "RepoView"),
            #[cfg(target_os = "macos")]
            ("secondary-]", "NavigateForward", "RepoView"),
            ("secondary-down", "GoToParent", "RepoView && !Input"),
            (
                "secondary-shift-down",
                "GoToSecondParent",
                "RepoView && !Input",
            ),
            ("secondary-up", "GoToChild", "RepoView && !Input"),
            ("secondary-shift-h", "GoToHead", "RepoView && !Input"),
            ("secondary-t", "NewTab", "Workspace"),
            ("secondary-w", "CloseTab", "Workspace"),
            ("secondary-shift-t", "ReopenClosedTab", "Workspace"),
            ("ctrl-tab", "NextTab", "Workspace"),
            ("ctrl-shift-tab", "PreviousTab", "Workspace"),
            ("secondary-1", "SelectTab1", "Workspace"),
            ("secondary-2", "SelectTab2", "Workspace"),
            ("secondary-3", "SelectTab3", "Workspace"),
            ("secondary-4", "SelectTab4", "Workspace"),
            ("secondary-5", "SelectTab5", "Workspace"),
            ("secondary-6", "SelectTab6", "Workspace"),
            ("secondary-7", "SelectTab7", "Workspace"),
            ("secondary-8", "SelectTab8", "Workspace"),
            ("secondary-9", "SelectTab9", "Workspace"),
            ("secondary-,", "OpenSettings", ""),
            ("secondary-o", "OpenRepository", ""),
            ("secondary-shift-p", "TogglePalette", ""),
            ("f1", "TogglePalette", ""),
            ("secondary-q", "Quit", ""),
            ("f5", "RefreshWorkingTree", "RepoView"),
            #[cfg(feature = "terminal")]
            ("ctrl-`", "ToggleTerminal", "RepoView"),
            ("escape", "CloseSettings", "SettingsView"),
            ("secondary-w", "CloseSettings", "SettingsView"),
            ("escape", "CloseGitConfig", "GitConfigView"),
            ("secondary-w", "CloseGitConfig", "GitConfigView"),
            ("s", "StageSelected", "WorktreePanel && !Input"),
            ("u", "UnstageSelected", "WorktreePanel && !Input"),
            ("d", "DiscardSelected", "WorktreePanel && !Input"),
            ("enter", "ToggleSelected", "WorktreePanel"),
            ("secondary-a", "SelectAllFiles", "WorktreePanel"),
            ("up", "SelectPreviousFile", "WorktreePanel"),
            ("down", "SelectNextFile", "WorktreePanel"),
            ("shift-up", "ExtendPreviousFile", "WorktreePanel"),
            ("shift-down", "ExtendNextFile", "WorktreePanel"),
            ("secondary-enter", "CommitMessage", "WorktreePanel"),
            ("escape", "ShowHistory", "WorktreePanel"),
            ("secondary-shift-b", "CreateBranchFromHead", "RepoView"),
            ("enter", "ConfirmBranchDialog", "BranchDialog"),
            ("escape", "CancelBranchDialog", "BranchDialog"),
            ("up", "BranchSwitcherPrevious", "BranchSwitcher > Input"),
            ("down", "BranchSwitcherNext", "BranchSwitcher > Input"),
            ("enter", "BranchSwitcherConfirm", "BranchSwitcher > Input"),
            ("escape", "BranchSwitcherDismiss", "BranchSwitcher > Input"),
            ("o", "UseOurs", "WorktreePanel && !Input"),
            ("t", "UseTheirs", "WorktreePanel && !Input"),
            ("m", "MarkFileResolved", "WorktreePanel && !Input"),
            ("n", "NextConflict", "WorktreePanel && !Input"),
            ("p", "PreviousConflict", "WorktreePanel && !Input"),
            ("alt-1", "TakeOurs", "WorktreePanel"),
            ("alt-2", "TakeTheirs", "WorktreePanel"),
            ("alt-3", "TakeBothOursFirst", "WorktreePanel"),
            ("alt-4", "TakeBothTheirsFirst", "WorktreePanel"),
            ("alt-5", "TakeBase", "WorktreePanel"),
            ("alt-0", "ResetConflict", "WorktreePanel"),
            // The merge editor's panel-wide bindings were registered after
            // the single-letter ones.
            ("alt-n", "NextConflict", "WorktreePanel"),
            ("alt-p", "PreviousConflict", "WorktreePanel"),
            ("secondary-alt-f", "FetchRemotes", "RepoView"),
            ("secondary-alt-l", "PullBranch", "RepoView"),
            ("secondary-alt-p", "PushBranch", "RepoView"),
            ("secondary-f", "FocusSearch", "RepoView"),
            ("escape", "SearchDismiss", "RepoView"),
            ("down", "SearchNext", "RepoView"),
            ("up", "SearchPrevious", "RepoView"),
            ("enter", "SearchSubmit", "RepoView"),
            // The search field's own bindings were registered after the rest
            // of the search group.
            ("down", "SearchNext", "CommitSearch > Input"),
            ("up", "SearchPrevious", "CommitSearch > Input"),
            // The picker's dialog bindings came after its page bindings.
            ("up", "PickerPrevious", "PickerPage"),
            ("k", "PickerPrevious", "PickerPage && !Input"),
            ("down", "PickerNext", "PickerPage"),
            ("j", "PickerNext", "PickerPage && !Input"),
            ("enter", "OpenSelectedRepository", "PickerPage"),
            ("secondary-enter", "OpenSelectedInNewTab", "PickerPage"),
            ("escape", "PickerEscape", "PickerPage"),
            ("backspace", "RemoveSelectedRecent", "PickerPage && !Input"),
            ("delete", "RemoveSelectedRecent", "PickerPage && !Input"),
            ("up", "PickerPrevious", "PickerDialog"),
            ("k", "PickerPrevious", "PickerDialog && !Input"),
            ("down", "PickerNext", "PickerDialog"),
            ("j", "PickerNext", "PickerDialog && !Input"),
            (
                "backspace",
                "RemoveSelectedRecent",
                "PickerDialog && !Input",
            ),
            ("delete", "RemoveSelectedRecent", "PickerDialog && !Input"),
            ("up", "PalettePrevious", "Palette > Input"),
            ("down", "PaletteNext", "Palette > Input"),
            ("escape", "PaletteDismiss", "Palette > Input"),
            // The application menu, the zoom and the shortcut sheet.
            ("escape", "CloseAbout", "AboutView"),
            ("secondary-=", "ZoomIn", ""),
            ("secondary-+", "ZoomIn", ""),
            ("secondary--", "ZoomOut", ""),
            ("secondary-0", "ResetZoom", ""),
            ("secondary-/", "ShowKeyboardShortcuts", ""),
            ("escape", "DismissShortcuts", "ShortcutsDialog > Input"),
            #[cfg(feature = "terminal")]
            ("ctrl-shift-n", "NewTerminalTab", "TerminalPanel"),
            #[cfg(feature = "terminal")]
            ("ctrl-shift-w", "CloseTerminalTab", "TerminalPanel"),
            #[cfg(feature = "terminal")]
            ("ctrl-shift-right", "NextTerminalTab", "TerminalPanel"),
            #[cfg(feature = "terminal")]
            ("ctrl-shift-left", "PreviousTerminalTab", "TerminalPanel"),
        ];

        // The groups are stored in one array; comparing the full dump fails
        // with a readable diff when a binding is added or removed.
        // Registration order, which is what a keystroke tie is settled by:
        // each group's ordinary bindings first, then the deferred ones.
        let mut dump: Vec<(String, String, String)> = Vec::new();
        // The snapshot is spelled the way the Linux tests spawn shortcuts, but
        // whether a macOS-only default is registered depends on the host: a
        // test run on macOS has `secondary-[` in its map and one on Linux does
        // not, exactly as the app builds it.
        let platform = if cfg!(target_os = "macos") {
            OperatingSystem::Mac
        } else {
            os()
        };
        for group in GROUP_ORDER {
            for deferred in [false, true] {
                for spec in ACTIONS.iter().filter(|spec| spec.group == group) {
                    for binding in spec
                        .defaults
                        .iter()
                        .filter(|b| applies(b, platform) && b.deferred == deferred)
                    {
                        dump.push((
                            binding.key.to_owned(),
                            short_name(spec.name).to_owned(),
                            binding.context.unwrap_or("").to_owned(),
                        ));
                    }
                }
            }
        }
        let expected: Vec<(String, String, String)> = expected
            .iter()
            .map(|(key, action, context)| {
                (
                    (*key).to_owned(),
                    (*action).to_owned(),
                    (*context).to_owned(),
                )
            })
            .collect();
        assert_eq!(dump, expected);
    }

    #[test]
    fn every_registry_name_matches_its_action_and_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for spec in ACTIONS {
            let built = (spec.build)();
            assert_eq!(
                built.name(),
                spec.name,
                "{} builds {}",
                spec.name,
                built.name()
            );
            assert!(
                seen.insert(short_name(spec.name)),
                "duplicate action {}",
                spec.name
            );
            assert!(!spec.title.is_empty());
            assert!(resolve(short_name(spec.name)).is_some());
        }
    }

    #[test]
    fn palette_orders_are_unique_and_present() {
        let mut orders = std::collections::HashSet::new();
        for spec in ACTIONS {
            if let Some(slot) = spec.palette {
                assert!(orders.insert(slot.order), "{} reuses an order", spec.name);
            }
        }
        // Every built-in palette entry has a registry row.
        let expected_count = if cfg!(feature = "terminal") { 56 } else { 55 };
        assert_eq!(orders.len(), expected_count);
    }

    #[test]
    fn a_valid_file_loads_every_entry() {
        let loaded = load(
            r#"[
                {"key": "secondary-shift-b", "action": "SwitchBranch", "context": "RepoView"},
                {"key": "secondary-k", "action": "strata::StashChanges"}
            ]"#,
        );
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries[0].action.as_deref(), Some("SwitchBranch"));
    }

    #[test]
    fn a_null_action_is_an_unbind_and_needs_no_registry_entry() {
        let loaded = load(r#"[{"key": "f12", "action": null, "context": "RepoView"}]"#);
        assert!(loaded.errors.is_empty());
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.entries[0].action, None);
    }

    #[test]
    fn an_unknown_action_is_a_warning() {
        let loaded = load(r#"[{"key": "f9", "action": "FlyToTheMoon"}]"#);
        assert!(loaded.entries.is_empty());
        assert!(loaded.warnings[0].contains("unknown action `FlyToTheMoon`"));
    }

    #[test]
    fn a_bad_key_names_the_key() {
        let loaded = load(r#"[{"key": "ctrl-a-b", "action": "Quit"}]"#);
        assert!(loaded.warnings.iter().any(|line| line.contains("ctrl-a-b")));
    }

    #[test]
    fn malformed_json_names_the_line() {
        let loaded = load("[\n  {\"key\": \"a\"},\n  {\"key\": },\n]");
        assert!(!loaded.errors.is_empty());
        assert!(
            loaded.errors[0].starts_with("line 3"),
            "{:?}",
            loaded.errors
        );
    }

    #[test]
    fn secondary_resolves_to_the_platform_modifier() {
        assert_eq!(display_key("secondary-b", OperatingSystem::Linux), "ctrl-B");
        assert_eq!(display_key("secondary-b", OperatingSystem::Mac), "⌘B");
    }

    #[test]
    fn user_entries_add_to_the_defaults() {
        let mut state = KeymapState::default();
        state.entries.push(UserBinding {
            key: "secondary-shift-b".to_owned(),
            action: Some("CreateBranchFromHead".to_owned()),
            context: Some("RepoView".to_owned()),
        });
        let rows = rows(OperatingSystem::Linux, &state);
        let row = rows
            .iter()
            .find(|row| row.name == "CreateBranchFromHead")
            .unwrap();
        assert_eq!(row.defaults, "ctrl-shift-B");
        assert_eq!(row.user, "ctrl-shift-B");
        assert!(row.changed);
        assert!(row.conflict.is_none(), "the same action is not a conflict");
    }

    #[test]
    fn a_key_used_by_another_action_is_a_conflict() {
        let mut state = KeymapState::default();
        state.entries.push(UserBinding {
            key: "secondary-b".to_owned(),
            action: Some("CreateBranchFromHead".to_owned()),
            context: Some("RepoView".to_owned()),
        });
        let rows = rows(OperatingSystem::Linux, &state);
        let row = rows
            .iter()
            .find(|row| row.name == "CreateBranchFromHead")
            .unwrap();
        assert_eq!(row.conflict, Some("SwitchBranch"));
        let switch = rows.iter().find(|row| row.name == "SwitchBranch").unwrap();
        assert_eq!(switch.conflict, Some("CreateBranchFromHead"));
    }

    #[test]
    fn a_key_in_another_window_is_not_a_conflict() {
        let state = KeymapState::default();
        // Ctrl+W closes a tab in the workspace and the settings window in its
        // own context; both are defaults and neither is a conflict.
        let rows = rows(OperatingSystem::Linux, &state);
        let close_tab = rows.iter().find(|row| row.name == "CloseTab").unwrap();
        assert!(close_tab.conflict.is_none());
        let close_settings = rows.iter().find(|row| row.name == "CloseSettings").unwrap();
        assert!(close_settings.conflict.is_none());
    }

    #[gpui::test]
    fn a_user_binding_wins_over_a_default(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(KeymapState::default());
            bind_group(cx, Group::Actions);
            cx.global_mut::<KeymapState>().entries.push(UserBinding {
                key: "f12".to_owned(),
                action: Some("SwitchBranch".to_owned()),
                context: Some("RepoView".to_owned()),
            });
            apply_user(cx);
        });

        let (bindings, pending) = cx.update(|cx| {
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let contexts = [gpui::KeyContext::parse("RepoView").unwrap()];
            keymap.bindings_for_input(
                &[Keystroke::parse_with_os("f12", OperatingSystem::Linux).unwrap()],
                &contexts,
            )
        });
        assert!(!pending);
        assert_eq!(
            bindings[0].action().name(),
            "strata::SwitchBranch",
            "the user's f12 must run before the default ToggleFrameStats"
        );
    }

    #[gpui::test]
    fn a_null_entry_removes_the_default(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(KeymapState::default());
            bind_group(cx, Group::Actions);
            cx.global_mut::<KeymapState>().entries.push(UserBinding {
                key: "f12".to_owned(),
                action: None,
                context: Some("RepoView".to_owned()),
            });
            apply_user(cx);
        });

        let (bindings, _) = cx.update(|cx| {
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let contexts = [gpui::KeyContext::parse("RepoView").unwrap()];
            keymap.bindings_for_input(
                &[Keystroke::parse_with_os("f12", OperatingSystem::Linux).unwrap()],
                &contexts,
            )
        });
        assert!(
            bindings
                .iter()
                .all(|binding| binding.action().name() != "strata::ToggleFrameStats"),
            "f12 must be unbound"
        );
    }

    #[test]
    fn captured_keystrokes_use_secondary() {
        let keystroke = Keystroke::parse_with_os("ctrl-shift-b", OperatingSystem::Linux).unwrap();
        assert_eq!(
            keystroke_text(&keystroke, OperatingSystem::Linux).unwrap(),
            "secondary-shift-b"
        );
        let keystroke = Keystroke::parse_with_os("cmd-alt-s", OperatingSystem::Mac).unwrap();
        assert_eq!(
            keystroke_text(&keystroke, OperatingSystem::Mac).unwrap(),
            "secondary-alt-s"
        );
        let modifier = Keystroke::parse_with_os("shift", OperatingSystem::Linux).unwrap();
        assert_eq!(keystroke_text(&modifier, OperatingSystem::Linux), None);
    }

    /// A key capture must not bind the keys an IME is using to compose text.
    #[test]
    fn key_capture_ignores_ime_composition() {
        let mut composing = Keystroke::parse_with_os("k", OperatingSystem::Linux).unwrap();
        composing.key_char = None;
        assert!(is_ime_composition(&composing));
        assert!(
            keystroke_text(&composing, OperatingSystem::Linux).is_some(),
            "the guard is what keeps this key out of the keymap"
        );

        let mut committed = Keystroke::parse_with_os("k", OperatingSystem::Linux).unwrap();
        committed.key_char = Some("k".to_owned());
        assert!(!is_ime_composition(&committed));

        for name in ["henkan", "muhenkan", "process", "eisu_toggle"] {
            let ime = Keystroke::parse_with_os(name, OperatingSystem::Linux).unwrap_or_else(|_| {
                Keystroke {
                    modifiers: Default::default(),
                    key: name.to_owned(),
                    key_char: None,
                }
            });
            assert!(is_ime_composition(&ime), "{name} belongs to the IME");
        }

        let enter = Keystroke::parse_with_os("enter", OperatingSystem::Linux).unwrap();
        assert!(!is_ime_composition(&enter), "Enter is still capturable");
        let secondary = Keystroke::parse_with_os("ctrl-k", OperatingSystem::Linux).unwrap();
        assert!(!is_ime_composition(&secondary));
    }
}
