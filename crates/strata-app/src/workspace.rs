//! What fills the window: the welcome screen or one or more repository tabs.
//!
//! `Root` is created with a fixed child, so the window's content is swapped one
//! level down: this view owns whichever tab is active and only renders the
//! active [`RepoView`]. Inactive tabs keep their [`Session`] (so their
//! filesystem watcher stays current and switching back is instant), while
//! pausing repaints and timers until re-activated. Releasing or closing a tab
//! takes its [`Session`] out and drops it on a background thread, because
//! `Session::drop` joins worker threads and must not block the UI thread.
//!
//! Swapping and opening tabs also gives every repository one place to record
//! "opened": the recent list and per-window session file are written here,
//! never by a view.

use std::path::{Path, PathBuf};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, BorrowAppContext as _, Bounds, Context, Entity, Global,
    InteractiveElement as _, IntoElement, ParentElement, Render, Styled, Subscription, Window,
    WindowBounds, WindowDecorations, WindowId, WindowOptions, div, px, size,
};
use gpui_component::dialog::DialogFooter;
use gpui_component::{Root, TitleBar, WindowExt as _};

use crate::actions::{
    CloneRepository, CloseTab, NewRepository, NewTab, NextTab, OpenRepository, PreviousTab,
    ReopenClosedTab, SelectTab1, SelectTab2, SelectTab3, SelectTab4, SelectTab5, SelectTab6,
    SelectTab7, SelectTab8, SelectTab9, WORKSPACE,
};
use crate::compare::{CompareTarget, CompareView};
use crate::debug_launch::{self, DebugDialog, DebugLaunch};
use crate::picker::{Mode, OpenTarget, Picker};
use crate::recent::{self, RecentRepositories};
use crate::repo_view::{Launch, RepoView};
use crate::settings::Settings;
use crate::style::Design;
use crate::tabs::{CloseOutcome, ClosedTab, ClosedTabStack, SavedSession, SavedWindow, TabDeck};

const DEFAULT_WINDOW_SIZE: (f32, f32) = (1360.0, 860.0);

/// Tracks open workspace windows so multi-window sessions survive quitting and
/// closing one secondary window updates `session.json` cleanly.
#[derive(Default)]
struct SessionRegistry {
    windows: Vec<(WindowId, SavedWindow)>,
    quitting: bool,
}

impl Global for SessionRegistry {}

/// Initializes workspace session tracking and quit protection.
pub(crate) fn init(cx: &mut App) {
    cx.set_global(SessionRegistry::default());
    cx.on_app_quit(|cx| {
        if cx.has_global::<SessionRegistry>() {
            cx.update_global::<SessionRegistry, _>(|reg, _| {
                reg.quitting = true;
            });
        }
        async {}
    })
    .detach();
}

/// Restored launches and active tab index for one window at startup.
pub(crate) struct RestoredWindow {
    pub(crate) tabs: Vec<Launch>,
    pub(crate) active: usize,
}

/// A parent repository the user navigated away from when opening a submodule
/// or linked worktree in the current tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParentRepo {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
}

/// Linear stack of parent repositories for a single workspace slot (not a tree:
/// opening an unrelated repository replaces the slot and clears it, and stepping
/// back pops the most recent parent).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NavigationStack {
    parents: Vec<ParentRepo>,
}

impl NavigationStack {
    pub(crate) fn push(&mut self, parent: ParentRepo) {
        self.parents.push(parent);
    }

    pub(crate) fn pop(&mut self) -> Option<ParentRepo> {
        self.parents.pop()
    }

    pub(crate) fn current(&self) -> Option<&ParentRepo> {
        self.parents.last()
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.parents.clear();
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.parents.len()
    }
}

/// One open repository tab together with its canonical root and parent history.
struct RepoSlot {
    view: Entity<RepoView>,
    /// The working-tree root (or git dir for bare repos). Canonical, so the
    /// picker, tab deduplication, and child-opening recognise the repository.
    path: Option<PathBuf>,
    parents: NavigationStack,
}

/// One open standalone file/folder compare tab.
struct CompareSlot {
    view: Entity<CompareView>,
    hint_path: Option<PathBuf>,
}

/// One open workspace tab (either a repository or a standalone compare view).
enum TabSlot {
    Repo(RepoSlot),
    Compare(CompareSlot),
}

impl TabSlot {
    fn as_repo(&self) -> Option<&RepoSlot> {
        match self {
            TabSlot::Repo(repo) => Some(repo),
            TabSlot::Compare(_) => None,
        }
    }

    fn repo_path(&self) -> Option<&Path> {
        self.as_repo().and_then(|repo| repo.path.as_deref())
    }

    fn set_active(&self, active: bool, cx: &mut App) {
        match self {
            TabSlot::Repo(slot) => slot.view.update(cx, |view, cx| view.set_active(active, cx)),
            TabSlot::Compare(slot) => slot.view.update(cx, |view, cx| view.set_active(active, cx)),
        }
    }

    fn window_title(&self, cx: &App) -> String {
        match self {
            TabSlot::Repo(slot) => slot.view.read(cx).window_title().to_owned(),
            TabSlot::Compare(slot) => slot.view.read(cx).window_title(),
        }
    }

    fn view_focus(&self, cx: &App) -> gpui::FocusHandle {
        match self {
            TabSlot::Repo(slot) => slot.view.read(cx).view_focus(),
            TabSlot::Compare(slot) => slot.view.read(cx).view_focus(),
        }
    }

    fn notify_view(&self, cx: &mut App) {
        match self {
            TabSlot::Repo(slot) => slot.view.update(cx, |_, cx| cx.notify()),
            TabSlot::Compare(slot) => slot.view.update(cx, |_, cx| cx.notify()),
        }
    }

    fn render_element(&self) -> AnyElement {
        match self {
            TabSlot::Repo(slot) => slot.view.clone().into_any_element(),
            TabSlot::Compare(slot) => slot.view.clone().into_any_element(),
        }
    }

    fn tab_display_info(
        &self,
        is_active: bool,
        active_name: Option<&str>,
        active_busy: bool,
        cx: &App,
    ) -> (String, bool, Option<PathBuf>) {
        match self {
            TabSlot::Repo(slot) => {
                let (label, busy) = if is_active {
                    let label = active_name
                        .map(str::to_owned)
                        .or_else(|| slot.path.as_deref().map(recent::repository_name))
                        .unwrap_or_else(|| t!("workspace.repository_tab").to_string());
                    (label, active_busy)
                } else {
                    (
                        slot_display_name(slot, cx),
                        slot.view.read(cx).has_operation_in_progress(),
                    )
                };
                (label, busy, slot.path.clone())
            }
            TabSlot::Compare(slot) => {
                let (label, busy) = if is_active {
                    (
                        active_name
                            .map(str::to_owned)
                            .unwrap_or_else(|| t!("compare.tab.prefix").to_string()),
                        active_busy,
                    )
                } else {
                    let view = slot.view.read(cx);
                    (view.tab_title(), view.is_loading())
                };
                (label, busy, slot.hint_path.clone())
            }
        }
    }

    #[cfg(test)]
    fn repo(&self) -> &RepoSlot {
        self.as_repo().expect("expected repository tab slot")
    }
}

/// The window's current content.
enum Content {
    /// The welcome screen, shown when no repository was opened at startup.
    Welcome(Entity<Picker>),
    /// One or more open tabs (always non-empty while active).
    Tabs(TabDeck<TabSlot>),
}

enum OpenDisposition {
    /// Unrelated repository opened from the picker: reset the parent stack.
    Replace,
    /// Child repository (submodule or linked worktree): push the current
    /// repository onto the parent stack before switching.
    PushParent,
}

pub struct Workspace {
    content: Content,
    window_id: WindowId,
    /// Repository tabs closed in this window, newest last, for
    /// "Reopen closed tab".
    closed_tabs: ClosedTabStack,
    /// `System` follows the OS. Repository views subscribe for themselves; the
    /// welcome screen has no view of its own to do that, so the workspace
    /// keeps the subscription while it is showing one.
    _appearance: Option<Subscription>,
    /// Overrides the recent-list and session directory in tests so test runs
    /// never mutate the user's real config files or depend on global env vars.
    #[cfg(test)]
    recent_dir: Option<PathBuf>,
    #[cfg(test)]
    launch_repo: fn(&Path, Option<PathBuf>) -> Launch,
}

impl Workspace {
    /// `None` opens the welcome screen; `Some` is whatever the command line or
    /// the restore settings resolved to.
    #[allow(dead_code)]
    pub(crate) fn new(launch: Option<Launch>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_with_debug(launch, DebugLaunch::default(), window, cx)
    }

