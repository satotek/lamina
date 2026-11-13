//! The repository picker: the welcome screen and the "Open repository" dialog.
//!
//! Both are one view, [`Picker`], told which shape to take by [`Mode`]. The
//! welcome screen is the window's content when Strata starts without a
//! repository; the dialog is opened over whatever the window already shows by
//! the `OpenRepository` action. Opening a repository is delegated to
//! [`Workspace`](crate::workspace::Workspace), which owns the content swap.
//!
//! The picker only reads files: `recent.json` through [`RecentRepositories`] and
//! a cheap `gix` discovery to check a path. It runs no git commands.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, FontWeight,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, PathPromptOptions, Render,
    ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled, Subscription,
    UniformListScrollHandle, Window, actions, div, px, uniform_list,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, TitleBar};
use strata_core::repo::Repo;

use crate::recent::{RecentRepositories, RecentRepository};
use crate::scrollbar::WithScrollbar as _;
use crate::settings::Settings;
use crate::style::Design;
use crate::ui;
use crate::workspace::Workspace;

/// Welcome screen: the whole window, and the only keyboard focus.
const PICKER_PAGE: &str = "PickerPage";
/// The same list inside the repository dialog. `Enter` and `Escape` belong to
/// the dialog there, so only list movement is bound to this context.
const PICKER_DIALOG: &str = "PickerDialog";

/// Height of one remembered repository: a name line over a path line.
const ROW_HEIGHT: f32 = 38.0;
/// How much of the list the dialog shows before it scrolls.
const DIALOG_LIST_HEIGHT: f32 = 300.0;
/// The welcome screen has the room for a little more.
const PAGE_LIST_HEIGHT: f32 = 260.0;

#[cfg(target_os = "macos")]
const MODIFIER: &str = "Cmd";
#[cfg(not(target_os = "macos"))]
const MODIFIER: &str = "Ctrl";
#[cfg(target_os = "macos")]
const PATH_KEY: &str = "cmd-l";
#[cfg(not(target_os = "macos"))]
const PATH_KEY: &str = "ctrl-l";

actions!(
    strata,
    [
        /// Moves the picker's selection up one repository.
        PickerPrevious,
        /// Moves the picker's selection down one repository.
        PickerNext,
        /// Opens the selected repository, or the typed path.
        OpenSelectedRepository,
        /// Switches the text field between filtering and entering a path.
        TogglePathEntry,
        /// Leaves the typed-path field.
        PickerEscape,
    ]
);

/// Binds the picker's own commands. `Ctrl+O` itself is bound globally by
/// `actions::bind` and handled by the workspace around both shapes.
pub fn bind(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", PickerPrevious, Some(PICKER_PAGE)),
        KeyBinding::new("k", PickerPrevious, Some(PICKER_PAGE)),
        KeyBinding::new("down", PickerNext, Some(PICKER_PAGE)),
        KeyBinding::new("j", PickerNext, Some(PICKER_PAGE)),
        KeyBinding::new("enter", OpenSelectedRepository, Some(PICKER_PAGE)),
        KeyBinding::new("escape", PickerEscape, Some(PICKER_PAGE)),
        KeyBinding::new(PATH_KEY, TogglePathEntry, Some(PICKER_PAGE)),
        KeyBinding::new("up", PickerPrevious, Some(PICKER_DIALOG)),
        KeyBinding::new("k", PickerPrevious, Some(PICKER_DIALOG)),
        KeyBinding::new("down", PickerNext, Some(PICKER_DIALOG)),
        KeyBinding::new("j", PickerNext, Some(PICKER_DIALOG)),
        KeyBinding::new(PATH_KEY, TogglePathEntry, Some(PICKER_DIALOG)),
    ]);
}

/// Which shape the picker takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The welcome screen that fills the window.
    Page,
    /// The dialog opened over an existing view.
    Dialog,
}

/// A directory chooser for one repository.
pub struct Picker {
    mode: Mode,
    workspace: Entity<Workspace>,
    entity: Entity<Picker>,
    recents: RecentRepositories,
    /// Filters the list, or holds the path to open while `path_mode` is on.
    input: Entity<InputState>,
    path_mode: bool,
    /// Position in the filtered list, not an index into `recents`.
    selected: usize,
    error: Option<String>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    _input_events: Subscription,
}

