//! What fills the window: the welcome screen or a repository view.
//!
//! `Root` is created with a fixed child, so the window's content is swapped one
//! level down: this view owns whichever child is current and replaces it when
//! the user opens another repository. Releasing the old [`RepoView`] frees it
//! (it keeps no handle to itself); the swap takes its [`Session`] out and hands
//! it to a background thread, because `Session::drop` joins its worker threads
//! and that must not happen on the UI thread.
//!
//! Swapping also gives every repository one place to record "opened": the recent
//! list is written here, never by a view. Each active repository lives in a
//! [`RepoSlot`] alongside its [`NavigationStack`], so future tabs can hold one
//! slot per tab without changing [`RepoView`].

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement, Render, Styled, Subscription, Window, div, px,
};
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;
use strata_core::session::Session;

use crate::actions::OpenRepository;
use crate::debug_launch::{self, DebugDialog, DebugLaunch};
use crate::picker::{Mode, Picker};
use crate::recent::{self, RecentRepositories};
use crate::repo_view::{Launch, RepoView};
use crate::settings::Settings;

/// A parent repository the user navigated away from when opening a submodule
/// or linked worktree in the current window.
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

/// One open repository view together with its canonical root and parent
/// history, self-contained so tabs can reuse it directly later.
struct RepoSlot {
    view: Entity<RepoView>,
    /// The working-tree root (or git dir for bare repos) on screen. Canonical,
    /// so the picker and child-opening can recognise the current repository.
    path: Option<PathBuf>,
    parents: NavigationStack,
}

/// The window's current content.
enum Content {
    /// The welcome screen, shown when no repository was opened at startup.
    Welcome(Entity<Picker>),
    /// A repository, synthetic (`--fake`) or real.
    Repo(RepoSlot),
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
    /// `System` follows the OS. A repository view subscribes for itself; the
    /// welcome screen has no view of its own to do that, so the workspace
    /// keeps the subscription while it is showing one.
    _appearance: Option<Subscription>,
    /// Overrides the recent-list directory in tests so test runs never mutate
    /// the user's real `recent.json` or depend on global env vars.
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
            Some(launch) => {
                let path = repo_path(&launch);
                let workspace = cx.weak_entity();
                let view = cx.new(|cx| {
                    RepoView::new_in_workspace(launch, Some(workspace), None, window, cx)
                });
                if debug.is_active() {
                    view.update(cx, |view, cx| view.set_debug_launch(debug, window, cx));
                }
                let slot = RepoSlot {
                    view,
                    path,
                    parents: NavigationStack::default(),
                };
                let workspace = Self {
                    content: Content::Repo(slot),
                    // The repository view subscribes on its own.
                    _appearance: None,
                    #[cfg(test)]
                    recent_dir: None,
                    #[cfg(test)]
                    launch_repo: Launch::open_with,
                };
                // The session already opened successfully, so this is the point
                // where the repository counts as "opened".
                if let Some(path) = workspace.current_path() {
                    workspace.record_recent(path);
                }
                workspace
            }
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

    fn welcome(appearance: Subscription, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.entity();
        let view = cx.new(|cx| Picker::new(Mode::Page, workspace, window, cx));
        let input = view.read(cx).input().clone();
        crate::ui::focus_input(&input, window, cx);
        window.set_window_title("Strata");
        Self {
            content: Content::Welcome(view),
            _appearance: Some(appearance),
            #[cfg(test)]
            recent_dir: None,
            #[cfg(test)]
            launch_repo: Launch::open_with,
        }
    }

    fn current_path(&self) -> Option<&Path> {
        match &self.content {
            Content::Welcome(_) => None,
            Content::Repo(slot) => slot.path.as_deref(),
        }
    }

    fn current_repo_name(&self, cx: &App) -> Option<String> {
        match &self.content {
            Content::Welcome(_) => None,
            Content::Repo(slot) => slot
                .view
                .read(cx)
                .repo_display_name()
                .map(str::to_owned)
                .or_else(|| slot.path.as_deref().map(recent::repository_name)),
        }
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
        recent::record(path);
    }

    /// Opens `path` in this window, replacing whatever it shows and resetting
    /// the back-navigation stack.
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