    pub(crate) fn new_with_debug(
        launch: Option<Launch>,
        debug: DebugLaunch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        match launch {
            Some(launch) => Self::new_with_tabs(vec![launch], 0, debug, window, cx),
            None => {
                if debug.is_active() {
                    if debug.has_repo_actions() {
                        eprintln!(
                            "debug-launch: error: repository options require an open repository (showing welcome screen)"
                        );
                    }
                    if let Some(dialog) = debug.dialog
                        && !dialog.needs_repo()
                    {
                        let workspace = cx.weak_entity();
                        let handle = window.window_handle();
                        window.defer(cx, move |_, cx| {
                            let _ = handle.update(cx, |_, window, cx| match dialog {
                                DebugDialog::OpenRepository => {
                                    let _ = workspace.update(cx, |this, cx| {
                                        this.open_picker(window, cx);
                                    });
                                    eprintln!("debug-launch: applied --dialog open-repository");
                                }
                                _ => debug_launch::dispatch_window_dialog(dialog, window, cx),
                            });
                        });
                    }
                }
                Self::welcome(
                    window.observe_window_appearance(|window, cx| {
                        crate::settings::apply_theme(Some(window), cx)
                    }),
                    window,
                    cx,
                )
            }
        }
    }

    /// Creates a workspace pre-populated with one or more repository tabs,
    /// keeping inactive tabs quiet (`active = false`) while leaving their
    /// background sessions and watchers running.
    pub(crate) fn new_with_tabs(
        launches: Vec<Launch>,
        active: usize,
        debug: DebugLaunch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if launches.is_empty() {
            return Self::new_with_debug(None, debug, window, cx);
        }
        let clamped_active = active.min(launches.len() - 1);
        let workspace_weak = cx.weak_entity();
        let mut slots = Vec::with_capacity(launches.len());

        for (index, launch) in launches.into_iter().enumerate() {
            let is_active = index == clamped_active;
            let path = repo_path(&launch);
            let ws = workspace_weak.clone();
            let view = cx.new(|cx| {
                RepoView::new_in_workspace_active(launch, Some(ws), None, is_active, window, cx)
            });
            if is_active && debug.is_active() {
                view.update(cx, |view, cx| {
                    view.set_debug_launch(debug.clone(), window, cx)
                });
            }
            slots.push(TabSlot::Repo(RepoSlot {
                view,
                path,
                parents: NavigationStack::default(),
            }));
        }

        let deck = TabDeck::from_items(slots, clamped_active);
        if let Some(active_slot) = deck.active() {
            let title = active_slot.window_title(cx);
            window.set_window_title(&title);
        }

        let window_id = window.window_handle().window_id();
        track_window_release(&cx.entity(), cx);

        let workspace = Self {
            content: Content::Tabs(deck),
            // Repository views manage window appearance subscriptions on their own.
            _appearance: None,
            window_id,
            closed_tabs: ClosedTabStack::default(),
            #[cfg(test)]
            recent_dir: None,
            #[cfg(test)]
            launch_repo: Launch::open_with,
        };

        // Record recents after all slots are constructed so the active tab ends
        // up newest in `recent.json`.
        if let Content::Tabs(deck) = &workspace.content {
            for (index, slot) in deck.items().iter().enumerate() {
                if index != clamped_active
                    && let Some(path) = slot.repo_path()
                {
                    workspace.record_recent(path);
                }
            }
            if let Some(active_path) = deck.active().and_then(TabSlot::repo_path) {
                workspace.record_recent(active_path);
            }
        }
        workspace.persist_session(cx);
        workspace
    }

    /// Creates a workspace showing a single standalone Compare tab.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new_with_compare(
        target: CompareTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_weak = cx.weak_entity();
        let view = cx.new(|cx| CompareView::new(target, workspace_weak, cx));
        let title = view.read(cx).window_title();
        let hint_path = view.read(cx).hint_path().map(Path::to_path_buf);
        window.set_window_title(&title);
        let focus = view.read(cx).view_focus();
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        let appearance = window
            .observe_window_appearance(|window, cx| crate::settings::apply_theme(Some(window), cx));
        let window_id = window.window_handle().window_id();
        track_window_release(&cx.entity(), cx);

