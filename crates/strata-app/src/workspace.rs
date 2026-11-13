//! What fills the window: the welcome screen or a repository view.
//!
//! `Root` is created with a fixed child, so the window's content is swapped one
//! level down: this view owns whichever child is current and replaces it when
//! the user opens another repository. Replacing the child releases the old
//! [`RepoView`] once the swap's effects are flushed, and with it its [`Session`],
//! whose worker threads are joined in `Session::drop`.
//!
//! Swapping also gives every repository one place to record "opened": the recent
//! list is written here, never by a view.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement, Render, Styled, Window, div, px,
};
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogFooter;
use strata_core::session::Session;

use crate::actions::OpenRepository;
use crate::picker::{Mode, Picker};
use crate::recent::{self, RecentRepositories};
use crate::repo_view::{Launch, RepoView};
use crate::settings::Settings;

/// The window's current content.
enum Content {
    /// The welcome screen, shown when no repository was opened at startup.
    Welcome(Entity<Picker>),
    /// A repository, synthetic (`--fake`) or real.
    Repo(Entity<RepoView>),
}

pub struct Workspace {
    content: Content,
    /// The working-tree root of the repository on screen. Canonical, so the
    /// picker can recognise the repository it is already showing.
    path: Option<PathBuf>,
}

impl Workspace {
    /// `None` opens the welcome screen; `Some` is whatever the command line or
    /// the restore settings resolved to.
    pub(crate) fn new(launch: Option<Launch>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        match launch {
            Some(launch) => {
                let path = repo_path(&launch);
                let view = cx.new(|cx| RepoView::new(launch, window, cx));
                // The session already opened successfully, so this is the point
                // where the repository counts as "opened".
                if let Some(path) = &path {
                    recent::record(path);
                }
                Self {
                    content: Content::Repo(view),
                    path,
                }
            }
            None => Self::welcome(window, cx),
        }
    }

    fn welcome(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.entity();
        let view = cx.new(|cx| Picker::new(Mode::Page, workspace, window, cx));
        let focus = view.read(cx).focus_handle();
        window.focus(&focus, cx);
        window.set_window_title("Strata");
        Self {
            content: Content::Welcome(view),
            path: None,
        }
    }

    /// Opens `path` in this window, replacing whatever it shows.
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
        if !is_current(self.path.as_deref(), &root) {
            let launch = Launch::open(&root);
            if let Launch::Error { error, .. } = &launch {
                return Err(format!("Cannot open {}: {error}", root.display()));
            }
            let path = repo_path(&launch);
            let title = launch.title();
            let view = cx.new(|cx| RepoView::new(launch, window, cx));
            // Replacing the content drops the previous view and its session.
            self.content = Content::Repo(view);
            self.path = path;
            // A new view syncs the title only when it differs from the one it
            // was built with, so the swap has to set the window title itself.
            window.set_window_title(&title);
            if let Some(path) = &self.path {
                recent::record(path);
            }
        }
        // A dialog closes first: it would otherwise hand focus back to a handle
        // that belonged to the view the swap just dropped. Both are deferred so
        // the closing dialog cannot run while the picker that opened it is still
        // being updated.
        if window.has_active_dialog(cx) {
            window.defer(cx, |window, cx| window.close_dialog(cx));
        }
        if let Some(focus) = self.content_focus(cx) {
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        }
        cx.notify();
        Ok(())
    }

    /// The focus handle of whichever view fills the window.
    fn content_focus(&self, cx: &App) -> Option<gpui::FocusHandle> {
        match &self.content {
            Content::Repo(view) => Some(view.read(cx).view_focus()),
            Content::Welcome(view) => Some(view.read(cx).focus_handle()),
        }
    }

    /// `OpenRepository`: the same picker as a dialog over the current view.
    fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let workspace = cx.entity();
        let picker = cx.new(|cx| Picker::new(Mode::Dialog, workspace, window, cx));
        let focus = picker.read(cx).focus_handle();
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
        // The dialog focuses its own card; the picker's keys need the focus.
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match &self.content {
            Content::Welcome(view) => view.clone().into_any_element(),
            Content::Repo(view) => view.clone().into_any_element(),
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
    match Launch::open(path) {
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