    /// Opens a submodule or linked worktree in the current window, pushing the
    /// current repository onto the navigation stack so the user can return to
    /// it.
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
            Content::Repo(slot) => slot.parents.current().cloned(),
            Content::Welcome(_) => None,
        };
        let Some(target) = target else {
            return Ok(());
        };
        let root = crate::picker::resolve_repository(&target.path)?;
        let launch = self.open_launch(&root, cx);
        if let Launch::Error { error, .. } = &launch {
            return Err(format!("Cannot open {}: {error}", root.display()));
        }

        let mut parents = match &self.content {
            Content::Repo(slot) => slot.parents.clone(),
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
            return Err(format!("Cannot open {}: {error}", root.display()));
        }
        let parents = match disposition {
            OpenDisposition::Replace => NavigationStack::default(),
            OpenDisposition::PushParent => {
                let mut parents = match &self.content {
                    Content::Repo(slot) => slot.parents.clone(),
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
        let slot = RepoSlot {
            view,
            path: path.clone(),
            parents,
        };
        // Taking the old session out explicitly keeps the hand-off obvious
        // at the swap site. The drop runs on another thread so the swap does
        // not wait for a running history walk to notice the shutdown; the
        // view's own memory is released when `previous` goes out of scope.
        let previous = std::mem::replace(&mut self.content, Content::Repo(slot));
        self._appearance = None;
        if let Content::Repo(previous) = previous
            && let Some(session) = previous.view.update(cx, |view, _| view.detach_session())
        {
            crate::repo_view::drop_session_in_background(session);
        }
        // A new view syncs the title only when it differs from the one it
        // was built with, so the swap has to set the window title itself.
        window.set_window_title(&title);
        if let Some(path) = &path {
            self.record_recent(path);
        }
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
            Content::Repo(slot) => Some(slot.view.read(cx).view_focus()),
            Content::Welcome(view) => Some(view.read(cx).input_focus(cx)),
        }
    }

    /// `OpenRepository`: the same picker as a dialog over the current view.
    pub(crate) fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let workspace = cx.entity();
        let picker = cx.new(|cx| Picker::new(Mode::Dialog, workspace, window, cx));
        let input = picker.read(cx).input().clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let content_picker = picker.clone();
            let footer_picker = picker.clone();
            let confirm_picker = picker.clone();
            dialog
                .title("Open repository")
                .w(px(600.0))
                .content(move |content, _, _| content.child(content_picker.clone()))
                .footer(
                    DialogFooter::new()
                        .child(
                            crate::ui::button("picker-dialog-open", "Open repository…")
                                .size(crate::ui::ButtonSize::Medium)
                                .on_click(move |_, window, cx| {
                                    footer_picker
                                        .update(cx, |picker, cx| picker.open_native(window, cx));
                                }),
                        )
                        .child(
                            crate::ui::button("picker-dialog-cancel", "Cancel")
                                .size(crate::ui::ButtonSize::Medium)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                )
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
        let content: AnyElement = match &self.content {
            Content::Welcome(view) => view.clone().into_any_element(),
            Content::Repo(slot) => slot.view.clone().into_any_element(),
        };
        div()
            .size_full()
            .on_action(
                cx.listener(|this, _: &OpenRepository, window, cx| this.open_picker(window, cx)),
            )
            .child(content)
    }
}

/// The repository that should reopen at startup: the newest recent entry, but
/// only while it still opens and only when the setting allows restoring.
pub(crate) fn restored_launch(cx: &App) -> Option<Launch> {
    if !Settings::get(cx).behaviour.restore_last_repository {
        return None;
    }
    let recents = RecentRepositories::load_default();
    let path = recents.most_recent()?;
    match Launch::open_with(path, Settings::get(cx).behaviour.git_executable.clone()) {
        launch @ Launch::Repo { .. } => Some(launch),
        // A repository that moved or was deleted is not an error at startup;
        // the welcome screen lists it so the user can retry or forget it.
        _ => None,
    }
}

/// The repository a launch ended up showing, as the working-tree root.
fn repo_path(launch: &Launch) -> Option<PathBuf> {
    let Launch::Repo { session, .. } = launch else {
        return None;
    };
    Some(canonical(worktree_root(session)))
}

fn worktree_root(session: &Session) -> PathBuf {
    let repo = session.repo();
    repo.workdir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.git_dir().to_path_buf())
}

fn canonical(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
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
            Content::Repo(slot) => {
                assert_eq!(slot.view.read(cx).parent_name(), None);
                slot.view.downgrade()
            }
            Content::Welcome(_) => panic!("expected repo content"),
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
                Content::Repo(slot) => {
                    assert_eq!(slot.view.read(cx).parent_name(), Some("alpha-repo"));
                    assert_eq!(slot.parents.len(), 1);
                    slot.view.downgrade()
                }
                Content::Welcome(_) => panic!("expected repo content"),
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
            vec![worktree_canon.clone(), main_canon.clone()]
        );

        // Navigate back via RepoView's NavigateBack action handler.
        visual_cx.update(|window, cx| {
            let _ = window.draw(cx);
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
                Content::Repo(slot) => {
                    assert_eq!(slot.view.read(cx).parent_name(), None);
                    assert!(slot.parents.is_empty());
                }
                Content::Welcome(_) => panic!("expected repo content"),
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
                Content::Repo(slot) => {
                    assert_eq!(slot.view.read(cx).parent_name(), None);
                    assert!(slot.parents.is_empty());
                }
                Content::Welcome(_) => panic!("expected repo content"),
            }
        });

        window
            .update(&mut visual_cx, |_, window, _| window.remove_window())
            .expect("close test window");
        visual_cx.run_until_parked();
    }
}