impl Picker {
    pub(crate) fn new(
        mode: Mode,
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
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter repositories"));
        let _input_events =
            cx.subscribe(&input, |this, _input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    // The list narrowed (or the typed path changed); the old
                    // position no longer points at what the user is looking at.
                    this.selected = 0;
                    this.error = None;
                    cx.notify();
                }
                InputEvent::PressEnter {
                    secondary: false, ..
                } => {
                    // The input owns `Enter` while it has focus, so the picker
                    // submits from here instead of from its own binding.
                    let Some(handle) = cx.active_window() else {
                        return;
                    };
                    let mut failure = None;
                    let _ = handle.update(cx, |_, window, cx| match this.open(window, cx) {
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
        Self {
            mode,
            workspace,
            entity: cx.entity(),
            recents,
            input,
            path_mode: false,
            selected: 0,
            error: None,
            focus,
            scroll: UniformListScrollHandle::default(),
            _input_events,
        }
    }

    /// The picker's text field: the window focuses it so typing goes to the
    /// filter (or the typed path) without a click.
    pub(crate) fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    /// The focus handle of that field, for a view that only has the handle.
    pub(crate) fn input_focus(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }

    /// The repositories the list shows: `recents` filtered by the text field,
    /// or all of them while the field is being used to type a path.
    fn filtered(&self, cx: &App) -> Vec<usize> {
        let query = if self.path_mode {
            String::new()
        } else {
            self.input.read(cx).value().to_string()
        };
        filter_indices(self.recents.entries(), &query)
    }

    fn selected_path(&self, cx: &App) -> Option<PathBuf> {
        let index = *self.filtered(cx).get(self.selected)?;
        self.recents.get(index).map(|entry| entry.path.clone())
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.filtered(cx).len();
        if len == 0 {
            return;
        }
        self.selected = move_selection(self.selected, delta, len);
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }

    /// Resolves what the picker should open and hands it to the workspace.
    /// `Err` is a message for the user, and the dialog stays open to show it.
    fn open(&mut self, window: &mut Window, cx: &mut App) -> Result<(), String> {
        let path = if self.path_mode {
            let text = self.input.read(cx).value().trim().to_owned();
            if text.is_empty() {
                return Err("Type the path to a repository, then press Enter.".to_owned());
            }
            PathBuf::from(text)
        } else {
            self.selected_path(cx)
                .ok_or_else(|| "Choose a repository first.".to_owned())?
        };
        self.open_path(path, window, cx)
    }

    fn open_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        self.workspace
            .update(cx, |workspace, cx| workspace.open_path(&path, window, cx))
    }

    /// `Enter` from the welcome screen or the dialog's confirm button. Returns
    /// whether a repository was opened; the dialog closes only when one was.
    pub(crate) fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match self.open(window, cx) {
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

    /// Opens the platform's directory chooser. Its answer arrives on a later
    /// frame, so the picker stays exactly as it is until then.
    pub(crate) fn open_native(&mut self, window: &mut Window, cx: &mut App) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open repository".into()),
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
                        picker.path_mode = false;
                        match picker.open_path(path, window, cx) {
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
                    picker.error = Some(format!(
                        "The system file chooser could not be opened: {error}. \
                         Use “Type a path…” instead."
                    ));
                    cx.notify();
                });
            }
            Err(_) => {}
        })
        .detach();
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
        let len = self.filtered(cx).len();
        self.selected = self.selected.min(len.saturating_sub(1));
        cx.notify();
    }

    fn toggle_path_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path_mode = !self.path_mode;
        self.error = None;
        let placeholder = if self.path_mode {
            "Path to a repository"
        } else {
            "Filter repositories"
        };
        let focus = self.input.focus_handle(cx);
        self.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.set_placeholder(placeholder, window, cx);
        });
        window.focus(&focus, cx);
        self.selected = 0;
        cx.notify();
    }

    fn leave_path_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_mode {
            self.toggle_path_entry(window, cx);
        }
        self.focus.focus(window, cx);
    }

    fn body(&self, cx: &App, design: &Design) -> AnyElement {
        let filtered = self.filtered(cx);
        let selected = self.selected;
        let entries = self.recents.clone();
        let list_entity = self.entity.clone();
        let list_design = design.clone();
        let now = crate::recent::now_seconds();
        let count = filtered.len();
        let filtered = Rc::new(filtered);
        let list_id = match self.mode {
            Mode::Page => "picker-recent-page",
            Mode::Dialog => "picker-recent-dialog",
        };

        let mut column = div().flex().flex_col().gap_2().w_full();
        if self.mode == Mode::Page {
            column =
                column
                    .child(
                        div()
                            .text_size(px(22.0))
                            .font_weight(FontWeight::BOLD)
                            .child("Strata"),
                    )
                    .child(div().text_size(px(12.0)).text_color(design.muted).child(
                        "Open a repository to browse its history, working tree and stashes.",
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .pt_1()
                            .child(open_native_button(&self.entity, self.mode))
                            .child(path_toggle_button(&self.entity, self.mode, self.path_mode)),
                    );
        }

        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    // A filled field, like the main window's search, so the
                    // picker's input reads as the same control.
                    Input::new(&self.input).bg(design.field),
                )
                // The dialog's footer carries the chooser button; the path
                // toggle belongs next to the field it switches either way.
                .when(self.mode == Mode::Dialog, |row| {
                    row.child(path_toggle_button(&self.entity, self.mode, self.path_mode))
                }),
        );

        column = column.child(
            div().flex().items_center().justify_between().pt_1().child(
                ui::section_header("picker-recent", "RECENT REPOSITORIES", design)
                    .count(self.recents.len())
                    .px(px(0.0))
                    .h(px(20.0)),
            ),
        );

        if let Some(error) = &self.error {
            column = column.child(div().max_w(px(520.0)).child(ui::banner(
                ui::BannerKind::Danger,
                error.clone(),
                design,
            )));
        }

        if count == 0 {
            column = column.child(
                div()
                    .pt_2()
                    .text_size(px(11.5))
                    .text_color(design.muted)
                    .child(if self.recents.is_empty() {
                        "No recent repositories yet."
                    } else {
                        "No recent repository matches that filter."
                    }),
            );
        } else {
            let height = match self.mode {
                Mode::Page => PAGE_LIST_HEIGHT,
                Mode::Dialog => DIALOG_LIST_HEIGHT,
            };
            let list = uniform_list(list_id, count, move |range, _window, _cx| {
                range
                    .map(|position| {
                        let index = filtered[position];
                        let Some(entry) = entries.get(index) else {
                            return div().into_any_element();
                        };
                        recent_row(
                            position,
                            entry,
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
            .h(px(height))
            .w_full();
            column = column.child(list);
        }

        if self.mode == Mode::Page {
            column = column.child(
                div()
                    .pt_1()
                    .text_size(px(10.5))
                    .text_color(design.muted)
                    .child(format!(
                        "↑/↓ choose · Enter open · {MODIFIER}+O open… · {MODIFIER}+L type a path"
                    )),
            );
        }

        div()
            .w(px(560.0))
            .flex()
            .flex_col()
            .child(column)
            .into_any_element()
    }
}

/// The system chooser button. Element ids carry the mode because the welcome
/// screen and the dialog can be on screen at the same time.
fn open_native_button(entity: &Entity<Picker>, mode: Mode) -> ui::UiButton {
    let entity = entity.clone();
    ui::button(format!("picker-open-native-{mode:?}"), "Open repository…").on_click(
        move |_, window, cx| {
            entity.update(cx, |picker, cx| picker.open_native(window, cx));
        },
    )
}

/// Switches the text field between filtering the list and taking a path.
fn path_toggle_button(entity: &Entity<Picker>, mode: Mode, path_mode: bool) -> ui::UiButton {
    let entity = entity.clone();
    let label: SharedString = if path_mode {
        "Use the recent list".into()
    } else {
        "Type a path…".into()
    };
    ui::button(format!("picker-toggle-path-{mode:?}"), label)
        .ghost()
        .on_click(move |_, window, cx| {
            entity.update(cx, |picker, cx| picker.toggle_path_entry(window, cx));
        })
}

/// Row rendering shared by the page and the dialog.
fn recent_row(
    position: usize,
    entry: &RecentRepository,
    selected: bool,
    now: u64,
    entity: Entity<Picker>,
    design: &Design,
) -> AnyElement {
    let group = format!("recent-{position}");
    let name = entry.name();
    let path = entry.path.display().to_string();
    let opened = opened_label(entry.opened_at, now);

    let open_entity = entity.clone();
    let open = div()
        .id(("recent-open", position))
        .flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            open_entity.update(cx, |picker, cx| {
                picker.selected = position;
                picker.open_selected(window, cx);
            });
        })
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(design.text)
                        .child(name),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(10.5))
                        .text_color(design.muted)
                        .child(path),
                ),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(10.5))
                .text_color(design.muted)
                .child(opened),
        );

    let remove_entity = entity;
    let remove = div()
        .id(("recent-remove", position))
        .flex_none()
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .text_size(px(12.0))
        .text_color(design.muted)
        // Hidden until the row is hovered, so the list reads as names rather
        // than names with buttons. It stays clickable once shown.
        .opacity(0.0)
        .group_hover(group.clone(), |style| style.opacity(1.0))
        .on_click(move |_, _, cx| {
            remove_entity.update(cx, |picker, cx| picker.remove(position, cx));
        })
        .child("×");

    let mut row = div()
        .group(group)
        .flex()
        .items_center()
        .gap_2()
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_2()
        .rounded_sm()
        .hover(|style| style.bg(design.row_selected))
        .child(open)
        .child(remove);
    if selected {
        row = row.bg(design.row_selected);
    }
    row.into_any_element()
}