        Self {
            content: Content::Tabs(TabDeck::from_items(
                vec![TabSlot::Compare(CompareSlot { view, hint_path })],
                0,
            )),
            window_id,
            closed_tabs: ClosedTabStack::default(),
            _appearance: Some(appearance),
            #[cfg(test)]
            recent_dir: None,
            #[cfg(test)]
            launch_repo: Launch::open_with,
        }
    }

    fn welcome(appearance: Subscription, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.entity();
        let view = cx.new(|cx| Picker::new(Mode::Page, OpenTarget::Current, workspace, window, cx));
        let input = view.read(cx).input().clone();
        crate::ui::focus_input(&input, window, cx);
        window.set_window_title(&t!("app.name"));
        let window_id = window.window_handle().window_id();
        track_window_release(&cx.entity(), cx);
        Self {
            content: Content::Welcome(view),
            window_id,
            closed_tabs: ClosedTabStack::default(),
            _appearance: Some(appearance),
            #[cfg(test)]
            recent_dir: None,
            #[cfg(test)]
            launch_repo: Launch::open_with,
        }
    }

    pub(crate) fn current_path(&self) -> Option<&Path> {
        match &self.content {
            Content::Welcome(_) => None,
            Content::Tabs(deck) => deck.active().and_then(TabSlot::repo_path),
        }
    }

    pub(crate) fn tab_count(&self) -> usize {
        match &self.content {
            Content::Welcome(_) => 0,
            Content::Tabs(deck) => deck.len(),
        }
    }

    fn current_repo_name(&self, cx: &App) -> Option<String> {
        let Content::Tabs(deck) = &self.content else {
            return None;
        };
        let slot = deck.active()?.as_repo()?;
        Some(slot_display_name(slot, cx))
    }

    fn record_recent(&self, path: &Path) {
        #[cfg(test)]
        if let Some(dir) = &self.recent_dir {
            let file = dir.join("recent.json");
            let mut recents = RecentRepositories::load(&file);
            recents.record_now(path);
            if let Err(error) = recents.save(&file) {
                log::warn!("cannot save test recent-repository list: {error}");
            }
            return;
        }
        #[cfg(not(test))]
        recent::record(path);
        #[cfg(test)]
        if std::env::var_os("STRATA_CONFIG_DIR").is_some() {
            recent::record(path);
        } else {
            let _ = path;
        }
    }

    fn saved_window(&self) -> Option<SavedWindow> {
        let Content::Tabs(deck) = &self.content else {
            return None;
        };
        let mut tabs = Vec::with_capacity(deck.len());
        let mut active = None;
        let raw_active = deck.active_index();
        for (index, slot) in deck.items().iter().enumerate() {
            let Some(path) = slot.repo_path() else {
                continue;
            };
            if index == raw_active {
                active = Some(tabs.len());
            }
            tabs.push(path.to_path_buf());
        }
        if tabs.is_empty() {
            return None;
        }
        Some(SavedWindow {
            tabs,
            active: active.unwrap_or(0),
        })
    }

    fn persist_session(&self, cx: &mut App) {
        let saved = self.saved_window();

        #[cfg(test)]
        if let Some(dir) = &self.recent_dir {
            let file = dir.join("session.json");
            let session = SavedSession {
                windows: saved.into_iter().collect(),
            };
            if let Err(error) = session.save(&file) {
                log::warn!("cannot save test window session: {error}");
            }
            return;
        }

        #[cfg(test)]
        if std::env::var_os("STRATA_CONFIG_DIR").is_none() {
            return;
        }

        if !cx.has_global::<SessionRegistry>() {
            return;
        }
        let window_id = self.window_id;
        let snapshot = cx.update_global::<SessionRegistry, _>(|reg, _| {
            if let Some(entry) = reg.windows.iter_mut().find(|(id, _)| *id == window_id) {
                if let Some(saved) = saved {
                    entry.1 = saved;
                } else {
                    reg.windows.retain(|(id, _)| *id != window_id);
                }
            } else if let Some(saved) = saved {
                reg.windows.push((window_id, saved));
            }
            SavedSession {
                windows: reg
                    .windows
                    .iter()
                    .map(|(_, window)| window.clone())
                    .collect(),
            }
        });
        if !snapshot.windows.is_empty()
            && let Err(error) = snapshot.save_default()
        {
            log::warn!("cannot save window session: {error}");
        }
    }

    /// Opens `path` in the active tab of this window, resetting its
    /// back-navigation stack (or opens the first tab from the welcome screen).
    ///
    /// The path may be any directory inside a repository; the repository's own
    /// root is what gets opened and remembered. `Err` is a message for the
    /// picker, which stays open so the user can correct the path.
    pub(crate) fn open_path(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let root = crate::picker::resolve_repository(path)?;
        if !is_current(self.current_path(), &root) {
            self.switch_repository(&root, OpenDisposition::Replace, window, cx)?;
        }
        self.finish_navigation(window, cx);
        Ok(())
    }

    /// Opens `path` in a new tab of this window, or switches to its existing tab
    /// if the same repository is already open here.
    pub(crate) fn open_path_in_new_tab(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let root = crate::picker::resolve_repository(path)?;
        let canon = canonical(root.clone());

        if let Content::Tabs(deck) = &self.content {
            if let Some(existing) = deck
                .items()
                .iter()
                .position(|slot| slot.repo_path() == Some(canon.as_path()))
            {
                let switched = self.activate_index(existing, window, cx);
                self.record_recent(&canon);
                if switched {
                    self.persist_session(cx);
                }
                self.finish_navigation(window, cx);
                return Ok(());
            }

            let launch = self.open_launch(&root, cx);
            if let Launch::Error { error, .. } = &launch {
                return Err(t!(
                    "workspace.cannot_open",
                    path = root.display(),
                    error = error
                )
                .to_string());
            }
            let slot_path = repo_path(&launch);
            let title = launch.title();
            let workspace = cx.weak_entity();
            let view = cx.new(|cx| {
                RepoView::new_in_workspace_active(launch, Some(workspace), None, true, window, cx)
            });

            if let Content::Tabs(deck) = &mut self.content {
                if let Some(active) = deck.active() {
                    active.set_active(false, cx);
                }
                deck.open(TabSlot::Repo(RepoSlot {
                    view,
                    path: slot_path.clone(),
                    parents: NavigationStack::default(),
                }));
            }
            window.set_window_title(&title);
            if let Some(path) = &slot_path {
                self.record_recent(path);
            }
            self.persist_session(cx);
            self.refresh_tab_strip(cx);
            self.finish_navigation(window, cx);
            return Ok(());
        }

        // Welcome screen has no open tabs yet, so opening in a new tab creates
        // the first tab.
        self.open_path(&root, window, cx)
    }

    /// Opens the standalone Compare setup dialog over the workspace window.
    pub(crate) fn open_compare_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::compare::open_compare_dialog(cx.entity(), window, cx);
    }

    /// Opens a standalone file or folder comparison in a new workspace tab.
    pub(crate) fn open_compare_target(
        &mut self,
        target: CompareTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = cx.weak_entity();
        let view = cx.new(|cx| CompareView::new(target, workspace, cx));
        let title = view.read(cx).window_title();
        let hint_path = view.read(cx).hint_path().map(Path::to_path_buf);
        let slot = TabSlot::Compare(CompareSlot { view, hint_path });

        match &mut self.content {
            Content::Welcome(_) => {
                // Keep Workspace's appearance subscription because CompareView
                // does not subscribe to window appearance on its own.
                self.content = Content::Tabs(TabDeck::from_items(vec![slot], 0));
            }
            Content::Tabs(deck) => {
                if let Some(active) = deck.active() {
                    active.set_active(false, cx);
                }
                deck.open(slot);
            }
        }
        window.set_window_title(&title);
        self.persist_session(cx);
        self.refresh_tab_strip(cx);
        self.finish_navigation(window, cx);
    }

    /// Opens a submodule or linked worktree in the current tab, pushing the
    /// current repository onto that tab's navigation stack so the user can return
    /// to it.
    pub(crate) fn open_child(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let root = crate::picker::resolve_repository(path)?;
        if !is_current(self.current_path(), &root) {
            self.switch_repository(&root, OpenDisposition::PushParent, window, cx)?;
        }
        self.finish_navigation(window, cx);
        Ok(())
    }

    /// Returns to the parent repository at the top of the active slot's stack.
    pub(crate) fn navigate_back(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let target = match &self.content {
            Content::Tabs(deck) => deck
                .active()
                .and_then(TabSlot::as_repo)
                .and_then(|slot| slot.parents.current().cloned()),
            Content::Welcome(_) => None,
        };
        let Some(target) = target else {
            return Ok(());
        };
        let root = crate::picker::resolve_repository(&target.path)?;
        let launch = self.open_launch(&root, cx);
        if let Launch::Error { error, .. } = &launch {
            return Err(t!(
                "workspace.cannot_open",
                path = root.display(),
                error = error
            )
            .to_string());
        }

        let mut parents = match &self.content {
            Content::Tabs(deck) => deck
                .active()
                .and_then(TabSlot::as_repo)
                .map(|slot| slot.parents.clone())
                .unwrap_or_default(),
            Content::Welcome(_) => NavigationStack::default(),
        };
        parents.pop();
        self.install_repo(launch, parents, window, cx);
        self.finish_navigation(window, cx);
        Ok(())
    }

    fn open_launch(&self, root: &Path, cx: &App) -> Launch {
        let git_executable = Settings::get(cx).behaviour.git_executable.clone();
        #[cfg(test)]
        {
            (self.launch_repo)(root, git_executable)
        }
        #[cfg(not(test))]
        {
            Launch::open_with(root, git_executable)
        }
    }

    fn switch_repository(
        &mut self,
        root: &Path,
        disposition: OpenDisposition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let launch = self.open_launch(root, cx);
        if let Launch::Error { error, .. } = &launch {
            return Err(t!(
                "workspace.cannot_open",
                path = root.display(),
                error = error
            )
            .to_string());
        }
        let parents = match disposition {
            OpenDisposition::Replace => NavigationStack::default(),
            OpenDisposition::PushParent => {
                let mut parents = match &self.content {
                    Content::Tabs(deck) => deck
                        .active()
                        .and_then(TabSlot::as_repo)
                        .map(|slot| slot.parents.clone())
                        .unwrap_or_default(),
                    Content::Welcome(_) => NavigationStack::default(),
                };
                if let Some(current_path) = self.current_path() {
                    let name = self
                        .current_repo_name(cx)
                        .unwrap_or_else(|| recent::repository_name(current_path));
                    parents.push(ParentRepo {
                        path: current_path.to_path_buf(),
                        name,
                    });
                }
                parents
            }
        };
        self.install_repo(launch, parents, window, cx);
        Ok(())
    }

    fn install_repo(
        &mut self,
        launch: Launch,
        parents: NavigationStack,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = repo_path(&launch);
        let title = launch.title();
        let parent_name = parents.current().map(|parent| parent.name.clone());
        let workspace = cx.weak_entity();
        let view = cx
            .new(|cx| RepoView::new_in_workspace(launch, Some(workspace), parent_name, window, cx));
        let slot = TabSlot::Repo(RepoSlot {
            view,
            path: path.clone(),
            parents,
        });

        self._appearance = None;
        match &mut self.content {
            Content::Welcome(_) => {
                self.content = Content::Tabs(TabDeck::from_items(vec![slot], 0));
            }
            Content::Tabs(deck) => {
                if let Some(active_slot) = deck.active_mut() {
                    let previous = std::mem::replace(active_slot, slot);
                    detach_and_drop_slot(previous, cx);
                } else {
                    deck.open(slot);
                }
            }
        }

        // A new view syncs the title only when it differs from the one it
        // was built with, so the swap has to set the window title itself.
        window.set_window_title(&title);
        if let Some(path) = &path {
            self.record_recent(path);
        }
        self.persist_session(cx);
    }

    pub(crate) fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.activate_index(index, window, cx) {
            self.persist_session(cx);
            self.finish_navigation(window, cx);
        }
    }

    pub(crate) fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Tabs(deck) = &mut self.content else {
            return;
        };
        let old_active = deck.active_index();
        let Some(new_active) = deck.cycle(delta) else {
            return;
        };
        self.transition_active_tab(old_active, new_active, window, cx);
    }

    pub(crate) fn jump_to_tab(
        &mut self,
        one_based: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Content::Tabs(deck) = &mut self.content else {
            return;
        };
        let old_active = deck.active_index();
        let Some(new_active) = deck.jump(one_based) else {
            return;
        };
        self.transition_active_tab(old_active, new_active, window, cx);
    }

    fn activate_index(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Content::Tabs(deck) = &mut self.content else {
            return false;
        };
        let old_active = deck.active_index();
        if !deck.select(index) {
            return false;
        }
        self.transition_active_after_deck_change(old_active, index, window, cx);
        true
    }

    fn transition_active_tab(
        &mut self,
        old_active: usize,
        new_active: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.transition_active_after_deck_change(old_active, new_active, window, cx);
        self.persist_session(cx);
        self.finish_navigation(window, cx);
    }

    fn transition_active_after_deck_change(
        &mut self,
        old_active: usize,
        new_active: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Content::Tabs(deck) = &self.content else {
            return;
        };
        if let Some(old_slot) = deck.get(old_active) {
            old_slot.set_active(false, cx);
        }
        if let Some(new_slot) = deck.get(new_active) {
            new_slot.set_active(true, cx);
            let title = new_slot.window_title(cx);
            window.set_window_title(&title);
        }
    }

    pub(crate) fn reorder_tab(
        &mut self,
        from: usize,
        to: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Content::Tabs(deck) = &mut self.content else {
            return;
        };
        if deck.reorder(from, to) {
            self.persist_session(cx);
            self.refresh_tab_strip(cx);
        }
    }

    pub(crate) fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Tabs(deck) = &mut self.content else {
            window.remove_window();
            return;
        };
        let old_active = deck.active_index();
        // Keep the position for "Reopen closed tab": closing drops the slot.
        if let Some(path) = deck
            .get(index)
            .and_then(TabSlot::repo_path)
            .map(Path::to_path_buf)
        {
            self.closed_tabs.push(ClosedTab { path, index });
        }
        match deck.close(index) {
            CloseOutcome::OutOfBounds => {}
            CloseOutcome::ClosedLast(removed) => {
                detach_and_drop_slot(removed, cx);
                window.remove_window();
            }
            CloseOutcome::Closed {
                removed,
                new_active,
            } => {
                detach_and_drop_slot(removed, cx);
                if index == old_active
                    && let Some(new_slot) = deck.get(new_active)
                {
                    new_slot.set_active(true, cx);
                    let title = new_slot.window_title(cx);
                    window.set_window_title(&title);
                    let focus = new_slot.view_focus(cx);
                    window.defer(cx, move |window, cx| focus.focus(window, cx));
                }
                self.persist_session(cx);
                self.refresh_tab_strip(cx);
            }
        }
    }

    pub(crate) fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.content {
            Content::Welcome(_) => window.remove_window(),
            Content::Tabs(deck) => {
                let active = deck.active_index();
                self.close_tab(active, window, cx);
            }
        }
    }

    /// Reopens the most recently closed repository tab at the position it had.
    ///
    /// Entries whose directory has disappeared, or is no longer a repository,
    /// are skipped newest first, so the shortcut still finds the tab the user
    /// means after a folder was deleted or moved.
    pub(crate) fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(closed) = self.closed_tabs.pop_existing(|path| path.is_dir()) {
            let launch = self.open_launch(&closed.path, cx);
            if let Launch::Error { error, .. } = &launch {
                log::warn!("cannot reopen {}: {error}", closed.path.display());
                continue;
            }
            let path = repo_path(&launch);
            let title = launch.title();
            let workspace = cx.weak_entity();
            let view = cx.new(|cx| {
                RepoView::new_in_workspace_active(launch, Some(workspace), None, true, window, cx)
            });
            let slot = TabSlot::Repo(RepoSlot {
                view,
                path: path.clone(),
                parents: NavigationStack::default(),
            });
            let inserted = if let Content::Tabs(deck) = &mut self.content {
                if let Some(active) = deck.active_mut() {
                    active.set_active(false, cx);
                }
                deck.insert(closed.index, slot)
            } else {
                // The welcome screen never has entries of its own, but a
                // reopened tab is still a better outcome than nothing.
                self.content = Content::Tabs(TabDeck::from_items(vec![slot], 0));
                0
            };

            window.set_window_title(&title);
            if let Some(path) = &path {
                self.record_recent(path);
            }
            self.persist_session(cx);
            self.refresh_tab_strip(cx);
            if let Content::Tabs(deck) = &self.content
                && let Some(slot) = deck.get(inserted)
            {
                let focus = slot.view_focus(cx);
                window.defer(cx, move |window, cx| focus.focus(window, cx));
            }
            self.finish_navigation(window, cx);
            return;
        }
    }

    /// Repaints the active tab view so the tab strip reflects changes from
    /// background tabs (e.g. an operation starting or finishing) or tab list
    /// updates without waking inactive tabs.
    pub(crate) fn refresh_tab_strip(&self, cx: &mut Context<Self>) {
        if let Content::Tabs(deck) = &self.content
            && let Some(active) = deck.active()
        {
            active.notify_view(cx);
        }
        cx.notify();
    }

    /// Renders the tab strip when 2 or more tabs are open in this window;
    /// returns `None` for 0 or 1 tabs so single-tab windows look identical.
    pub(crate) fn render_tab_strip(
        workspace: &Entity<Self>,
        active_name: Option<&str>,
        active_busy: bool,
        design: &Design,
        cx: &App,
    ) -> Option<AnyElement> {
        let ws = workspace.read(cx);
        let Content::Tabs(deck) = &ws.content else {
            return None;
        };
        if deck.len() < 2 {
            return None;
        }
        let active_index = deck.active_index();
        let workspace_weak = workspace.downgrade();

        let tabs = deck
            .items()
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let is_active = index == active_index;
                let (label, busy, hint_path) =
                    slot.tab_display_info(is_active, active_name, active_busy, cx);
                let select_ws = workspace_weak.clone();
                let close_ws = workspace_weak.clone();
                let reorder_ws = workspace_weak.clone();
                // The active tab's fill already separates it from what is
                // beside it; a divider belongs only between two quiet tabs.
                let separator = index > 0 && !is_active && active_index != index - 1;

                let mut item =
                    crate::ui::tab(("repo-tab", index), index, label.clone(), is_active, design)
                        .separator(separator)
                        .busy(busy)
                        .tooltip(label)
                        .on_select(move |_, window, cx| {
                            let _ =
                                select_ws.update(cx, |this, cx| this.select_tab(index, window, cx));
                        })
                        .on_close(move |window, cx| {
                            let _ =
                                close_ws.update(cx, |this, cx| this.close_tab(index, window, cx));
                        })
                        .on_reorder(move |from, to, window, cx| {
                            let _ = reorder_ws
                                .update(cx, |this, cx| this.reorder_tab(from, to, window, cx));
                        });

                if let Some(path) = hint_path {
                    item = item.hint(&path);
                }
                item
            })
            .collect::<Vec<_>>();

        Some(
            crate::ui::tab_strip("repo-tab-strip", design)
                .children(tabs)
                .into_any_element(),
        )
    }

    fn finish_navigation(&self, window: &mut Window, cx: &mut Context<Self>) {
        // A dialog closes first: it would otherwise hand focus back to a handle
        // that belonged to the view the swap just dropped. Both are deferred so
        // the closing dialog cannot run while the view that opened it is still
        // being updated.
        if window.has_active_dialog(cx) {
            window.defer(cx, |window, cx| window.close_dialog(cx));
        }
        if let Some(focus) = self.content_focus(cx) {
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        }
        cx.notify();
    }

    /// The focus handle of whichever view fills the window.
    fn content_focus(&self, cx: &App) -> Option<gpui::FocusHandle> {
        match &self.content {
            Content::Tabs(deck) => deck.active().map(|slot| slot.view_focus(cx)),
            Content::Welcome(view) => Some(view.read(cx).input_focus(cx)),
        }
    }

    /// `OpenRepository`: the same picker as a dialog over the current view.
    pub(crate) fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker_for(OpenTarget::Current, window, cx);
    }

    pub(crate) fn open_picker_for(
        &mut self,
        target: OpenTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let has_open_repo = matches!(&self.content, Content::Tabs(_));
        let workspace = cx.entity();
        let picker = cx.new(|cx| Picker::new(Mode::Dialog, target, workspace, window, cx));
        let input = picker.read(cx).input().clone();
        let title = match target {
            OpenTarget::Current => t!("picker.open_repository").to_string(),
            OpenTarget::NewTab => t!("picker.open_new_tab").to_string(),
        };
        window.open_dialog(cx, move |dialog, _, cx| {
            let content_picker = picker.clone();
            let footer_picker = picker.clone();
            let clone_footer_picker = picker.clone();
            let init_footer_picker = picker.clone();
            let new_tab_footer_picker = picker.clone();
            let confirm_picker = picker.clone();
            // Five buttons outgrow the dialog in longer languages; they wrap
            // onto a second row instead of spilling past its left edge.
            let footer = DialogFooter::new()
                .flex_wrap()
                .child(
                    crate::ui::button(
                        "picker-dialog-open",
                        format!("{}…", t!("picker.open_repository")),
                    )
                    .size(crate::ui::ButtonSize::Medium)
                    .on_click(move |_, window, cx| {
                        footer_picker.update(cx, |picker, cx| picker.open_native(window, cx));
                    }),
                )
                .child(
                    crate::ui::button("picker-dialog-clone", t!("picker.clone").to_string())
                        .kind(crate::ui::ButtonKind::Normal)
                        .size(crate::ui::ButtonSize::Medium)
                        .on_click(move |_, window, cx| {
                            clone_footer_picker
                                .update(cx, |picker, cx| picker.open_clone(window, cx));
                        }),
                )
                .child(
                    crate::ui::button("picker-dialog-init", t!("picker.new").to_string())
                        .kind(crate::ui::ButtonKind::Normal)
                        .size(crate::ui::ButtonSize::Medium)
                        .on_click(move |_, window, cx| {
                            init_footer_picker
                                .update(cx, |picker, cx| picker.open_init(window, cx));
                        }),
                )
                .when(target == OpenTarget::Current && has_open_repo, |footer| {
                    footer.child(
                        crate::ui::button(
                            "picker-dialog-open-new-tab",
                            t!("workspace.open_new_tab_ellipsis").to_string(),
                        )
                        .kind(crate::ui::ButtonKind::Normal)
                        .size(crate::ui::ButtonSize::Medium)
                        .on_click(move |_, window, cx| {
                            new_tab_footer_picker.update(cx, |picker, cx| {
                                picker.open_native_in(OpenTarget::NewTab, window, cx)
                            });
                        }),
                    )
                })
                .child(
                    crate::ui::button("picker-dialog-cancel", t!("common.cancel").to_string())
                        .kind(crate::ui::ButtonKind::Normal)
                        .size(crate::ui::ButtonSize::Medium)
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                );
            let zoom = Settings::get(cx).appearance.zoom();
            dialog
                .title(title.clone())
                .w(px(720.0 * zoom))
                .content(move |content, _, _| content.child(content_picker.clone()))
                .footer(footer)
                .on_ok(move |_, window, cx| {
                    // A successful open closes the dialog itself, one frame
                    // later, so the confirm must not close it a second time.
                    confirm_picker.update(cx, |picker, cx| picker.open_selected(window, cx));
                    false
                })
        });
        // The dialog's shell takes the focus when it opens; the field needs it
        // to receive what the user types.
        crate::ui::focus_input(&input, window, cx);
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_count = self.tab_count();
        let content: AnyElement = match &self.content {
            Content::Welcome(view) => view.clone().into_any_element(),
            Content::Tabs(deck) => match deck.active() {
                Some(slot) => slot.render_element(),
                None => div().into_any_element(),
            },
        };
        let mut root = div()
            .size_full()
            .key_context(WORKSPACE)
            .on_action(cx.listener(|this, _: &OpenRepository, window, cx| {
                this.open_picker_for(OpenTarget::Current, window, cx);
            }))
            .on_action(cx.listener(|_, _: &CloneRepository, window, cx| {
                crate::clone_dialog::open_clone(cx.entity(), OpenTarget::Current, window, cx);
            }))
            .on_action(cx.listener(|_, _: &NewRepository, window, cx| {
                crate::clone_dialog::open_init(cx.entity(), OpenTarget::Current, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| {
                this.open_picker_for(OpenTarget::NewTab, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &CloseTab, window, cx| this.close_active_tab(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ReopenClosedTab, window, cx| {
                this.reopen_closed_tab(window, cx);
            }));
        root.text_style().font_fallbacks = Some(crate::style::font_fallbacks());
        if tab_count >= 2 {
            root = root
                .on_action(
                    cx.listener(|this, _: &NextTab, window, cx| this.cycle_tab(1, window, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &PreviousTab, window, cx| this.cycle_tab(-1, window, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &SelectTab1, window, cx| this.jump_to_tab(1, window, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &SelectTab2, window, cx| this.jump_to_tab(2, window, cx)),
                );
            if tab_count >= 3 {
                root =
                    root.on_action(cx.listener(|this, _: &SelectTab3, window, cx| {
                        this.jump_to_tab(3, window, cx)
                    }));
            }
            if tab_count >= 4 {
                root =
                    root.on_action(cx.listener(|this, _: &SelectTab4, window, cx| {
                        this.jump_to_tab(4, window, cx)
                    }));
            }
            if tab_count >= 5 {
                root =
                    root.on_action(cx.listener(|this, _: &SelectTab5, window, cx| {
                        this.jump_to_tab(5, window, cx)
                    }));
            }
            if tab_count >= 6 {
                root =
                    root.on_action(cx.listener(|this, _: &SelectTab6, window, cx| {
                        this.jump_to_tab(6, window, cx)
                    }));
            }
            if tab_count >= 7 {
                root =
                    root.on_action(cx.listener(|this, _: &SelectTab7, window, cx| {
                        this.jump_to_tab(7, window, cx)
                    }));
            }
            if tab_count >= 8 {
                root =
                    root.on_action(cx.listener(|this, _: &SelectTab8, window, cx| {
                        this.jump_to_tab(8, window, cx)
                    }));
            }
            if tab_count >= 9 {
                root =
                    root.on_action(cx.listener(|this, _: &SelectTab9, window, cx| {
                        this.jump_to_tab(9, window, cx)
                    }));
            }
        }
        root.child(content)
    }
}

fn track_window_release(entity: &Entity<Workspace>, cx: &mut App) {
    cx.observe_release(entity, |this, cx| {
        on_workspace_released(this.window_id, cx);
    })
    .detach();
}

/// Drops one closed window from `SessionRegistry` while leaving the last
/// saved session intact when the final window closes or the app is quitting.
fn on_workspace_released(window_id: WindowId, cx: &mut App) {
    if !cx.has_global::<SessionRegistry>() {
        return;
    }
    let snapshot = cx.update_global::<SessionRegistry, _>(|reg, _| {
        if reg.quitting {
            return None;
        }
        let had = reg.windows.iter().any(|(id, _)| *id == window_id);
        if !had {
            return None;
        }
        reg.windows.retain(|(id, _)| *id != window_id);
        if reg.windows.is_empty() {
            return None;
        }
        Some(SavedSession {
            windows: reg
                .windows
                .iter()
                .map(|(_, window)| window.clone())
                .collect(),
        })
    });
    if let Some(snapshot) = snapshot
        && let Err(error) = snapshot.save_default()
    {
        log::warn!("cannot update window session after close: {error}");
    }
}

/// Takes the slot's [`Session`] out and drops it on a background thread (or
/// cancels a running comparison) so closing or replacing a tab never blocks the
/// UI thread on worker shutdown.
fn detach_and_drop_slot(slot: TabSlot, cx: &mut App) {
    match slot {
        TabSlot::Repo(slot) => {
            #[cfg(feature = "terminal")]
            if let Some(terminal) = slot.view.update(cx, |view, _| {
                view.terminal_open = false;
                view.terminal.take()
            }) {
                terminal.update(cx, |panel, _| panel.close_all_sessions());
            }
            if let Some(session) = slot.view.update(cx, |view, _| view.detach_session()) {
                crate::repo_view::drop_session_in_background(session);
            }
        }
        TabSlot::Compare(slot) => {
            slot.view.update(cx, |view, _| view.cancel());
        }
    }
}

fn slot_display_name(slot: &RepoSlot, cx: &App) -> String {
    slot.view
        .read(cx)
        .repo_display_name()
        .map(str::to_owned)
        .or_else(|| slot.path.as_deref().map(recent::repository_name))
        .unwrap_or_else(|| t!("workspace.repository_tab").to_string())
}

/// Opens `root` in a new Strata window.
pub(crate) fn open_new_window(root: PathBuf, cx: &mut App) {
    let launch = Launch::open_with(&root, Settings::get(cx).behaviour.git_executable.clone());
    if let Launch::Error { error, .. } = &launch {
        log::warn!("cannot open {}: {error}", root.display());
        return;
    }
    open_tabs_window(
        vec![launch],
        0,
        DebugLaunch::default(),
        DEFAULT_WINDOW_SIZE,
        cx,
    );
}

/// Opens a repository window hosting `launches` with `active` selected.
pub(crate) fn open_tabs_window(
    launches: Vec<Launch>,
    active: usize,
    debug: DebugLaunch,
    window_size: (f32, f32),
    cx: &mut App,
) {
    let clamped = if launches.is_empty() {
        0
    } else {
        active.min(launches.len() - 1)
    };
    let title = launches
        .get(clamped)
        .map_or_else(|| t!("app.name").to_string(), Launch::title);
    let bounds = Bounds::centered(None, size(px(window_size.0), px(window_size.1)), cx);
    let min_size = size(
        px(crate::style::MIN_WINDOW_SIZE.0),
        px(crate::style::MIN_WINDOW_SIZE.1),
    );
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(crate::style::window_titlebar(title)),
        window_decorations: Some(WindowDecorations::Client),
        window_min_size: Some(min_size),
        focus: true,
        app_id: Some("strata".to_owned()),
        ..TitleBar::window_options()
    };
    #[cfg(target_os = "linux")]
    let options = {
        let mut options = options;
        options.icon = crate::linux_window_icon();
        options
    };
    let opened = cx.open_window(options, move |window, cx| {
        crate::settings::apply_theme(Some(window), cx);
        let content = cx.new(|cx| Workspace::new_with_tabs(launches, clamped, debug, window, cx));
        cx.new(|cx| Root::new(content, window, cx))
    });
    match opened {
        Ok(handle) => {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        Err(error) => log::warn!("cannot open a repository window: {error}"),
    }
}

/// Opens a standalone Compare window for `target`.
#[allow(dead_code)]
pub(crate) fn open_compare_window(target: CompareTarget, window_size: (f32, f32), cx: &mut App) {
    let title = format!("{} — {}", t!("compare.tab.prefix"), target.title());
    let bounds = Bounds::centered(None, size(px(window_size.0), px(window_size.1)), cx);
    let min_size = size(
        px(crate::style::MIN_WINDOW_SIZE.0),
        px(crate::style::MIN_WINDOW_SIZE.1),
    );
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(crate::style::window_titlebar(title)),
        window_decorations: Some(WindowDecorations::Client),
        window_min_size: Some(min_size),
        focus: true,
        app_id: Some("strata".to_owned()),
        ..TitleBar::window_options()
    };
    #[cfg(target_os = "linux")]
    let options = {
        let mut options = options;
        options.icon = crate::linux_window_icon();
        options
    };
    let opened = cx.open_window(options, move |window, cx| {
        crate::settings::apply_theme(Some(window), cx);
        let content = cx.new(|cx| Workspace::new_with_compare(target, window, cx));
        cx.new(|cx| Root::new(content, window, cx))
    });
    match opened {
        Ok(handle) => {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        Err(error) => log::warn!("cannot open a compare window: {error}"),
    }
}

/// Restores open windows and their tabs at startup when `restore_last_repository`
/// allows it, skipping repositories that no longer exist.
///
/// Falls back to the single most recent repository when no `session.json`
/// survives yet so upgrades from single-repository versions stay seamless.
pub(crate) fn restored_windows(cx: &App) -> Vec<RestoredWindow> {
    if !Settings::get(cx).behaviour.restore_last_repository {
        return Vec::new();
    }
    let git_executable = Settings::get(cx).behaviour.git_executable.clone();
    let session = SavedSession::load_default();
    let resolved = session.resolve_windows(|path| path.is_dir());

    let mut windows = Vec::new();
    for saved in resolved {
        let mut launches = Vec::with_capacity(saved.tabs.len());
        let mut active = None;
        for (index, path) in saved.tabs.iter().enumerate() {
            if let launch @ Launch::Repo { .. } = Launch::open_with(path, git_executable.clone()) {
                if index == saved.active {
                    active = Some(launches.len());
                }
                launches.push(launch);
            }
        }
        if !launches.is_empty() {
            windows.push(RestoredWindow {
                tabs: launches,
                active: active.unwrap_or(0),
            });
        }
    }
    if !windows.is_empty() {
        return windows;
    }

    let recents = RecentRepositories::load_default();
    if let Some(path) = recents.most_recent()
        && let launch @ Launch::Repo { .. } = Launch::open_with(path, git_executable)
    {
        windows.push(RestoredWindow {
            tabs: vec![launch],
            active: 0,
        });
    }
    windows
}

/// The repository a launch ended up showing, as the working-tree root.
fn repo_path(launch: &Launch) -> Option<PathBuf> {
    let Launch::Repo { session, .. } = launch else {
        return None;
    };
    session
        .location()
        .local_path()
        .map(|path| canonical(path.to_path_buf()))
}

fn canonical(path: PathBuf) -> PathBuf {
    strata_core::fs_path::canonicalize(&path)
}

fn is_current(current: Option<&Path>, root: &Path) -> bool {
    current.is_some_and(|current| current == canonical(root.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::VisualTestContext;
    use gpui_component::Root;
    use std::process::Command;

    fn parent(name: &str) -> ParentRepo {
        ParentRepo {
            path: PathBuf::from(format!("/src/{name}")),
            name: name.to_owned(),
        }
    }

    #[test]
    fn navigation_stack_pushes_and_pops_parents_in_lifo_order() {
        let mut stack = NavigationStack::default();
        assert!(stack.is_empty());
        assert_eq!(stack.current(), None);

        stack.push(parent("superproject"));
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.current(), Some(&parent("superproject")));

        stack.push(parent("sub-a"));
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.current(), Some(&parent("sub-a")));

        assert_eq!(stack.pop(), Some(parent("sub-a")));
        assert_eq!(stack.current(), Some(&parent("superproject")));

        assert_eq!(stack.pop(), Some(parent("superproject")));
        assert_eq!(stack.current(), None);
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn navigation_stack_treats_revisits_as_a_simple_stack_and_clears_on_replace() {
        let mut stack = NavigationStack::default();
        stack.push(parent("repo-a"));
        stack.push(parent("repo-b"));
        // Re-opening repo-a as a child pushes repo-b again rather than building a tree.
        stack.push(parent("repo-a"));
        assert_eq!(stack.len(), 3);

        stack.clear();
        assert!(stack.is_empty());
        assert_eq!(stack.current(), None);
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test User")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test User")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("run git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo_with_commit(dir: &Path) {
        std::fs::create_dir_all(dir).expect("create repo dir");
        git(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "hello\n").expect("write README");
        git(dir, &["add", "README.md"]);
        git(dir, &["commit", "-m", "initial commit"]);
    }

    #[gpui::test]
    fn switching_to_child_repo_drops_previous_view_and_navigates_back(
        cx: &mut gpui::TestAppContext,
    ) {
        let temp = tempfile::tempdir().expect("tempdir");
        let main_dir = temp.path().join("alpha-repo");
        let worktree_dir = temp.path().join("alpha-feature");
        let unrelated_dir = temp.path().join("beta-repo");
        let recent_dir = temp.path().join("config");

        init_repo_with_commit(&main_dir);
        git(
            &main_dir,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree_dir.to_str().expect("utf8 path"),
            ],
        );
        init_repo_with_commit(&unrelated_dir);

        let main_canon = canonical(main_dir.clone());
        let worktree_canon = canonical(worktree_dir.clone());
        let unrelated_canon = canonical(unrelated_dir.clone());

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
            crate::actions::bind(cx);
        });

        let workspace_cell: std::rc::Rc<std::cell::RefCell<Option<Entity<Workspace>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let workspace_cell_init = workspace_cell.clone();
        let initial_launch = Launch::open_quiet(&main_dir, None);
        let recent_dir_for_ws = recent_dir.clone();

        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), move |window, cx| {
                    let workspace = cx.new(|cx| {
                        let mut ws = Workspace::new(Some(initial_launch), window, cx);
                        ws.recent_dir = Some(recent_dir_for_ws.clone());
                        ws.launch_repo = Launch::open_quiet;
                        // Re-record into the test recent directory so assertions cover initial open too.
                        if let Some(path) = ws.current_path() {
                            ws.record_recent(path);
                        }
                        ws
                    });
                    *workspace_cell_init.borrow_mut() = Some(workspace.clone());
                    cx.new(|cx| Root::new(workspace, window, cx))
                })
            })
            .expect("open window");
        let mut visual_cx = VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();

        let workspace = workspace_cell.borrow().clone().expect("workspace created");
        let first_view_weak = visual_cx.update(|_, cx| match &workspace.read(cx).content {
            Content::Tabs(deck) => {
                let slot = deck.active().expect("active slot").repo();
                assert_eq!(slot.view.read(cx).parent_name(), None);
                slot.view.downgrade()
            }
            Content::Welcome(_) => panic!("expected repo tabs"),
        });

        // Open child worktree through RepoView -> Workspace plumbing.
        let first_view = first_view_weak.upgrade().expect("first view alive");
        visual_cx.update(|window, cx| {
            first_view.update(cx, |view, cx| {
                view.open_repository_in_window(&worktree_dir, window, cx);
            });
        });
        drop(first_view);
        visual_cx.run_until_parked();

        assert!(
            first_view_weak.upgrade().is_none(),
            "old RepoView must be dropped cleanly when switching repositories"
        );

        let second_view_weak = visual_cx.update(|_, cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.current_path(), Some(worktree_canon.as_path()));
            match &ws.content {
                Content::Tabs(deck) => {
                    let slot = deck.active().expect("active slot").repo();
                    assert_eq!(slot.view.read(cx).parent_name(), Some("alpha-repo"));
                    assert_eq!(slot.parents.len(), 1);
                    slot.view.downgrade()
                }
                Content::Welcome(_) => panic!("expected repo tabs"),
            }
        });

        // Recent list should now have worktree first, then main_dir.
        let recents = RecentRepositories::load(&recent_dir.join("recent.json"));
        assert_eq!(
            recents
                .entries()
                .iter()
                .map(|e| e.path.clone())
                .collect::<Vec<_>>(),
            // The recent list stores paths without Windows' `\\?\` prefix.
            [&worktree_canon, &main_canon]
                .into_iter()
                .map(|path| {
                    PathBuf::from(
                        path.to_string_lossy()
                            .trim_start_matches(r"\\?\")
                            .to_owned(),
                    )
                })
                .collect::<Vec<_>>()
        );

        // Navigate back via RepoView's NavigateBack action handler.
        visual_cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_action(Box::new(crate::actions::NavigateBack), cx);
        });
        visual_cx.run_until_parked();

        assert!(
            second_view_weak.upgrade().is_none(),
            "child RepoView must be dropped cleanly when navigating back"
        );
        visual_cx.update(|_, cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.current_path(), Some(main_canon.as_path()));
            match &ws.content {
                Content::Tabs(deck) => {
                    let slot = deck.active().expect("active slot").repo();
                    assert_eq!(slot.view.read(cx).parent_name(), None);
                    assert!(slot.parents.is_empty());
                }
                Content::Welcome(_) => panic!("expected repo tabs"),
            }
        });

        // Push child again, then replace via picker (`open_path`) and ensure stack clears.
        visual_cx.update(|window, cx| {
            workspace
                .update(cx, |ws, cx| ws.open_child(&worktree_dir, window, cx))
                .expect("open child");
        });
        visual_cx.run_until_parked();
        visual_cx.update(|window, cx| {
            workspace
                .update(cx, |ws, cx| ws.open_path(&unrelated_dir, window, cx))
                .expect("open unrelated");
        });
        visual_cx.run_until_parked();

        visual_cx.update(|_, cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.current_path(), Some(unrelated_canon.as_path()));
            match &ws.content {
                Content::Tabs(deck) => {
                    let slot = deck.active().expect("active slot").repo();
                    assert_eq!(slot.view.read(cx).parent_name(), None);
                    assert!(slot.parents.is_empty());
                }
                Content::Welcome(_) => panic!("expected repo tabs"),
            }
        });

        window
            .update(&mut visual_cx, |_, window, _| window.remove_window())
            .expect("close test window");
        visual_cx.run_until_parked();
    }

    #[gpui::test]
    fn tabs_keep_inactive_sessions_quiet_reorder_cycle_jump_persist_and_close_last_window(
        cx: &mut gpui::TestAppContext,
    ) {
        let temp = tempfile::tempdir().expect("tempdir");
        let alpha_dir = temp.path().join("alpha");
        let beta_dir = temp.path().join("beta");
        let gamma_dir = temp.path().join("gamma");
        let config_dir = temp.path().join("config");

        init_repo_with_commit(&alpha_dir);
        init_repo_with_commit(&beta_dir);
        init_repo_with_commit(&gamma_dir);

        let alpha_canon = canonical(alpha_dir.clone());
        let beta_canon = canonical(beta_dir.clone());
        let gamma_canon = canonical(gamma_dir.clone());

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
            crate::actions::bind(cx);
        });

        let workspace_cell: std::rc::Rc<std::cell::RefCell<Option<Entity<Workspace>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let workspace_cell_init = workspace_cell.clone();
        let initial_launch = Launch::open_quiet(&alpha_dir, None);
        let config_dir_for_ws = config_dir.clone();

        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), move |window, cx| {
                    let workspace = cx.new(|cx| {
                        let mut ws = Workspace::new(Some(initial_launch), window, cx);
                        ws.recent_dir = Some(config_dir_for_ws.clone());
                        ws.launch_repo = Launch::open_quiet;
                        ws.persist_session(cx);
                        ws
                    });
                    *workspace_cell_init.borrow_mut() = Some(workspace.clone());
                    cx.new(|cx| Root::new(workspace, window, cx))
                })
            })
            .expect("open window");
        let mut visual_cx = VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();

        let workspace = workspace_cell.borrow().clone().expect("workspace created");
        let alpha_weak = visual_cx.update(|_, cx| match &workspace.read(cx).content {
            Content::Tabs(deck) => {
                assert_eq!(deck.len(), 1);
                let slot = deck.active().expect("active slot").repo();
                assert!(slot.view.read(cx).is_active());
                assert!(slot.view.read(cx).has_session());
                slot.view.downgrade()
            }
            Content::Welcome(_) => panic!("expected repo tabs"),
        });

        // Opening a second repository in a new tab keeps the first RepoView + Session
        // alive in the background while deactivating it so it stops repainting/polling.
        visual_cx.update(|window, cx| {
            workspace
                .update(cx, |ws, cx| ws.open_path_in_new_tab(&beta_dir, window, cx))
                .expect("open beta tab");
        });
        visual_cx.run_until_parked();

        let beta_weak = visual_cx.update(|_, cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.tab_count(), 2);
            assert_eq!(ws.current_path(), Some(beta_canon.as_path()));
            let Content::Tabs(deck) = &ws.content else {
                panic!("expected repo tabs");
            };
            let alpha_view = alpha_weak.upgrade().expect("alpha tab must remain alive");
            assert!(!alpha_view.read(cx).is_active());
            assert!(alpha_view.read(cx).has_session());

            let beta_slot = deck.active().expect("active slot").repo();
            assert!(beta_slot.view.read(cx).is_active());
            assert!(beta_slot.view.read(cx).has_session());
            beta_slot.view.downgrade()
        });

        // Re-opening an already open repository in a new tab switches to the existing tab
        // rather than duplicating it.
        visual_cx.update(|window, cx| {
            workspace
                .update(cx, |ws, cx| ws.open_path_in_new_tab(&alpha_dir, window, cx))
                .expect("focus existing alpha tab");
        });
        visual_cx.run_until_parked();

        visual_cx.update(|_, cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.tab_count(), 2);
            assert_eq!(ws.current_path(), Some(alpha_canon.as_path()));
            assert!(
                alpha_weak
                    .upgrade()
                    .expect("alpha alive")
                    .read(cx)
                    .is_active()
            );
            assert!(
                !beta_weak
                    .upgrade()
                    .expect("beta alive")
                    .read(cx)
                    .is_active()
            );
        });

        // Open a third repository tab (`gamma`).
        visual_cx.update(|window, cx| {
            workspace
                .update(cx, |ws, cx| ws.open_path_in_new_tab(&gamma_dir, window, cx))
                .expect("open gamma tab");
        });
        visual_cx.run_until_parked();

        let gamma_weak = visual_cx.update(|_, cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.tab_count(), 3);
            assert_eq!(ws.current_path(), Some(gamma_canon.as_path()));
            let Content::Tabs(deck) = &ws.content else {
                panic!("expected repo tabs");
            };
            deck.active()
                .expect("active gamma slot")
                .repo()
                .view
                .downgrade()
        });

        // Cycle forward (wraps 2 -> 0) and backward (wraps 0 -> 2) via actions, then jump.
        visual_cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_action(Box::new(NextTab), cx);
        });
        visual_cx.run_until_parked();
        visual_cx.update(|_, cx| {
            assert_eq!(
                workspace.read(cx).current_path(),
                Some(alpha_canon.as_path())
            );
        });

        visual_cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_action(Box::new(PreviousTab), cx);
        });
        visual_cx.run_until_parked();
        visual_cx.update(|_, cx| {
            assert_eq!(
                workspace.read(cx).current_path(),
                Some(gamma_canon.as_path())
            );
        });

        visual_cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_action(Box::new(SelectTab2), cx);
        });
        visual_cx.run_until_parked();
        visual_cx.update(|_, cx| {
            assert_eq!(
                workspace.read(cx).current_path(),
                Some(beta_canon.as_path())
            );
        });

        // Reorder active tab `beta` (index 1) to the front (index 0).
        visual_cx.update(|window, cx| {
            workspace.update(cx, |ws, cx| ws.reorder_tab(1, 0, window, cx));
        });
        visual_cx.run_until_parked();
        visual_cx.update(|_, cx| {
            let Content::Tabs(deck) = &workspace.read(cx).content else {
                panic!("expected repo tabs");
            };
            assert_eq!(deck.active_index(), 0);
            let paths = deck
                .items()
                .iter()
                .filter_map(|slot| slot.repo_path().map(Path::to_path_buf))
                .collect::<Vec<_>>();
            assert_eq!(
                paths,
                vec![beta_canon.clone(), alpha_canon.clone(), gamma_canon.clone()]
            );
        });

        // Session persistence wrote the reordered tabs and active index.
        let saved = SavedSession::load(&config_dir.join("session.json"));
        assert_eq!(
            saved,
            SavedSession {
                windows: vec![SavedWindow {
                    tabs: vec![beta_canon.clone(), alpha_canon.clone(), gamma_canon.clone()],
                    active: 0,
                }],
            }
        );

        // Removing a missing directory during restore skips it while preserving the rest.
        std::fs::remove_dir_all(&alpha_dir).expect("remove alpha dir");
        let resolved = saved.resolve_windows(|path| path.is_dir());
        assert_eq!(
            resolved,
            vec![SavedWindow {
                tabs: vec![beta_canon.clone(), gamma_canon.clone()],
                active: 0,
            }]
        );

        // Close an inactive tab (`alpha` at index 1): its view and session drop cleanly.
        visual_cx.update(|window, cx| {
            workspace.update(cx, |ws, cx| ws.close_tab(1, window, cx));
        });
        visual_cx.run_until_parked();
        assert!(
            alpha_weak.upgrade().is_none(),
            "closing an inactive tab must drop its RepoView and Session"
        );
        visual_cx.update(|_, cx| {
            assert_eq!(workspace.read(cx).tab_count(), 2);
            assert_eq!(
                workspace.read(cx).current_path(),
                Some(beta_canon.as_path())
            );
        });

        // Close active tab (`beta`) via CloseTab action: view drops and `gamma` activates.
        visual_cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_action(Box::new(CloseTab), cx);
        });
        visual_cx.run_until_parked();
        assert!(
            beta_weak.upgrade().is_none(),
            "closing active tab must drop its RepoView and Session"
        );
        visual_cx.update(|_, cx| {
            assert_eq!(workspace.read(cx).tab_count(), 1);
            assert_eq!(
                workspace.read(cx).current_path(),
                Some(gamma_canon.as_path())
            );
            assert!(
                gamma_weak
                    .upgrade()
                    .expect("gamma alive")
                    .read(cx)
                    .is_active(),
                "remaining tab must become active"
            );
        });

        // Closing the last remaining tab closes the window.
        visual_cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_action(Box::new(CloseTab), cx);
        });
        visual_cx.run_until_parked();
        drop(visual_cx);
        drop(workspace);
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(
                cx.windows().is_empty(),
                "closing the last tab should close the window (windows left: {})",
                cx.windows().len()
            );
        });
        cx.run_until_parked();
        if let Some(view) = gamma_weak.upgrade() {
            let has_session = cx.read(|cx| view.read(cx).has_session());
            drop(view);
            panic!("closing last tab kept RepoView alive (has_session={has_session})");
        }
    }

    #[gpui::test]
    fn compare_tab_opens_switches_and_drops_cleanly(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().expect("tempdir");
        let left_file = temp.path().join("left.txt");
        let right_file = temp.path().join("right.txt");
        std::fs::write(&left_file, "one\ntwo\n").unwrap();
        std::fs::write(&right_file, "one\nthree\n").unwrap();

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
            crate::actions::bind(cx);
        });

        let workspace_cell: std::rc::Rc<std::cell::RefCell<Option<Entity<Workspace>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let workspace_cell_init = workspace_cell.clone();
        let target = CompareTarget::TwoWayFiles {
            left: left_file,
            right: right_file,
        };

        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), move |window, cx| {
                    let workspace = cx.new(|cx| Workspace::new_with_compare(target, window, cx));
                    *workspace_cell_init.borrow_mut() = Some(workspace.clone());
                    cx.new(|cx| Root::new(workspace, window, cx))
                })
            })
            .expect("open window");
        let mut visual_cx = VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();

        let workspace = workspace_cell.borrow().clone().expect("workspace created");
        let compare_weak = visual_cx.update(|_, cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.tab_count(), 1);
            assert_eq!(ws.current_path(), None);
            let Content::Tabs(deck) = &ws.content else {
                panic!("expected compare tab");
            };
            let TabSlot::Compare(slot) = deck.active().expect("active slot") else {
                panic!("expected compare slot");
            };
            assert!(!slot.view.read(cx).is_loading());
            assert_eq!(slot.view.read(cx).tab_title(), "left.txt ↔ right.txt");
            slot.view.downgrade()
        });

        let second_left = temp.path().join("second_left.txt");
        let second_right = temp.path().join("second_right.txt");
        std::fs::write(&second_left, "alpha\n").unwrap();
        std::fs::write(&second_right, "beta\n").unwrap();
        let second_target = CompareTarget::TwoWayFiles {
            left: second_left,
            right: second_right,
        };
        visual_cx.update(|window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.open_compare_target(second_target, window, cx)
            });
        });
        visual_cx.run_until_parked();

        let second_weak = visual_cx.update(|_, cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.tab_count(), 2);
            let Content::Tabs(deck) = &ws.content else {
                panic!("expected tabs");
            };
            let TabSlot::Compare(slot) = deck.active().expect("active slot") else {
                panic!("expected compare slot");
            };
            assert!(!slot.view.read(cx).is_loading());
            assert_eq!(
                slot.view.read(cx).tab_title(),
                "second_left.txt ↔ second_right.txt"
            );
            slot.view.downgrade()
        });

        // Switch back to the first tab and verify active state updates.
        visual_cx.update(|window, cx| {
            workspace.update(cx, |ws, cx| ws.select_tab(0, window, cx));
        });
        visual_cx.run_until_parked();
        visual_cx.update(|_, cx| {
            let first = compare_weak.upgrade().expect("first compare alive");
            let second = second_weak.upgrade().expect("second compare alive");
            assert!(first.read(cx).is_active());
            assert!(!second.read(cx).is_active());
        });

        // Close the inactive second compare tab and confirm it drops cleanly.
        visual_cx.update(|window, cx| {
            workspace.update(cx, |ws, cx| ws.close_tab(1, window, cx));
        });
        visual_cx.run_until_parked();
        assert!(second_weak.upgrade().is_none());

        // Close the last remaining compare tab via CloseTab action.
        visual_cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_action(Box::new(CloseTab), cx);
        });
        visual_cx.run_until_parked();
        drop(visual_cx);
        drop(workspace);
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(
                cx.windows().is_empty(),
                "closing the last compare tab should close the window"
            );
        });
        cx.run_until_parked();
        assert!(compare_weak.upgrade().is_none());
    }

    #[cfg(all(not(windows), feature = "terminal"))]
    #[gpui::test]
    fn workspace_tabs_preserve_terminal_sessions_on_switch_and_kill_on_close(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::time::{Duration, Instant};

        fn pid_alive(pid: u32) -> bool {
            Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let alpha_dir = temp.path().join("alpha");
        let beta_dir = temp.path().join("beta");
        let config_dir = temp.path().join("config");

        init_repo_with_commit(&alpha_dir);
        init_repo_with_commit(&beta_dir);

        cx.update(|cx| {
            cx.set_quit_mode(gpui::QuitMode::Explicit);
            gpui_component::init(cx);
            let mut settings = Settings::default();
            settings.tools.embedded_shell = Some("/bin/sh -c \"sleep 30\"".to_owned());
            cx.set_global(settings);
            crate::actions::bind(cx);
            crate::terminal_panel::bind(cx);
        });

        let workspace_cell: std::rc::Rc<std::cell::RefCell<Option<Entity<Workspace>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let workspace_cell_init = workspace_cell.clone();
        let initial_launch = Launch::open_quiet(&alpha_dir, None);
        let config_dir_for_ws = config_dir.clone();

        let window = cx
            .update(|cx| {
                cx.open_window(Default::default(), move |window, cx| {
                    let workspace = cx.new(|cx| {
                        let mut ws = Workspace::new(Some(initial_launch), window, cx);
                        ws.recent_dir = Some(config_dir_for_ws.clone());
                        ws.launch_repo = Launch::open_quiet;
                        ws
                    });
                    *workspace_cell_init.borrow_mut() = Some(workspace.clone());
                    cx.new(|cx| Root::new(workspace, window, cx))
                })
            })
            .expect("open window");
        let mut visual_cx = VisualTestContext::from_window(window.into(), cx);
        visual_cx.run_until_parked();

        let workspace = workspace_cell.borrow().clone().expect("workspace created");

        // Open terminal in alpha and create 2 terminal tabs.
        let alpha_pids = visual_cx.update(|window, cx| {
            let Content::Tabs(deck) = &workspace.read(cx).content else {
                panic!("expected repo tabs");
            };
            let alpha_view = deck.active().expect("alpha active").repo().view.clone();
            alpha_view.update(cx, |view, cx| {
                view.quiet_terminal = true;
                view.toggle_terminal(window, cx);
            });
            let panel = alpha_view
                .read(cx)
                .terminal
                .clone()
                .expect("alpha terminal created");
            panel.update(cx, |p, cx| p.new_tab(window, cx));
            assert_eq!(panel.read(cx).tab_count(), 2);
            panel
                .read(cx)
                .tab_child_pids()
                .into_iter()
                .map(|p| p.expect("alpha terminal pid"))
                .collect::<Vec<_>>()
        });
        assert_eq!(alpha_pids.len(), 2);

        // Open beta in a second repository tab with 1 terminal tab.
        visual_cx.update(|window, cx| {
            workspace
                .update(cx, |ws, cx| ws.open_path_in_new_tab(&beta_dir, window, cx))
                .expect("open beta tab");
        });
        visual_cx.run_until_parked();

        let beta_pid = visual_cx.update(|window, cx| {
            let Content::Tabs(deck) = &workspace.read(cx).content else {
                panic!("expected repo tabs");
            };
            let beta_view = deck.active().expect("beta active").repo().view.clone();
            beta_view.update(cx, |view, cx| {
                view.quiet_terminal = true;
                view.toggle_terminal(window, cx);
            });
            let panel = beta_view
                .read(cx)
                .terminal
                .clone()
                .expect("beta terminal created");
            assert_eq!(panel.read(cx).tab_count(), 1);
            panel.read(cx).active_child_pid().expect("beta pid")
        });

        // Switch away to alpha and back to beta: all sessions must survive.
        visual_cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_action(Box::new(PreviousTab), cx);
        });
        visual_cx.run_until_parked();
        for pid in alpha_pids.iter().chain(std::iter::once(&beta_pid)) {
            assert!(
                pid_alive(*pid),
                "switching repository tabs must not kill terminal PID {pid}"
            );
        }
        visual_cx.update(|_, cx| {
            let Content::Tabs(deck) = &workspace.read(cx).content else {
                panic!("expected repo tabs");
            };
            let alpha_view = &deck.items()[0].repo().view;
            let beta_view = &deck.items()[1].repo().view;
            assert_eq!(
                alpha_view
                    .read(cx)
                    .terminal
                    .as_ref()
                    .expect("alpha terminal")
                    .read(cx)
                    .tab_count(),
                2
            );
            assert_eq!(
                beta_view
                    .read(cx)
                    .terminal
                    .as_ref()
                    .expect("beta terminal")
                    .read(cx)
                    .tab_count(),
                1
            );
        });

        // Close beta repository tab: beta's terminal PTY must terminate, alpha's must stay alive.
        visual_cx.update(|window, cx| {
            workspace.update(cx, |ws, cx| ws.close_tab(1, window, cx));
        });
        visual_cx.run_until_parked();

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && pid_alive(beta_pid) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !pid_alive(beta_pid),
            "closing repository tab must kill its terminal PTY session"
        );
        for pid in &alpha_pids {
            assert!(
                pid_alive(*pid),
                "closing another repository tab must keep surviving tab's terminal PID {pid} alive"
            );
        }

        // Clean up remaining tab + window.
        window
            .update(&mut visual_cx, |_, window, _| window.remove_window())
            .expect("close test window");
        visual_cx.run_until_parked();
    }
}
