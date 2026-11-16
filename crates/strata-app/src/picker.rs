//! The repository picker: the welcome screen and the "Open repository" dialog.
//!
//! Both are one view, [`Picker`], told which shape to take by [`Mode`]. The
//! welcome screen is the window's content when Strata starts without a
//! repository; the dialog is opened over whatever the window already shows by
//! the `OpenRepository` action. Opening a repository is delegated to
//! [`Workspace`](crate::workspace::Workspace), which owns the content swap.
//!
//! The picker only reads files: `recent.json` through [`RecentRepositories`] and
//! cheap `gix` reads (`quick_status` / discovery) on background threads. It runs
//! no git commands.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable as _,
    FontWeight, InteractiveElement, IntoElement, KeyDownEvent, Keystroke, OperatingSystem,
    ParentElement, PathPromptOptions, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, UniformListScrollHandle, Window,
    actions, div, px, uniform_list,
};
use gpui_component::WindowExt as _;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::PopupMenuItem;
use gpui_component::{ActiveTheme as _, Icon, Sizable as _, TitleBar};
use gpui_kit_assets::IconName;
use strata_core::repo::{QuickStatus, Repo};
use strata_core::tools as core_tools;
use strata_core::{HeadInfo, RefKind, RepoState};

use crate::keymap;
use crate::recent::{RecentRepositories, RecentRepository};
use crate::scrollbar::WithScrollbar as _;
use crate::settings::Settings;
use crate::style::Design;
use crate::tools::Tooling;
use crate::ui;
use crate::workspace::Workspace;

/// Welcome screen: the whole window, and the only keyboard focus.
const PICKER_PAGE: &str = "PickerPage";
/// The same list inside the repository dialog. `Enter` and `Escape` belong to
/// the dialog there, so only list movement and deletion are bound to this context.
const PICKER_DIALOG: &str = "PickerDialog";

/// Height of one remembered or ghq repository row: a name line over a path line.
const ROW_HEIGHT: f32 = 44.0;
/// How much of the recent list the dialog shows before it scrolls.
const DIALOG_LIST_HEIGHT: f32 = 280.0;
/// Maximum height of the recent list on the welcome screen before it scrolls.
const PAGE_LIST_HEIGHT: f32 = 240.0;
/// Maximum height of the expanded ghq list before it scrolls.
const GHQ_LIST_HEIGHT: f32 = 180.0;
/// Character budget for formatted repository paths before middle-ellipsising.
const PATH_DISPLAY_MAX_CHARS: usize = 56;
/// Maximum width of a branch chip inside a picker row so long branch names
/// cannot crowd out the repository name.
const BRANCH_CHIP_MAX_WIDTH: f32 = 110.0;

actions!(
    strata,
    [
        /// Moves the picker's selection up one repository.
        PickerPrevious,
        /// Moves the picker's selection down one repository.
        PickerNext,
        /// Opens the selected repository, or the typed path.
        OpenSelectedRepository,
        /// Opens the selected repository, or the typed path, in a new tab.
        OpenSelectedInNewTab,
        /// Returns focus from the search field to the picker list.
        PickerEscape,
        /// Removes the selected recent repository from the recent list.
        RemoveSelectedRecent,
    ]
);

/// Binds the picker's own commands. `Ctrl+O` itself is bound globally by
/// `actions::bind` and handled by the workspace around both shapes.
pub fn bind(cx: &mut App) {
    crate::keymap::bind_group(cx, crate::keymap::Group::Picker);
}

/// Which shape the picker takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The welcome screen that fills the window.
    Page,
    /// The dialog opened over an existing view.
    Dialog,
}

/// Where the chosen repository should open by default when submitting the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum OpenTarget {
    /// Replace the active repository view in this window (or open the first tab).
    #[default]
    Current,
    /// Add a new tab in this window (or switch to it if already open).
    NewTab,
}

/// Per-path background status read via `strata_core::repo::quick_status`.
#[derive(Debug, Clone)]
enum RepoQuickState {
    Pending,
    Loaded(QuickStatus),
    NotFound,
    Unavailable,
}

/// The ghq repositories the picker can open.
///
/// The list is loaded on the background executor and filtered once per query
/// change, so a `ghq list -p` with thousands of entries never scans the paths
/// on every frame. `uniform_list` only builds the visible rows.
#[derive(Default)]
struct GhqState {
    paths: Rc<Vec<PathBuf>>,
    /// Indices into `paths`, best fuzzy match first.
    filtered: Rc<Vec<usize>>,
    loading: bool,
    error: Option<String>,
    visible: bool,
}

/// A directory chooser for one repository.
pub struct Picker {
    mode: Mode,
    target: OpenTarget,
    workspace: Entity<Workspace>,
    entity: Entity<Picker>,
    recents: RecentRepositories,
    /// Cached per-path status for `recents`, populated off the UI thread.
    quick_statuses: HashMap<PathBuf, RepoQuickState>,
    /// Filters the list, or holds a filesystem path when the text looks like one.
    input: Entity<InputState>,
    /// Position across the visible rows (recent, then expanded ghq).
    selected: usize,
    error: Option<String>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    ghq: GhqState,
    ghq_expanded: bool,
    ghq_scroll: UniformListScrollHandle,
    ghq_generation: u64,
    _input_events: Subscription,
    _tooling: Subscription,
}