impl Render for Picker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let body = self.body(cx, &design);

        match self.mode {
            Mode::Page => div()
                .size_full()
                .flex()
                .flex_col()
                .bg(design.background)
                .text_color(design.text)
                .track_focus(&self.focus)
                .key_context(PICKER_PAGE)
                .on_action(cx.listener(|this, _: &PickerPrevious, _, cx| this.step(-1, cx)))
                .on_action(cx.listener(|this, _: &PickerNext, _, cx| this.step(1, cx)))
                .on_action(cx.listener(|this, _: &OpenSelectedRepository, window, cx| {
                    this.open_selected(window, cx);
                }))
                .on_action(cx.listener(|this, _: &TogglePathEntry, window, cx| {
                    this.toggle_path_entry(window, cx);
                }))
                .on_action(cx.listener(|this, _: &PickerEscape, window, cx| {
                    this.leave_path_entry(window, cx);
                }))
                .child(
                    // The same 44 px chrome strip as the main window, so the
                    // welcome screen is the same app with no repository open.
                    TitleBar::new()
                        .h(px(44.0))
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
                                        .gap(px(7.0))
                                        .child(crate::style::app_mark(16.0))
                                        .child(crate::style::wordmark(9.5, design.muted)),
                                )
                                .child(div().w(px(1.0)).h(px(18.0)).mx_1().bg(design.border))
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(design.muted)
                                        .child("Welcome"),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .items_center()
                        .justify_center()
                        .p_6()
                        .child(body),
                )
                .into_any_element(),
            Mode::Dialog => div()
                .flex()
                .flex_col()
                .w_full()
                .track_focus(&self.focus)
                .key_context(PICKER_DIALOG)
                .on_action(cx.listener(|this, _: &PickerPrevious, _, cx| this.step(-1, cx)))
                .on_action(cx.listener(|this, _: &PickerNext, _, cx| this.step(1, cx)))
                .on_action(cx.listener(|this, _: &TogglePathEntry, window, cx| {
                    this.toggle_path_entry(window, cx);
                }))
                .child(
                    div()
                        .pb_1()
                        .text_size(px(11.0))
                        .text_color(design.muted)
                        .child("Choose a repository to open in this window."),
                )
                .child(body)
                .into_any_element(),
        }
    }
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
        0..=59 => "just now".to_owned(),
        60..=3_599 => counted(seconds / 60, "minute"),
        3_600..=86_399 => counted(seconds / 3_600, "hour"),
        86_400..=2_591_999 => counted(seconds / 86_400, "day"),
        _ => crate::data::format_time(opened_at as i64),
    }
}

fn counted(value: u64, unit: &str) -> String {
    if value == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{value} {unit}s ago")
    }
}

/// Checks that `path` is a directory inside a git repository and returns the
/// repository's own root, so opening a subdirectory of a worktree recenters the
/// list on the worktree rather than on the subdirectory the user picked.
pub(crate) fn resolve_repository(path: &Path) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(format!("{} does not exist.", path.display()));
    }
    if !path.is_dir() {
        return Err(format!(
            "{} is a file. Choose the folder that contains the repository.",
            path.display()
        ));
    }
    match Repo::open(path) {
        Ok(repo) => Ok(repo
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| repo.git_dir().to_path_buf())),
        Err(_) => Err(format!(
            "{} is not inside a git repository.",
            path.display()
        )),
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