impl Picker {
    pub(crate) fn new(
        mode: Mode,
        target: OpenTarget,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The view's own handle is only where `Escape` returns the keyboard;
        // the field itself is what has to be focused for typing, and the
        // window does that once this picker is in the tree.
        let focus = cx.focus_handle();
        // A fresh read: a repository may have been opened since the last picker.
        let recents = RecentRepositories::load_default();
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("picker.filter").to_string()));
        let _input_events =
            cx.subscribe(&input, |this, _input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    // The list narrowed (or the typed path changed); the old
                    // position no longer points at what the user is looking at.
                    this.selected = 0;
                    this.error = None;
                    this.refresh_ghq_filter(cx);
                    cx.notify();
                }
                InputEvent::PressEnter { secondary, .. } => {
                    // The input owns `Enter` while it has focus, so the picker
                    // submits from here instead of from its own binding.
                    let target = if *secondary {
                        OpenTarget::NewTab
                    } else {
                        this.target
                    };
                    let Some(handle) = cx.active_window() else {
                        return;
                    };
                    let mut failure = None;
                    let _ =
                        handle.update(cx, |_, window, cx| match this.open_in(target, window, cx) {
                            Ok(()) => {}
                            Err(message) => failure = Some(message),
                        });
                    if let Some(message) = failure {
                        this.error = Some(message);
                    }
                    cx.notify();
                }
                _ => {}
            });
        let tooling = cx.observe_global::<Tooling>(|this, cx| this.reload_ghq(cx));
        let mut picker = Self {
            mode,
            target,
            workspace,
            entity: cx.entity(),
            recents,
            quick_statuses: HashMap::new(),
            input,
            selected: 0,
            error: None,
            focus,
            scroll: UniformListScrollHandle::default(),
            ghq: GhqState::default(),
            ghq_expanded: false,
            ghq_scroll: UniformListScrollHandle::default(),
            ghq_generation: 0,
            _input_events,
            _tooling: tooling,
        };
        picker.load_recent_statuses(cx);
        picker.reload_ghq(cx);
        picker
    }

    /// The picker's text field: the window focuses it so typing goes to the
    /// filter (or typed path) without a click.
    pub(crate) fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    /// The focus handle of that field, for a view that only has the handle.
    pub(crate) fn input_focus(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }

    /// Reads cheap `QuickStatus` for each recent repository in the background so
    /// row rendering never blocks on filesystem or repository inspection.
    fn load_recent_statuses(&mut self, cx: &mut Context<Self>) {
        for entry in self.recents.entries() {
            let path = entry.path.clone();
            if crate::remote_open::is_stored_remote(&path) {
                continue;
            }
            if self.quick_statuses.contains_key(&path) {
                continue;
            }
            self.quick_statuses
                .insert(path.clone(), RepoQuickState::Pending);
            let read_path = path.clone();
            let task = cx.background_executor().spawn(async move {
                if !read_path.exists() {
                    return RepoQuickState::NotFound;
                }
                match strata_core::repo::quick_status(&read_path) {
                    Ok(status) => RepoQuickState::Loaded(status),
                    Err(_) => RepoQuickState::Unavailable,
                }
            });
            cx.spawn(async move |this, cx| {
                let state = task.await;
                let _ = this.update(cx, |picker, cx| {
                    picker.quick_statuses.insert(path, state);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Loads `ghq list -p` off the UI thread. Re-runs whenever detection
    /// finishes or the tool settings change, and never when the tool is off.
    fn reload_ghq(&mut self, cx: &mut Context<Self>) {
        self.ghq_generation += 1;
        let generation = self.ghq_generation;
        self.ghq.loading = false;
        self.ghq.error = None;

        let Some(tooling) = Tooling::get(cx) else {
            self.clear_ghq(cx);
            return;
        };
        if !Settings::get(cx).tools.ghq.enabled {
            self.clear_ghq(cx);
            return;
        }
        let Some(ghq) = tooling.ghq().cloned() else {
            self.clear_ghq(cx);
            return;
        };

        let root = Settings::get(cx).tools.ghq.root.clone();
        let binary = ghq.binary;
        self.ghq.visible = true;
        self.ghq.loading = true;
        cx.notify();
        let task = cx
            .background_executor()
            .spawn(async move { core_tools::ghq_list(&binary, root.as_deref()) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |picker, cx| {
                if picker.ghq_generation != generation {
                    return;
                }
                picker.ghq.loading = false;
                match result {
                    Ok(paths) => {
                        picker.ghq.paths = Rc::new(paths);
                        picker.ghq.error = None;
                    }
                    Err(error) => {
                        picker.ghq.paths = Rc::default();
                        picker.ghq.error = Some(error.to_string());
                    }
                }
                picker.refresh_ghq_filter(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn clear_ghq(&mut self, cx: &mut Context<Self>) {
        self.ghq = GhqState::default();
        cx.notify();
    }

    /// Re-ranks the ghq paths with the same subsequence matcher the palette
    /// uses.
    fn refresh_ghq_filter(&mut self, cx: &App) {
        if self.ghq.paths.is_empty() {
            self.ghq.filtered = Rc::default();
            return;
        }
        let query = self.input.read(cx).value().to_string();
        let labels: Vec<String> = self
            .ghq
            .paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let candidates: Vec<crate::palette::Candidate<'_>> = labels
            .iter()
            .map(|label| crate::palette::Candidate {
                label,
                alias: None,
                visible: true,
                recent: None,
            })
            .collect();
        self.ghq.filtered = Rc::new(crate::palette::rank(&candidates, &query));
    }

    fn has_query(&self, cx: &App) -> bool {
        !self.input.read(cx).value().trim().is_empty()
    }

    fn ghq_rows_expanded(&self, cx: &App) -> bool {
        self.ghq.visible && (self.ghq_expanded || self.has_query(cx))
    }

    fn visible_ghq_count(&self, cx: &App) -> usize {
        if self.ghq_rows_expanded(cx) {
            self.ghq.filtered.len()
        } else {
            0
        }
    }

    /// The recent repositories matching the search field.
    fn filtered(&self, cx: &App) -> Vec<usize> {
        let query = self.input.read(cx).value().to_string();
        filter_indices(self.recents.entries(), &query)
    }

    fn row_count(&self, cx: &App) -> usize {
        self.filtered(cx).len() + self.visible_ghq_count(cx)
    }

    fn selected_path(&self, cx: &App) -> Option<PathBuf> {
        let recents = self.filtered(cx);
        if self.selected < recents.len() {
            let index = *recents.get(self.selected)?;
            return self.recents.get(index).map(|entry| entry.path.clone());
        }
        if !self.ghq_rows_expanded(cx) {
            return None;
        }
        let position = self.selected - recents.len();
        let index = *self.ghq.filtered.get(position)?;
        self.ghq.paths.get(index).cloned()
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let recents = self.filtered(cx).len();
        let len = recents + self.visible_ghq_count(cx);
        if len == 0 {
            return;
        }
        self.selected = move_selection(self.selected, delta, len);
        if self.selected < recents {
            self.scroll
                .scroll_to_item(self.selected, ScrollStrategy::Center);
        } else {
            self.ghq_scroll
                .scroll_to_item(self.selected - recents, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// Resolves what the picker should open and hands it to the workspace.
    /// `Err` is a message for the user, and the dialog stays open to show it.
    fn open_in(
        &mut self,
        target: OpenTarget,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        let raw = self.input.read(cx).value().to_string();
        let path = if looks_like_path(&raw) {
            expand_home_path(&raw, home_dir().as_deref())
        } else {
            self.selected_path(cx)
                .ok_or_else(|| t!("picker.choose_first").to_string())?
        };
        self.open_path_in(path, target, window, cx)
    }

    fn open_path_in(
        &mut self,
        path: PathBuf,
        target: OpenTarget,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        self.workspace.update(cx, |workspace, cx| match target {
            OpenTarget::Current => workspace.open_path(&path, window, cx),
            OpenTarget::NewTab => workspace.open_path_in_new_tab(&path, window, cx),
        })
    }

    /// `Enter` from the welcome screen or the dialog's confirm button. Returns
    /// whether a repository was opened; the dialog closes only when one was.
    pub(crate) fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.open_selected_in(self.target, window, cx)
    }

    fn open_selected_in(
        &mut self,
        target: OpenTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.open_in(target, window, cx) {
            // The workspace closes the dialog and focuses the new view once the
            // swap is done, so this entity may be dropped right after.
            Ok(()) => true,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                false
            }
        }
    }

    fn open_row_in(
        &mut self,
        position: usize,
        target: OpenTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = position;
        let Some(path) = self
            .filtered(cx)
            .get(position)
            .copied()
            .and_then(|index| self.recents.get(index))
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        match self.open_path_in(path, target, window, cx) {
            Ok(()) => {}
            Err(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }

    fn open_row_in_new_window(
        &mut self,
        position: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = position;
        let Some(path) = self
            .filtered(cx)
            .get(position)
            .copied()
            .and_then(|index| self.recents.get(index))
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        if let Err(message) = crate::remote_open::parse_stored_strict(&path) {
            self.error = Some(message);
            cx.notify();
            return;
        }
        if crate::remote_open::is_stored_remote(&path) {
            crate::workspace::open_new_window(path, cx);
            if self.mode == Mode::Dialog && window.has_active_dialog(cx) {
                window.defer(cx, |window, cx| window.close_dialog(cx));
            }
            return;
        }
        match resolve_repository(&path) {
            Ok(root) => {
                crate::workspace::open_new_window(root, cx);
                if self.mode == Mode::Dialog && window.has_active_dialog(cx) {
                    window.defer(cx, |window, cx| window.close_dialog(cx));
                }
            }
            Err(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }

    fn open_ghq_row_in(
        &mut self,
        position: usize,
        target: OpenTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let recents = self.filtered(cx).len();
        self.selected = recents + position;
        let Some(path) = self
            .ghq
            .filtered
            .get(position)
            .copied()
            .and_then(|index| self.ghq.paths.get(index))
            .cloned()
        else {
            return;
        };
        match self.open_path_in(path, target, window, cx) {
            Ok(()) => {}
            Err(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }

    fn open_ghq_row_in_new_window(
        &mut self,
        position: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let recents = self.filtered(cx).len();
        self.selected = recents + position;
        let Some(path) = self
            .ghq
            .filtered
            .get(position)
            .copied()
            .and_then(|index| self.ghq.paths.get(index))
            .cloned()
        else {
            return;
        };
        match resolve_repository(&path) {
            Ok(root) => {
                crate::workspace::open_new_window(root, cx);
                if self.mode == Mode::Dialog && window.has_active_dialog(cx) {
                    window.defer(cx, |window, cx| window.close_dialog(cx));
                }
            }
            Err(message) => {
                self.error = Some(message);
                cx.notify();
            }
        }
    }

    /// Opens the platform's directory chooser using the picker's default target.
    pub(crate) fn open_native(&mut self, window: &mut Window, cx: &mut App) {
        self.open_native_in(self.target, window, cx);
    }

    /// Opens the clone dialog with this picker's target, so a clone started
    /// from a new-tab picker lands in a new tab.
    ///
    /// The picker's own dialog closes first: two stacked dialogs would leave
    /// the older one behind once the repository opens.
    pub(crate) fn open_clone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_creator(window, cx, |workspace, target, window, cx| {
            crate::clone_dialog::open_clone(workspace, target, window, cx);
        });
    }

    /// Opens the init dialog with this picker's target.
    pub(crate) fn open_init(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_creator(window, cx, |workspace, target, window, cx| {
            crate::clone_dialog::open_init(workspace, target, window, cx);
        });
    }

    /// Opens the standalone Compare setup dialog on the picker's workspace.
    pub(crate) fn open_compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_creator(window, cx, |workspace, _, window, cx| {
            let options = workspace.read(cx).revision_options(cx);
            crate::compare::open_compare_dialog(workspace, options, window, cx);
        });
    }

    fn open_creator(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        open: impl FnOnce(Entity<Workspace>, OpenTarget, &mut Window, &mut App) + 'static,
    ) {
        let workspace = self.workspace.clone();
        let target = self.target;
        if self.mode == Mode::Dialog {
            window.close_dialog(cx);
            window.defer(cx, move |window, cx| open(workspace, target, window, cx));
        } else {
            open(workspace, target, window, cx);
        }
    }

    /// Opens the platform's directory chooser and opens the result in `target`.
    /// Its answer arrives on a later frame, so the picker stays as it is until then.
    pub(crate) fn open_native_in(&mut self, target: OpenTarget, window: &mut Window, cx: &mut App) {
        let prompt = match target {
            OpenTarget::Current => t!("picker.open_repository").to_string(),
            OpenTarget::NewTab => t!("picker.open_new_tab").to_string(),
        };
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        let handle = window.window_handle();
        let weak = self.entity.downgrade();
        cx.spawn(async move |cx| match receiver.await {
            Ok(Ok(Some(mut paths))) => {
                let Some(path) = paths.pop() else {
                    return;
                };
                let _ = handle.update(cx, |_, window, cx| {
                    let _ = weak.update(cx, |picker, cx| {
                        match picker.open_path_in(path, target, window, cx) {
                            Ok(()) => {}
                            Err(message) => {
                                picker.error = Some(message);
                                cx.notify();
                            }
                        }
                    });
                });
            }
            // Cancelled; nothing to report.
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                let _ = weak.update(cx, |picker, cx| {
                    picker.error = Some(t!("picker.err.chooser_failed", error = error).to_string());
                    cx.notify();
                });
            }
            Err(_) => {}
        })
        .detach();
    }

    /// Focuses the unified search-or-path field and seeds `~/` for path entry.
    fn focus_path_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let focus = self.input.focus_handle(cx);
        self.input.update(cx, |state, cx| {
            state.set_value("~/", window, cx);
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Forgets one row of the filtered list and saves the shorter list.
    fn remove(&mut self, position: usize, cx: &mut Context<Self>) {
        let Some(index) = self.filtered(cx).get(position).copied() else {
            return;
        };
        let Some(path) = self.recents.get(index).map(|entry| entry.path.clone()) else {
            return;
        };
        self.recents.remove(&path);
        if let Err(error) = self.recents.save_default() {
            log::warn!("cannot save the recent-repository list: {error}");
        }
        let len = self.row_count(cx);
        self.selected = self.selected.min(len.saturating_sub(1));
        cx.notify();
    }

    fn remove_selected(&mut self, cx: &mut Context<Self>) {
        let recents_len = self.filtered(cx).len();
        if self.selected < recents_len {
            self.remove(self.selected, cx);
        }
    }

    fn body(&self, cx: &App, design: &Design) -> AnyElement {
        let filtered = self.filtered(cx);
        let selected = self.selected;
        let entries = self.recents.clone();
        let quick_statuses = self.quick_statuses.clone();
        let list_entity = self.entity.clone();
        let list_design = design.clone();
        let target = self.target;
        let mode = self.mode;
        let now = crate::recent::now_seconds();
        let home = Rc::new(home_dir());
        let recent_home = Rc::clone(&home);
        let recent_count = filtered.len();
        let filtered = Rc::new(filtered);
        let list_id = match self.mode {
            Mode::Page => "picker-recent-page",
            Mode::Dialog => "picker-recent-dialog",
        };

        let ghq_paths = Rc::clone(&self.ghq.paths);
        let ghq_filtered = Rc::clone(&self.ghq.filtered);
        let ghq_count = ghq_filtered.len();
        let ghq_selected = selected.checked_sub(recent_count);
        let ghq_visible = self.ghq.visible;
        let has_query = self.has_query(cx);
        let ghq_expanded = self.ghq_rows_expanded(cx);
        let ghq_loading = self.ghq.loading;
        let ghq_error = self.ghq.error.clone();
        let ghq_id = match self.mode {
            Mode::Page => "picker-ghq-page",
            Mode::Dialog => "picker-ghq-dialog",
        };

        let mut column = div()
            .flex()
            .flex_col()
            .gap(design.px(10.0))
            .w_full()
            .min_h_0()
            .flex_shrink(1.0);

        if self.mode == Mode::Page {
            column = column
                .child(brand_block(design))
                .child(self.search_field(cx, design))
                .child(self.action_tiles(cx, design))
                .child(remote_repository_link(design));
        } else {
            column = column.child(self.search_field(cx, design));
        }

        if let Some(error) = &self.error {
            column = column.child(ui::banner(ui::BannerKind::Danger, error.clone(), design));
        }

        column = column.child(
            div().flex().items_center().justify_between().pt_1().child(
                ui::section_header(
                    "picker-recent",
                    t!("picker.section.recent").to_string(),
                    design,
                )
                .count(self.recents.len())
                .px(px(0.0))
                .h(design.px(20.0)),
            ),
        );

        if recent_count == 0 {
            column = column.child(
                div()
                    .py_1()
                    .text_size(design.px(11.5))
                    .text_color(design.muted)
                    .child(if self.recents.is_empty() {
                        t!("picker.empty.recent").to_string()
                    } else {
                        t!("picker.empty.recent_filter").to_string()
                    }),
            );
        } else {
            let max_height = match self.mode {
                Mode::Page => PAGE_LIST_HEIGHT,
                Mode::Dialog => DIALOG_LIST_HEIGHT,
            };
            let list_height = ((recent_count as f32) * ROW_HEIGHT).min(max_height);
            let list = uniform_list(list_id, recent_count, move |range, _window, _cx| {
                range
                    .map(|position| {
                        let index = filtered[position];
                        let Some(entry) = entries.get(index) else {
                            return div().into_any_element();
                        };
                        let status = quick_statuses.get(&entry.path).cloned();
                        recent_row(
                            mode,
                            target,
                            position,
                            entry,
                            status.as_ref(),
                            recent_home.as_deref(),
                            position == selected,
                            now,
                            list_entity.clone(),
                            &list_design,
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .track_scroll(&self.scroll)
            .with_scrollbar(&self.scroll)
            .flex_shrink(1.0)
            .min_h(design.px(ROW_HEIGHT.min(list_height)))
            .h(design.px(list_height))
            .w_full();
            column = column.child(list);
        }

        // With nothing under the ghq root the section would only be a lone
        // label; it comes back while loading or to report a failure.
        let ghq_worth_showing = ghq_loading || ghq_error.is_some() || !ghq_paths.is_empty();
        if ghq_visible && ghq_worth_showing {
            if !ghq_expanded {
                let expand_entity = self.entity.clone();
                let mut collapsed = div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pt_1()
                    .h(design.px(24.0))
                    .child(
                        div()
                            .text_size(px(design.tiny()))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(design.muted)
                            .child(if ghq_loading && ghq_paths.is_empty() {
                                t!("picker.loading_ghq").to_string()
                            } else {
                                t!("picker.ghq.collapsed", count = ghq_paths.len()).to_string()
                            }),
                    );
                if ghq_error.is_none() && !ghq_paths.is_empty() {
                    collapsed = collapsed.child(
                        div()
                            .id(format!("picker-ghq-show-all-{mode:?}"))
                            .cursor_pointer()
                            .text_size(px(design.tiny()))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(design.accent_text)
                            .hover(|style| style.underline())
                            .on_click(move |_, _, cx| {
                                expand_entity.update(cx, |picker, cx| {
                                    picker.ghq_expanded = true;
                                    cx.notify();
                                });
                            })
                            .child(t!("picker.ghq.show_all").to_string()),
                    );
                }
                column = column.child(collapsed);
                if let Some(error) = ghq_error {
                    column = column.child(ui::banner(
                        ui::BannerKind::Danger,
                        t!("picker.ghq_failed", error = error).to_string(),
                        design,
                    ));
                }
            } else {
                let mut header =
                    ui::section_header("picker-ghq", t!("picker.section.ghq").to_string(), design)
                        .count(ghq_paths.len())
                        .px(px(0.0))
                        .h(design.px(20.0));
                if !has_query {
                    let toggle_entity = self.entity.clone();
                    header = header.disclosure(true).on_click(move |_, cx| {
                        toggle_entity.update(cx, |picker, cx| {
                            picker.ghq_expanded = false;
                            let recents_len = picker.filtered(cx).len();
                            picker.selected = picker.selected.min(recents_len.saturating_sub(1));
                            cx.notify();
                        });
                    });
                }
                column = column.child(div().pt_1().child(header));

                if ghq_loading && ghq_paths.is_empty() {
                    column = column.child(
                        div()
                            .text_size(design.px(11.5))
                            .text_color(design.muted)
                            .child(t!("picker.loading_ghq").to_string()),
                    );
                } else if let Some(error) = ghq_error {
                    column = column.child(ui::banner(
                        ui::BannerKind::Danger,
                        t!("picker.ghq_failed", error = error).to_string(),
                        design,
                    ));
                } else if ghq_count == 0 {
                    column = column.child(
                        div()
                            .text_size(design.px(11.5))
                            .text_color(design.muted)
                            .child(if ghq_paths.is_empty() {
                                t!("picker.empty.ghq").to_string()
                            } else {
                                t!("picker.empty.ghq_filter").to_string()
                            }),
                    );
                } else {
                    let entity = self.entity.clone();
                    let row_design = design.clone();
                    let paths = Rc::clone(&ghq_paths);
                    let indices = Rc::clone(&ghq_filtered);
                    let ghq_home = Rc::clone(&home);
                    let ghq_height = ((ghq_count as f32) * ROW_HEIGHT).min(GHQ_LIST_HEIGHT);
                    let list = uniform_list(ghq_id, ghq_count, move |range, _window, _cx| {
                        range
                            .map(|position| {
                                let index = indices[position];
                                let Some(path) = paths.get(index) else {
                                    return div().into_any_element();
                                };
                                ghq_row(
                                    mode,
                                    target,
                                    position,
                                    path,
                                    ghq_home.as_deref(),
                                    ghq_selected == Some(position),
                                    entity.clone(),
                                    &row_design,
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .track_scroll(&self.ghq_scroll)
                    .with_scrollbar(&self.ghq_scroll)
                    .flex_shrink(1.0)
                    .min_h(design.px(ROW_HEIGHT.min(ghq_height)))
                    .h(design.px(ghq_height))
                    .w_full();
                    column = column.child(list);
                }
            }
        }

        if self.mode == Mode::Page {
            column = column.child(footer_hints(cx.platform_info().os, design));
        }

        div()
            .w_full()
            .when(self.mode == Mode::Page, |body| body.max_w(design.px(680.0)))
            .flex()
            .flex_col()
            .min_h_0()
            .flex_shrink(1.0)
            .child(column)
            .into_any_element()
    }

    fn search_field(&self, cx: &App, design: &Design) -> AnyElement {
        let input_focus = self.input.focus_handle(cx);
        div()
            .id(format!("picker-search-field-{:?}", self.mode))
            .flex()
            .items_center()
            .gap(design.px(8.0))
            .w_full()
            .h(design.px(34.0))
            .px(design.px(10.0))
            .rounded(design.px(8.0))
            .bg(design.field)
            .border_1()
            .border_color(design.border)
            .cursor_text()
            .on_click(move |_, window, cx| {
                window.focus(&input_focus, cx);
            })
            .child(
                Icon::new(IconName::Search)
                    .with_size(design.px(14.0))
                    .text_color(design.muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.input).appearance(false).bordered(false)),
            )
            // A path is not a filter, so the list below stays as it was; the
            // keycap says what Enter will do instead.
            .when(looks_like_path(&self.input.read(cx).value()), |field| {
                field.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(design.px(6.0))
                        .text_size(px(design.tiny()))
                        .text_color(design.muted)
                        .child(t!("picker.path_enter").to_string())
                        .child(ui::keycaps(
                            keystroke_caps("enter", cx.platform_info().os),
                            design,
                        )),
                )
            })
            .into_any_element()
    }

    fn action_tiles(&self, cx: &App, design: &Design) -> AnyElement {
        let open_caps = action_shortcut_caps("OpenRepository", cx);
        let open_entity = self.entity.clone();
        let clone_entity = self.entity.clone();
        let init_entity = self.entity.clone();
        let compare_entity = self.entity.clone();
        let path_entity = self.entity.clone();

        div()
            .flex()
            .flex_wrap()
            .items_stretch()
            .gap(design.px(8.0))
            .w_full()
            .child(
                ui::action_tile(
                    format!("picker-tile-open-{:?}", self.mode),
                    IconName::FolderOpen,
                    t!("picker.tile.open").to_string(),
                    design,
                )
                .primary(true)
                .keycaps(open_caps)
                .on_click(move |_, window, cx| {
                    open_entity.update(cx, |picker, cx| picker.open_native(window, cx));
                }),
            )
            .child(
                ui::action_tile(
                    format!("picker-tile-clone-{:?}", self.mode),
                    IconName::Download,
                    t!("picker.tile.clone").to_string(),
                    design,
                )
                .on_click(move |_, window, cx| {
                    clone_entity.update(cx, |picker, cx| picker.open_clone(window, cx));
                }),
            )
            .child(
                ui::action_tile(
                    format!("picker-tile-new-{:?}", self.mode),
                    IconName::FolderPlus,
                    t!("picker.tile.new").to_string(),
                    design,
                )
                .on_click(move |_, window, cx| {
                    init_entity.update(cx, |picker, cx| picker.open_init(window, cx));
                }),
            )
            .child(
                ui::action_tile(
                    format!("picker-tile-compare-{:?}", self.mode),
                    IconName::Replace,
                    t!("picker.tile.compare").to_string(),
                    design,
                )
                .on_click(move |_, window, cx| {
                    compare_entity.update(cx, |picker, cx| picker.open_compare(window, cx));
                }),
            )
            .child(
                ui::action_tile(
                    format!("picker-tile-path-{:?}", self.mode),
                    IconName::FolderInput,
                    t!("picker.tile.path").to_string(),
                    design,
                )
                .on_click(move |_, window, cx| {
                    path_entity.update(cx, |picker, cx| picker.focus_path_entry(window, cx));
                }),
            )
            .into_any_element()
    }
}

/// A text link under the welcome tiles. A sixth tile would not fit the
/// minimum window, and opening a remote repository is the less common path.
fn remote_repository_link(design: &Design) -> AnyElement {
    div()
        .id("picker-open-remote")
        .flex()
        .justify_center()
        .w_full()
        .cursor_pointer()
        .text_size(px(design.small()))
        .text_color(design.accent)
        .child(t!("action.open_remote_repository").to_string())
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(crate::actions::OpenRemoteRepository), cx);
        })
        .into_any_element()
}

fn brand_block(design: &Design) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(design.px(12.0))
        .pb(design.px(4.0))
        .child(crate::style::app_mark(design.scale(32.0)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(design.px(4.0))
                .child(crate::style::wordmark(design.scale(13.0), design.text))
                .child(
                    div()
                        .text_size(design.px(12.0))
                        .text_color(design.muted)
                        .child(t!("picker.subtitle").to_string()),
                ),
        )
        .into_any_element()
}

fn action_shortcut_caps(action: &str, cx: &App) -> Vec<SharedString> {
    let os = cx.platform_info().os;
    let default_state = keymap::KeymapState::default();
    let state = cx
        .try_global::<keymap::KeymapState>()
        .unwrap_or(&default_state);
    keymap::resolve(action)
        .and_then(|spec| {
            keymap::effective_keystrokes(state, spec, os)
                .into_iter()
                .next()
        })
        .map(|keystroke| {
            ui::shortcut_caps(&keystroke, os)
                .into_iter()
                .map(SharedString::from)
                .collect()
        })
        .unwrap_or_default()
}

fn keystroke_caps(spec: &str, os: OperatingSystem) -> Vec<SharedString> {
    Keystroke::parse_with_os(spec, os)
        .map(|keystroke| {
            ui::shortcut_caps(&keystroke, os)
                .into_iter()
                .map(SharedString::from)
                .collect()
        })
        .unwrap_or_default()
}

fn footer_hints(os: OperatingSystem, design: &Design) -> AnyElement {
    let item = |caps: Vec<SharedString>, label: String| {
        div()
            .flex()
            .items_center()
            .gap(design.px(5.0))
            .child(ui::keycaps(caps, design))
            .child(div().text_color(design.muted).child(label))
    };

    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(design.px(14.0))
        .gap_y(design.px(4.0))
        .pt_1()
        .text_size(design.px(10.5))
        .text_color(design.muted)
        .child(item(
            vec!["↑".into(), "↓".into()],
            t!("picker.hint.select").to_string(),
        ))
        .child(item(
            keystroke_caps("enter", os),
            t!("picker.hint.open").to_string(),
        ))
        .child(item(
            keystroke_caps("secondary-enter", os),
            t!("picker.hint.new_tab").to_string(),
        ))
        .child(item(
            keystroke_caps("backspace", os),
            t!("picker.hint.remove").to_string(),
        ))
        .into_any_element()
}

/// Shared row chrome for recent and ghq repositories.
#[allow(clippy::too_many_arguments)]
fn repo_row_shell(
    open_id: (&'static str, usize),
    group: String,
    name: String,
    formatted_path: String,
    selected: bool,
    trailing: Vec<AnyElement>,
    remove_button: Option<AnyElement>,
    on_open: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    design: &Design,
) -> gpui::Stateful<gpui::Div> {
    let avatar = ui::repo_avatar(
        crate::style::repo_initial(&name),
        design.repo_color(&name),
        design,
    );

    let mut open = div()
        .id(open_id)
        .flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .items_center()
        .gap(design.px(10.0))
        .cursor_pointer()
        .on_click(on_open)
        .child(avatar)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .truncate()
                        .text_size(design.px(12.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(design.text)
                        .child(name),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(design.px(10.5))
                        .text_color(design.muted)
                        .child(formatted_path),
                ),
        );

    if !trailing.is_empty() {
        open = open.child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(design.px(6.0))
                .children(trailing),
        );
    }

    div()
        .id(SharedString::from(group.clone()))
        .group(group)
        .flex()
        .items_center()
        .gap(design.px(6.0))
        .h(design.px(ROW_HEIGHT))
        .w_full()
        .px(design.px(8.0))
        .rounded(design.px(7.0))
        .when(selected, |row| row.bg(design.accent_soft))
        .when(!selected, |row| row.hover(|style| style.bg(design.field)))
        .child(open)
        .when_some(remove_button, |row, button| row.child(button))
}

fn repo_state_label(state: RepoState) -> String {
    match state {
        RepoState::Normal => String::new(),
        RepoState::Merging => t!("progress.merging").to_string(),
        RepoState::Rebasing => t!("progress.rebasing").to_string(),
        RepoState::CherryPicking => t!("progress.cherry_picking").to_string(),
        RepoState::Reverting => t!("progress.reverting").to_string(),
        RepoState::ApplyingPatches => t!("progress.applying_patches").to_string(),
        RepoState::Bisecting => t!("progress.bisecting").to_string(),
    }
}

/// Row rendering shared by the page and the dialog.
#[allow(clippy::too_many_arguments)]
fn recent_row(
    mode: Mode,
    default_target: OpenTarget,
    position: usize,
    entry: &RecentRepository,
    quick_status: Option<&RepoQuickState>,
    home: Option<&Path>,
    selected: bool,
    now: u64,
    entity: Entity<Picker>,
    design: &Design,
) -> AnyElement {
    let group = format!("recent-{mode:?}-{position}");
    let name = entry.name();
    let formatted_path = format_repo_path(&entry.path, home, PATH_DISPLAY_MAX_CHARS);
    let opened = opened_label(entry.opened_at, now);

    let mut trailing: Vec<AnyElement> = Vec::new();
    if crate::remote_open::is_stored_remote(&entry.path) {
        trailing.push(ui::status_badge(
            t!("remote_repo.badge").to_string(),
            ui::BadgeTone::Accent,
            design,
        ));
    }
    match quick_status {
        Some(RepoQuickState::NotFound) => {
            trailing.push(ui::status_badge(
                t!("picker.status.not_found").to_string(),
                ui::BadgeTone::Neutral,
                design,
            ));
        }
        Some(RepoQuickState::Loaded(status)) => {
            if status.state != RepoState::Normal {
                trailing.push(ui::status_badge(
                    repo_state_label(status.state),
                    ui::BadgeTone::Warning,
                    design,
                ));
            }
            if status.dirty {
                trailing.push(ui::dirty_dot(design));
            }
            if let Some(head) = &status.head {
                match head {
                    HeadInfo::Branch { short_name, .. } => {
                        trailing.push(
                            ui::ref_chip(
                                ("recent-branch", position),
                                RefKind::LocalBranch,
                                false,
                                short_name.clone(),
                                design.scale(BRANCH_CHIP_MAX_WIDTH),
                                design,
                            )
                            .into_any_element(),
                        );
                    }
                    HeadInfo::Detached { target } => {
                        trailing.push(ui::status_badge(
                            target.to_hex_with_len(7).to_string(),
                            ui::BadgeTone::Neutral,
                            design,
                        ));
                    }
                }
            }
        }
        Some(RepoQuickState::Pending | RepoQuickState::Unavailable) | None => {}
    }
    trailing.push(
        div()
            .flex_none()
            .text_size(design.px(10.5))
            .text_color(design.muted)
            .child(opened)
            .into_any_element(),
    );

    let remove_entity = entity.clone();
    let remove = div()
        .id(("recent-remove", position))
        .flex_none()
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .text_size(design.px(12.0))
        .text_color(design.muted)
        // Hidden until the row is hovered, so the list reads as names rather
        // than names with buttons. It stays clickable once shown.
        .opacity(0.0)
        .group_hover(group.clone(), |style| style.opacity(1.0))
        .on_click(move |_, _, cx| {
            remove_entity.update(cx, |picker, cx| picker.remove(position, cx));
        })
        .child("×")
        .into_any_element();

    let open_entity = entity.clone();
    let row = repo_row_shell(
        ("recent-open", position),
        group,
        name,
        formatted_path,
        selected,
        trailing,
        Some(remove),
        move |event, window, cx| {
            let target = if event.modifiers().secondary(cx) {
                OpenTarget::NewTab
            } else {
                default_target
            };
            open_entity.update(cx, |picker, cx| {
                picker.open_row_in(position, target, window, cx);
            });
        },
        design,
    );

    let menu_id: SharedString = format!("recent-menu-{mode:?}-{position}").into();
    let menu_entity = entity;
    ui::context_menu(menu_id, design, row, move |menu, _window, _cx| {
        let open_current = {
            let entity = menu_entity.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                entity.update(cx, |picker, cx| {
                    picker.open_row_in(position, OpenTarget::Current, window, cx);
                });
            }
        };
        let open_new_tab = {
            let entity = menu_entity.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                entity.update(cx, |picker, cx| {
                    picker.open_row_in(position, OpenTarget::NewTab, window, cx);
                });
            }
        };
        let open_new_window = {
            let entity = menu_entity.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                entity.update(cx, |picker, cx| {
                    picker.open_row_in_new_window(position, window, cx);
                });
            }
        };
        let remove = {
            let entity = menu_entity.clone();
            move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                entity.update(cx, |picker, cx| picker.remove(position, cx));
            }
        };
        menu.item(PopupMenuItem::new(t!("branch.menu.open").to_string()).on_click(open_current))
            .item(
                PopupMenuItem::new(t!("branch.menu.open_new_tab").to_string())
                    .on_click(open_new_tab),
            )
            .item(
                PopupMenuItem::new(t!("branch.menu.open_new_window").to_string())
                    .on_click(open_new_window),
            )
            .separator()
            .item(PopupMenuItem::new(t!("picker.remove_recent").to_string()).on_click(remove))
    })
    .into_any_element()
}

/// Row rendering for a ghq repository.
#[allow(clippy::too_many_arguments)]
fn ghq_row(
    mode: Mode,
    default_target: OpenTarget,
    position: usize,
    path: &Path,
    home: Option<&Path>,
    selected: bool,
    entity: Entity<Picker>,
    design: &Design,
) -> AnyElement {
    let group = format!("ghq-{mode:?}-{position}");
    let name = crate::recent::repository_name(path);
    let formatted_path = format_repo_path(path, home, PATH_DISPLAY_MAX_CHARS);

    let open_entity = entity.clone();
    let row = repo_row_shell(
        ("ghq-open", position),
        group,
        name,
        formatted_path,
        selected,
        Vec::new(),
        None,
        move |event, window, cx| {
            let target = if event.modifiers().secondary(cx) {
                OpenTarget::NewTab
            } else {
                default_target
            };
            open_entity.update(cx, |picker, cx| {
                picker.open_ghq_row_in(position, target, window, cx);
            });
        },
        design,
    );

    let menu_id: SharedString = format!("ghq-menu-{mode:?}-{position}").into();
    let menu_entity = entity;
    ui::context_menu(menu_id, design, row, move |menu, _window, _cx| {
        let open_current = {
            let entity = menu_entity.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                entity.update(cx, |picker, cx| {
                    picker.open_ghq_row_in(position, OpenTarget::Current, window, cx);
                });
            }
        };
        let open_new_tab = {
            let entity = menu_entity.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                entity.update(cx, |picker, cx| {
                    picker.open_ghq_row_in(position, OpenTarget::NewTab, window, cx);
                });
            }
        };
        let open_new_window = {
            let entity = menu_entity.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                entity.update(cx, |picker, cx| {
                    picker.open_ghq_row_in_new_window(position, window, cx);
                });
            }
        };
        menu.item(PopupMenuItem::new(t!("branch.menu.open").to_string()).on_click(open_current))
            .item(
                PopupMenuItem::new(t!("branch.menu.open_new_tab").to_string())
                    .on_click(open_new_tab),
            )
            .item(
                PopupMenuItem::new(t!("branch.menu.open_new_window").to_string())
                    .on_click(open_new_window),
            )
    })
    .into_any_element()
}

impl Render for Picker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let body = self.body(cx, &design);

        let handle_backspace_delete = cx.listener(|this, event: &KeyDownEvent, window, cx| {
            let key = event.keystroke.key.as_str();
            let mods = event.keystroke.modifiers;
            // While the filter field is focused with text, Backspace/Delete
            // edits the query as usual; when empty, it removes the selected
            // recent row without forcing the user to press Escape first.
            if !event.is_held
                && (key == "backspace" || key == "delete")
                && !mods.control
                && !mods.alt
                && !mods.platform
                && !mods.shift
                && !mods.function
                && this.input.focus_handle(cx).is_focused(window)
                && this.input.read(cx).value().is_empty()
            {
                this.remove_selected(cx);
                cx.stop_propagation();
            }
        });

        match self.mode {
            Mode::Page => div()
                .size_full()
                .flex()
                .flex_col()
                .bg(design.background)
                .text_color(design.text)
                .track_focus(&self.focus)
                .key_context(PICKER_PAGE)
                .capture_key_down(handle_backspace_delete)
                .on_action(cx.listener(|this, _: &PickerPrevious, _, cx| this.step(-1, cx)))
                .on_action(cx.listener(|this, _: &PickerNext, _, cx| this.step(1, cx)))
                .on_action(cx.listener(|this, _: &OpenSelectedRepository, window, cx| {
                    this.open_selected(window, cx);
                }))
                .on_action(cx.listener(|this, _: &OpenSelectedInNewTab, window, cx| {
                    this.open_selected_in(OpenTarget::NewTab, window, cx);
                }))
                .on_action(cx.listener(|this, _: &PickerEscape, window, cx| {
                    this.focus.focus(window, cx);
                }))
                .on_action(cx.listener(|this, _: &RemoveSelectedRecent, _, cx| {
                    this.remove_selected(cx);
                }))
                .child(
                    // The same 44 px chrome strip as the main window, so the
                    // welcome screen is the same app with no repository open.
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
                                        .child(crate::style::app_mark(
                                            design.scale(crate::style::BRAND_MARK),
                                        ))
                                        .child(crate::style::wordmark(
                                            design.scale(crate::style::BRAND_WORDMARK),
                                            design.text,
                                        )),
                                )
                                .child(div().w(px(1.0)).h(design.px(18.0)).mx_1().bg(design.border))
                                .child(
                                    div()
                                        .text_size(design.px(11.0))
                                        .text_color(design.muted)
                                        .child(t!("picker.welcome").to_string()),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .items_center()
                        .px(design.px(24.0))
                        // Anchor ~15-20% from the top on tall windows while
                        // letting short windows collapse to keep header, list
                        // and footer usable.
                        .child(
                            div()
                                .flex_1()
                                .min_h(design.px(12.0))
                                .max_h(design.px(120.0)),
                        )
                        .child(body)
                        .child(div().flex_1().min_h(design.px(12.0))),
                )
                .into_any_element(),
            Mode::Dialog => {
                let subtitle = match self.target {
                    OpenTarget::Current => t!("picker.choose_window").to_string(),
                    OpenTarget::NewTab => t!("picker.choose_tab").to_string(),
                };
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .track_focus(&self.focus)
                    .key_context(PICKER_DIALOG)
                    .capture_key_down(handle_backspace_delete)
                    .on_action(cx.listener(|this, _: &PickerPrevious, _, cx| this.step(-1, cx)))
                    .on_action(cx.listener(|this, _: &PickerNext, _, cx| this.step(1, cx)))
                    .on_action(cx.listener(|this, _: &RemoveSelectedRecent, _, cx| {
                        this.remove_selected(cx);
                    }))
                    .child(
                        div()
                            .pb_1()
                            .text_size(design.px(11.0))
                            .text_color(design.muted)
                            .child(subtitle),
                    )
                    .child(body)
                    .into_any_element()
            }
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Whether `text` looks like a filesystem path rather than a repository filter.
pub(crate) fn looks_like_path(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.starts_with('/')
        || trimmed.starts_with('~')
        || trimmed.starts_with('.')
        || trimmed.starts_with("\\\\")
        || is_windows_drive_path(trimmed)
}

fn is_windows_drive_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Expands a leading `~`, `~/...`, or `~\...` against `home`.
pub(crate) fn expand_home_path(text: &str, home: Option<&Path>) -> PathBuf {
    let trimmed = text.trim();
    if let Some(home) = home {
        if trimmed == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = trimmed
            .strip_prefix("~/")
            .or_else(|| trimmed.strip_prefix("~\\"))
        {
            return home.join(rest);
        }
    }
    PathBuf::from(trimmed)
}

/// Shortens `home` to `~` and middle-ellipsises long paths so the repo folder
/// and leading root stay readable in narrow rows.
pub(crate) fn format_repo_path(path: &Path, home: Option<&Path>, max_chars: usize) -> String {
    if crate::remote_open::is_stored_remote(path) {
        return middle_ellipsis(&path.to_string_lossy(), max_chars);
    }
    let shortened = match home {
        Some(home) if path == home => "~".to_owned(),
        Some(home) => match path.strip_prefix(home) {
            Ok(rel) => format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    };
    middle_ellipsis(&shortened, max_chars)
}

fn middle_ellipsis(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars || max_chars < 5 {
        return text.to_owned();
    }
    let keep = max_chars - 1;
    let head = keep * 2 / 5;
    let tail = keep - head;
    let prefix: String = text.chars().take(head).collect();
    let suffix: String = text
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

/// Indices of the entries whose name or path contains `query`, ignoring case.
/// An empty query keeps everything.
pub(crate) fn filter_indices(entries: &[RecentRepository], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            if query.is_empty() {
                return true;
            }
            entry.name().to_lowercase().contains(&query)
                || entry.path.to_string_lossy().to_lowercase().contains(&query)
        })
        .map(|(index, _)| index)
        .collect()
}

/// Clamps a list movement to the list. An empty list has no position at all,
/// which callers treat as "nothing selected".
pub(crate) fn move_selection(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).clamp(0, len as isize - 1) as usize
}

/// "just now", then minutes, hours, days, then an absolute date.
pub(crate) fn opened_label(opened_at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(opened_at);
    match seconds {
        0..=59 => t!("time.just_now").to_string(),
        60..=3_599 => minutes(seconds / 60),
        3_600..=86_399 => hours(seconds / 3_600),
        86_400..=2_591_999 => days(seconds / 86_400),
        _ => crate::data::format_time(opened_at as i64),
    }
}

fn minutes(count: u64) -> String {
    if count == 1 {
        t!("time.minutes_long.one", count = count).to_string()
    } else {
        t!("time.minutes_long.other", count = count).to_string()
    }
}

fn hours(count: u64) -> String {
    if count == 1 {
        t!("time.hours.one", count = count).to_string()
    } else {
        t!("time.hours.other", count = count).to_string()
    }
}

fn days(count: u64) -> String {
    if count == 1 {
        t!("time.days.one", count = count).to_string()
    } else {
        t!("time.days.other", count = count).to_string()
    }
}

/// Checks that `path` is a directory inside a git repository and returns the
/// repository's own root, so opening a subdirectory of a worktree recenters the
/// list on the worktree rather than on the subdirectory the user picked.
pub(crate) fn resolve_repository(path: &Path) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(t!("picker.err.does_not_exist", path = path.display()).to_string());
    }
    if !path.is_dir() {
        return Err(t!("picker.err.is_file", path = path.display()).to_string());
    }
    match Repo::open(path) {
        Ok(repo) => Ok(repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| repo.git_dir().to_path_buf())),
        Err(_) => Err(t!("picker.err.not_repo", path = path.display()).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, opened_at: u64) -> RecentRepository {
        RecentRepository {
            path: PathBuf::from(path),
            opened_at,
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Strata Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Strata Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn path_detection_recognizes_paths_and_ignores_repo_queries() {
        assert!(looks_like_path("/work/strata"));
        assert!(looks_like_path("  ~/work/strata  "));
        assert!(looks_like_path("~"));
        assert!(looks_like_path("./repo"));
        assert!(looks_like_path("../repo"));
        assert!(looks_like_path(r"C:\work\strata"));
        assert!(looks_like_path("C:/work/strata"));
        assert!(looks_like_path(r"\\server\share\repo"));

        assert!(!looks_like_path(""));
        assert!(!looks_like_path("   "));
        assert!(!looks_like_path("strata"));
        assert!(!looks_like_path("work/strata"));
    }

    #[test]
    fn home_expansion_and_path_formatting_handle_tilde_and_ellipsis() {
        let home = Path::new("/Users/ada");
        assert_eq!(
            expand_home_path("~", Some(home)),
            PathBuf::from("/Users/ada")
        );
        assert_eq!(
            expand_home_path("~/work/strata", Some(home)),
            PathBuf::from("/Users/ada/work/strata")
        );
        assert_eq!(
            expand_home_path("/tmp/other", Some(home)),
            PathBuf::from("/tmp/other")
        );

        let expected_short = format!(
            "~{}work{}strata",
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR
        );
        assert_eq!(
            // Joined, not spelled with `/`: a Windows path uses its own separator.
            format_repo_path(&home.join("work").join("strata"), Some(home), 56),
            expected_short
        );
        assert_eq!(format_repo_path(home, Some(home), 56), "~");

        let long = format_repo_path(
            Path::new("/Users/ada/projects/organizations/deeply/nested/workspace/strata"),
            Some(home),
            24,
        );
        assert_eq!(long.chars().count(), 24);
        assert!(long.contains('…'));
        assert!(long.starts_with('~'));
        assert!(long.ends_with("strata"));
    }

    #[test]
    fn filtering_matches_names_and_paths_ignoring_case() {
        let entries = [
            entry("/work/strata", 1),
            entry("/work/other-app", 2),
            entry("/tmp/notes", 3),
        ];
        assert_eq!(filter_indices(&entries, ""), vec![0, 1, 2]);
        assert_eq!(filter_indices(&entries, "  "), vec![0, 1, 2]);
        assert_eq!(filter_indices(&entries, "STRAT"), vec![0]);
        assert_eq!(filter_indices(&entries, "work"), vec![0, 1]);
        assert_eq!(filter_indices(&entries, "tmp/not"), vec![2]);
        assert!(filter_indices(&entries, "nothing").is_empty());
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        assert_eq!(move_selection(0, -1, 3), 0);
        assert_eq!(move_selection(2, 1, 3), 2);
        assert_eq!(move_selection(1, 1, 3), 2);
        assert_eq!(move_selection(9, 1, 3), 2, "a stale position is repaired");
        assert_eq!(move_selection(2, -5, 3), 0);
        assert_eq!(move_selection(0, 1, 0), 0, "an empty list has no position");
    }

    #[test]
    fn last_opened_times_read_as_relative_labels() {
        let now = 1_700_000_000;
        assert_eq!(opened_label(now, now), "just now");
        assert_eq!(opened_label(now - 1, now), "just now");
        assert_eq!(opened_label(now - 60, now), "1 minute ago");
        assert_eq!(opened_label(now - 120, now), "2 minutes ago");
        assert_eq!(opened_label(now - 3_600, now), "1 hour ago");
        assert_eq!(opened_label(now - 7_200, now), "2 hours ago");
        assert_eq!(opened_label(now - 86_400, now), "1 day ago");
        assert_eq!(opened_label(now - 172_800, now), "2 days ago");
        assert_eq!(
            opened_label(now - 40 * 86_400, now),
            crate::data::format_time((now - 40 * 86_400) as i64),
            "older entries get an absolute date"
        );
        assert_eq!(
            opened_label(now + 60, now),
            "just now",
            "clock skew is harmless"
        );
    }

    #[test]
    fn a_non_repository_directory_is_rejected_with_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve_repository(dir.path()).unwrap_err();
        assert!(
            error.contains("is not inside a git repository"),
            "unexpected message: {error}"
        );
        assert!(error.contains(&dir.path().display().to_string()));
    }

    #[test]
    fn a_missing_path_and_a_file_are_reported_separately() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        assert!(
            resolve_repository(&missing)
                .unwrap_err()
                .contains("does not exist")
        );

        let file = dir.path().join("notes.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(resolve_repository(&file).unwrap_err().contains("is a file"));
    }

    #[test]
    fn a_repository_resolves_to_its_worktree_root_from_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let nested = repo.join("src");
        std::fs::create_dir(&nested).unwrap();

        let root = resolve_repository(&nested).expect("a subdirectory is inside the repository");
        assert_eq!(
            std::fs::canonicalize(&root).unwrap(),
            std::fs::canonicalize(&repo).unwrap()
        );
        assert_eq!(
            std::fs::canonicalize(resolve_repository(&repo).unwrap()).unwrap(),
            std::fs::canonicalize(&repo).unwrap()
        );
    }
}
