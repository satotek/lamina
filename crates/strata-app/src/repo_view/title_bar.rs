//! The title bar: the app mark and wordmark, the repository and branch chips,
//! the commit search, and the remote, stash and settings actions.

use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Styled,
    Window, div, px,
};
use gpui_component::input::Input;
use gpui_component::{Sizable as _, TitleBar};
use gpui_kit_assets::IconName;
use strata_core::HeadInfo;

use super::RepoView;
use super::common::{glyph, icon};
use crate::actions;
use crate::data::Source;
use crate::remote;
use crate::settings::Settings;
use crate::style::{self, Design};
use crate::worktree_panel::MainMode;

impl RepoView {
    /// The title bar: the app mark, the repository and branch, the commit
    /// search, and the remote and window actions on the right.
    ///
    /// The actions never leave the window; they drop to icons at the narrowest
    /// widths, where the names and then the search give way: see
    /// [`TitleBarFit`].
    pub(super) fn header(&self, design: &Design, window: &Window) -> AnyElement {
        let has_back = self.parent_name.is_some();
        let fit = TitleBarFit::new(window.viewport_size().width.as_f32(), has_back);
        let source = self.source();
        let head = source.as_ref().and_then(Source::head);
        let repo = match &self.fake {
            Some(_) => t!("title_bar.synthetic_repo").to_string(),
            None => self.name.clone().unwrap_or_else(|| "strata".to_owned()),
        };
        let branch = match head {
            Some(HeadInfo::Branch { short_name, .. }) => short_name.clone(),
            Some(HeadInfo::Detached { .. }) => t!("title_bar.detached").to_string(),
            // Synthetic history has no `HeadInfo`, but its sidebar names the
            // branch it pretends to be on.
            None => source
                .as_ref()
                .and_then(|source| source.sidebar().head_branch)
                .unwrap_or_else(|| t!("title_bar.loading").to_string()),
        };
        let upstream = match &source {
            Some(Source::Repo(snapshot)) => head
                .and_then(|head| match head {
                    HeadInfo::Branch { short_name, .. } => snapshot.upstreams.get(short_name),
                    HeadInfo::Detached { .. } => None,
                })
                .cloned(),
            _ => None,
        };
        let track = upstream
            .as_ref()
            .and_then(crate::branches::upstream_label)
            .unwrap_or_default();
        let track_gone = upstream.as_ref().is_some_and(|info| info.gone);
        let track_note = upstream.as_ref().and_then(crate::branches::upstream_note);
        // The chip carries the current branch's pull request the same way the
        // sidebar row does, so the window's most prominent branch and the list
        // never disagree.
        let branch_prs = self.pull_requests.branches();
        let branch_pr = branch_prs.get(&branch).cloned();
        let branch_pr_note = branch_prs.note().map(str::to_owned);
        let back_button = self.parent_name.as_deref().map(|parent| {
            let tooltip = t!(
                "title_bar.back_tooltip",
                parent = parent,
                key = back_key_label()
            )
            .to_string();
            crate::ui::content_button("back-to-parent")
                .kind(crate::ui::ButtonKind::Ghost)
                .size(crate::ui::ButtonSize::Small)
                .tooltip(tooltip)
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(actions::NavigateBack), cx);
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .max_w(px(fit.back_width))
                        .text_size(px(design.small()))
                        .text_color(design.muted)
                        .child(icon(IconName::ArrowLeft, 13.0, design.muted))
                        .when(fit.back_label, |btn| {
                            btn.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .child(t!("title_bar.back", parent = parent).to_string()),
                            )
                        }),
                )
        });
        let repository = div()
            .id("repository-button")
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(30.0))
            .px_2p5()
            .rounded(px(7.0))
            .cursor_pointer()
            .hover(|style| style.bg(design.field))
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(actions::OpenRepository), cx);
            })
            .child(icon(IconName::Folder, 15.0, design.text))
            .child(
                div()
                    .max_w(px(fit.repository_width))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(repo),
            )
            .child(icon(IconName::ChevronDown, 12.0, design.muted));

        let branch_chip = fit.branch.then(|| {
            let mut chip = crate::ui::content_button("branch-chip")
                .ghost()
                .cursor_pointer()
                .h(px(28.0))
                .px_2p5()
                .rounded(px(7.0))
                .border_1()
                .border_color(design.border)
                .bg(design.background)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(design.ui))
                        .child(glyph(style::glyph::BRANCH, 14.0, design.text))
                        .child(div().max_w(px(fit.branch_width)).truncate().child(branch))
                        .when(fit.track && track_gone, |content| {
                            content.child(icon(IconName::Unlink, 12.0, design.tag_text))
                        })
                        .when(fit.track && !track.is_empty(), |content| {
                            content.child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.0))
                                    .text_color(design.muted)
                                    .child(track),
                            )
                        })
                        .when_some(branch_pr, |content, pull_request| {
                            content.child(crate::pull_requests::badge(
                                &pull_request,
                                branch_pr_note.as_deref(),
                                design,
                            ))
                        })
                        .child(icon(IconName::ChevronDown, 12.0, design.muted)),
                );
            if let Some(note) = track_note {
                chip = chip.tooltip(note.to_owned());
            }
            // Without a session there is nothing to switch to, so the chip
            // stays the plain label it was.
            if self.session.is_some() {
                crate::branch_switcher::chip(self, chip, design)
            } else {
                chip.into_any_element()
            }
        });

        // The real field lives in the title bar when a repository can search;
        // `--fake` gets the same look without an editable field.
        let search_field: AnyElement = match self.search.input() {
            Some(input) => div()
                .flex_1()
                .min_w_0()
                .child(Input::new(input).appearance(false).bordered(false).small())
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(t!("title_bar.search_commits").to_string())
                .into_any_element(),
        };
        let mode = self.search.mode();
        let show_mode_toggles = fit.search_width >= 260.0 && self.session.is_some();
        let search = div()
            .id("commit-search-field")
            .occlude()
            .flex()
            .items_center()
            .gap(px(6.0))
            .w(px(fit.search_width))
            .min_w(px(MIN_SEARCH_WIDTH))
            .max_w_full()
            .h(px(28.0))
            .px_2p5()
            .rounded(px(7.0))
            .bg(design.field)
            .text_color(design.muted)
            .cursor_text()
            .child(icon(IconName::Search, 14.0, design.muted))
            .child(search_field)
            .when(show_mode_toggles, |search| {
                let entity = self.entity.clone();
                let entity_regex = self.entity.clone();
                search
                    .child(search_mode_button(
                        "title-search-content",
                        IconName::FileSearch,
                        t!("title_bar.search_content_tooltip").to_string(),
                        crate::search::SearchMode::Content,
                        mode == crate::search::SearchMode::Content,
                        entity,
                    ))
                    .child(search_mode_button(
                        "title-search-content-regex",
                        IconName::Regex,
                        t!("title_bar.search_regex_tooltip").to_string(),
                        crate::search::SearchMode::ContentRegex,
                        mode == crate::search::SearchMode::ContentRegex,
                        entity_regex,
                    ))
            })
            .when(fit.key_hint, |search| {
                search.child(kbd(search_key_label(), design))
            });

        let separator = div()
            .flex_none()
            .w(px(1.0))
            .h(px(22.0))
            .mx_1p5()
            .bg(design.border);

        let undo_redo = fit
            .undo_redo
            .then(|| self.undo_redo_buttons(design, window));
        let nav = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .flex_none()
            .occlude()
            .when_some(undo_redo, |nav, buttons| nav.child(buttons))
            .child(self.remote_buttons(
                source.as_ref(),
                &remote::Colors {
                    foreground: design.text,
                    muted_foreground: design.muted,
                    border: design.border,
                    accent: design.accent,
                    accent_foreground: design.on_accent,
                },
                fit.remote_compact,
            ))
            .child(stash_button(design, fit.stash_label))
            .child(separator)
            .child(self.terminal_button(design, window))
            .child(
                crate::ui::icon_button("open-settings", IconName::Settings)
                    .ghost()
                    .w(px(32.0))
                    .h(px(32.0))
                    .text_color(design.muted)
                    .tooltip(t!("title_bar.settings_tooltip").to_string())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(actions::OpenSettings), cx);
                    }),
            );

        // The search field is dropped only when even the barest name row
        // leaves no usable width for it, which is below the app's own minimum
        // window size. A field too narrow to use is worse than losing it, and
        // keeping it would push the actions off screen.
        let search_pane = fit.search.then(|| {
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .justify_center()
                .px_2()
                .child(search)
        });

        TitleBar::new()
            .h(px(style::TITLE_BAR_HEIGHT))
            .bg(design.chrome)
            .border_color(design.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_none()
                    // The app's name reads as a label, not a control: muted
                    // spaced capitals, set off from the repository chip by a
                    // divider so a repository named "strata" is not confused
                    // with it. The wordmark is the first thing the bar drops.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .child(style::app_mark(style::BRAND_MARK))
                            .when(fit.wordmark, |mark| {
                                mark.child(style::wordmark(style::BRAND_WORDMARK, design.text))
                            }),
                    )
                    .child(div().w(px(1.0)).h(px(18.0)).mx_1().bg(design.border))
                    .when_some(back_button, |row, button| row.child(control(button)))
                    .child(control(repository))
                    .when_some(branch_chip, |row, chip| row.child(control(chip))),
            )
            .when_some(search_pane, |bar, pane| bar.child(pane))
            .child(nav)
            .into_any_element()
    }
}

/// Wraps a title bar control so a click on it stays a click.
///
/// The bar under every control is a window drag area, and GPUI on Windows
/// answers the OS hit test with the first window-control area among all the
/// hitboxes under the pointer, not just the topmost one. Without a
/// mouse-blocking layer in between, Windows treats a press on any button here
/// as a press on the caption and the button never sees it.
fn control(element: impl IntoElement) -> impl IntoElement {
    div().min_w_0().occlude().child(element)
}

/// The Fetch / Pull / Push row at full size, with the 2 px gap between buttons.
const REMOTE_ACTIONS_WIDTH: f32 = 3.0 * remote::LABELLED_BUTTON_WIDTH + 4.0;

/// The same row as icon-only buttons, which the narrowest tier shows.
const COMPACT_REMOTE_ACTIONS_WIDTH: f32 = 3.0 * remote::ICON_BUTTON_WIDTH + 4.0;

/// The gap between the actions row's parts.
const ACTION_GAP: f32 = 2.0;

/// The separator's 1 px rule and its 6 px margins.
const SEPARATOR_WIDTH: f32 = 13.0;

/// The terminal and settings buttons.
const TOOL_BUTTON_WIDTH: f32 = 32.0;

/// The undo and redo pair, with the same 2 px gap the network actions use.
const UNDO_REDO_WIDTH: f32 = 2.0 * TOOL_BUTTON_WIDTH + ACTION_GAP;

/// The width of the title bar's actions: the undo/redo pair, the remote row,
/// the stash action (52 px with its label, 32 as an icon), the separator, the
/// terminal and the settings. They never leave the window, so this is what the
/// rest of the bar has to leave room for. The undo pair is the part that gives
/// way at the narrowest widths, where its shortcuts still run it.
const fn actions_width(remote: f32, stash: f32, undo_redo: bool) -> f32 {
    let undo = if undo_redo {
        UNDO_REDO_WIDTH + ACTION_GAP
    } else {
        0.0
    };
    remote
        + ACTION_GAP
        + undo
        + stash
        + ACTION_GAP
        + SEPARATOR_WIDTH
        + ACTION_GAP
        + TOOL_BUTTON_WIDTH
        + ACTION_GAP
        + TOOL_BUTTON_WIDTH
}

/// Below this width the remote buttons lose their labels, since that frees
/// more room for the search field and the back button than the labels are
/// worth.
const REMOTE_ICONS_BELOW: f32 = 760.0;

/// Space the title bar reserves around this row: its own left padding, the
/// platform's window controls (`gpui-component` draws three 34 px buttons at
/// the far right on Linux and Windows), and the search column's side padding.
const TITLE_BAR_CHROME: f32 = 144.0;

/// The narrowest the search field gets. Below this the field cannot show what
/// is typed, so the names give way instead.
const MIN_SEARCH_WIDTH: f32 = 120.0;

/// The widest the search field grows; the design's width for it.
const MAX_SEARCH_WIDTH: f32 = 420.0;

/// The search field keeps the `Ctrl F` hint only while there is room for both
/// the hint and a readable amount of query text beside it.
const KEY_HINT_BELOW: f32 = 1500.0;
const KEY_HINT_MIN_SEARCH_WIDTH: f32 = 240.0;

/// The narrowest the back button gets: its arrow alone, with the parent name
/// left to the tooltip.
const MIN_BACK_WIDTH: f32 = 20.0;

/// The narrowest the name chips get before the search field is given up.
const MIN_REPOSITORY_WIDTH: f32 = 70.0;
const MIN_BRANCH_WIDTH: f32 = 60.0;

/// What the title bar shows at a given window width.
///
/// The actions on the right always stay in the window, down to the 720 px
/// minimum size. Everything between them and the app mark gives way in one
/// order as the window narrows: the undo/redo pair, the search hint, the
/// wordmark, the upstream tracking label, the remote buttons' labels, the
/// repository and branch caps, the back button's label, and finally the branch
/// chip. The search field is the last thing to go: it is the only part with no
/// icon-only form, so it is narrowed to [`MIN_SEARCH_WIDTH`] before it
/// disappears. The name caps are what the row reserves; a longer name
/// truncates inside its cap.
#[derive(Clone, Copy, Debug)]
struct TitleBarFit {
    /// The search column. It is dropped only when even the barest name row
    /// leaves no usable width for it, which is below the app's minimum window
    /// size.
    search: bool,
    /// The "STRATA" wordmark next to the app mark.
    wordmark: bool,
    /// The `Ctrl F` hint inside the search field.
    key_hint: bool,
    /// The branch chip, with the tracking label when `track` is set.
    branch: bool,
    track: bool,
    /// The stash action's label, as opposed to its icon alone.
    stash_label: bool,
    /// Fetch, Pull and Push as icons only, with the labels in their tooltips.
    remote_compact: bool,
    /// The undo/redo pair before the remote actions. It gives way at the
    /// narrowest widths, where the shortcuts still run both actions.
    undo_redo: bool,
    /// Whether the "Back to <parent>" label is shown beside its arrow icon.
    back_label: bool,
    back_width: f32,
    repository_width: f32,
    branch_width: f32,
    search_width: f32,
}

impl TitleBarFit {
    fn new(width: f32, has_back: bool) -> Self {
        let (repository_width, branch_width, branch, track, wordmark, back_label, back_width) =
            Self::tier(width, has_back);
        let mut fit = Self {
            search: false,
            wordmark,
            key_hint: false,
            branch,
            track,
            stash_label: width >= 1300.0,
            remote_compact: width < REMOTE_ICONS_BELOW,
            undo_redo: true,
            back_label,
            back_width,
            repository_width,
            branch_width,
            search_width: 0.0,
        };
        // The search field is the last part to go. Everything cheaper gives
        // way first, so a window with a back button keeps a field it can
        // actually type into instead of losing it whole.
        while fit.available(width) < MIN_SEARCH_WIDTH && fit.shed() {}
        let available = fit.available(width);
        fit.search = available >= MIN_SEARCH_WIDTH;
        fit.search_width = if fit.search {
            available.min(MAX_SEARCH_WIDTH)
        } else {
            0.0
        };
        fit.key_hint = width >= KEY_HINT_BELOW && fit.search_width >= KEY_HINT_MIN_SEARCH_WIDTH;
        fit
    }

    /// The detail tier the window is wide enough for, before the search field
    /// has had its say. Each step down frees roughly the width the step above
    /// it adds.
    fn tier(width: f32, has_back: bool) -> (f32, f32, bool, bool, bool, bool, f32) {
        if !has_back {
            if width >= 1500.0 {
                (220.0, 240.0, true, true, true, false, 0.0)
            } else if width >= 1300.0 {
                (180.0, 200.0, true, true, false, false, 0.0)
            } else if width >= 1100.0 {
                (160.0, 160.0, true, false, false, false, 0.0)
            } else if width >= 900.0 {
                (100.0, 80.0, true, false, false, false, 0.0)
            } else if width >= REMOTE_ICONS_BELOW {
                (100.0, 0.0, false, false, false, false, 0.0)
            } else {
                (70.0, 0.0, false, false, false, false, 0.0)
            }
        } else if width >= 1500.0 {
            (180.0, 200.0, true, true, true, true, 140.0)
        } else if width >= 1300.0 {
            (150.0, 170.0, true, true, false, true, 120.0)
        } else if width >= 1100.0 {
            (130.0, 130.0, true, false, false, true, 100.0)
        } else if width >= 1000.0 {
            (90.0, 60.0, true, false, false, true, 80.0)
        } else if width >= REMOTE_ICONS_BELOW {
            (90.0, 0.0, false, false, false, true, 80.0)
        } else {
            (70.0, 0.0, false, false, false, false, MIN_BACK_WIDTH)
        }
    }

    /// The width the actions row reserves for the details it currently shows.
    fn actions_width(&self) -> f32 {
        let remote = if self.remote_compact {
            COMPACT_REMOTE_ACTIONS_WIDTH
        } else {
            REMOTE_ACTIONS_WIDTH
        };
        actions_width(
            remote,
            if self.stash_label { 64.0 } else { 32.0 },
            self.undo_redo,
        )
    }

    /// What is left for the search field once the name row and the actions
    /// have taken their share of `width`.
    fn available(&self, width: f32) -> f32 {
        width - TITLE_BAR_CHROME - self.actions_width() - self.reserved_names()
    }

    /// The widest the name chips get: the app mark and optional wordmark, the
    /// gaps and the divider, then the chips with their padding, icons and
    /// chevrons, and the tracking label when it is shown.
    ///
    /// This is a reservation, not a measurement: a long repository or branch
    /// name truncates inside its cap instead of widening the row.
    fn reserved_names(&self) -> f32 {
        // App mark, the wordmark and its gap, three 8 px row gaps, the divider
        // with its margins, and the repository chip's own padding and icons.
        let mut width = style::BRAND_MARK
            + if self.wordmark {
                style::wordmark_width(style::BRAND_WORDMARK) + 7.0
            } else {
                0.0
            }
            + 24.0
            + 9.0
            + 59.0
            + self.repository_width;
        if self.back_label || self.back_width > 0.0 {
            // The row gap and the small ghost button's horizontal padding.
            width += 8.0 + 16.0 + self.back_width;
        }
        if self.branch {
            // The row gap, the branch chip's padding, glyph and chevron, then
            // the upstream label and its gap.
            width += 8.0 + 58.0 + self.branch_width;
            if self.track {
                width += 76.0;
            }
        }
        width
    }

    /// Gives up one detail, the least costly first, to make room for the
    /// search field. Returns false once nothing else may give way.
    fn shed(&mut self) -> bool {
        if self.undo_redo {
            // The pair is the newest detail and the one with a complete
            // alternative: both actions keep their shortcuts, so it gives way
            // before the remote labels and the chips that identify the
            // repository.
            self.undo_redo = false;
        } else if self.track {
            self.track = false;
        } else if self.wordmark {
            self.wordmark = false;
        } else if !self.remote_compact {
            // Icons keep Fetch, Pull and Push reachable through their tooltips.
            self.remote_compact = true;
        } else if self.stash_label {
            self.stash_label = false;
        } else if self.repository_width > MIN_REPOSITORY_WIDTH {
            self.repository_width = MIN_REPOSITORY_WIDTH;
        } else if self.back_label {
            self.back_label = false;
            self.back_width = MIN_BACK_WIDTH;
        } else if self.branch && self.branch_width > MIN_BRANCH_WIDTH {
            self.branch_width = MIN_BRANCH_WIDTH;
        } else if self.branch {
            self.branch = false;
        } else {
            return false;
        }
        true
    }
}

impl RepoView {
    /// The working tree root, or `None` for the synthetic benchmark and the
    /// error screen, which have no directory to open.
    pub(super) fn work_dir(&self) -> Option<PathBuf> {
        let path = self.session.as_ref()?.repo().workdir()?.to_path_buf();
        Some(std::fs::canonicalize(&path).unwrap_or(path))
    }

    /// Opens a terminal in the repository's working tree.
    pub(crate) fn open_terminal_here(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.work_dir() else {
            self.report_tool_error(t!("title_bar.open_repository_first").to_string(), cx);
            return;
        };
        match crate::external_tools::open_terminal(&dir, Settings::get(cx)) {
            Ok(()) => cx.notify(),
            Err(error) => self.report_tool_error(error, cx),
        }
    }

    /// Opens a repository-relative `path` in the editor, at `line` when known.
    pub(crate) fn open_file_in_editor(
        &mut self,
        path: &str,
        line: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.work_dir() else {
            self.report_tool_error(t!("title_bar.open_repository_first").to_string(), cx);
            return;
        };
        let file = dir.join(path);
        match crate::external_tools::open_in_editor(&file, line, Settings::get(cx)) {
            Ok(()) => cx.notify(),
            Err(error) => self.report_tool_error(error, cx),
        }
    }

    /// Opens whatever file the current view has selected in the editor. The
    /// palette and its action use this because they carry no file of their own.
    pub(crate) fn open_selection_in_editor(&mut self, cx: &mut Context<Self>) {
        let path = match self.mode {
            MainMode::Worktree => self.worktree.selected_path().map(str::to_owned),
            MainMode::History | MainMode::Reflog => self
                .files
                .as_ref()
                .and_then(|files| self.selected_file.and_then(|index| files.get(index)))
                .map(|file| file.path.clone()),
        };
        match path {
            Some(path) => self.open_file_in_editor(&path, None, cx),
            None => self.report_tool_error(t!("title_bar.select_file_to_open").to_string(), cx),
        }
    }

    fn report_tool_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.status_error = Some(error);
        cx.notify();
    }
    #[cfg(feature = "terminal")]
    pub(super) fn terminal_toggle_tooltip(&self, window: &Window) -> String {
        let os = if cfg!(target_os = "macos") {
            gpui::OperatingSystem::Mac
        } else if cfg!(target_os = "windows") {
            gpui::OperatingSystem::Windows
        } else {
            gpui::OperatingSystem::Linux
        };
        match window
            .bindings_for_action_in(&actions::ToggleTerminal, &self.focus)
            .last()
        {
            Some(binding) => t!(
                "title_bar.toggle_terminal_tooltip",
                shortcut = crate::palette::shortcut(binding, os)
            )
            .to_string(),
            None => t!("action.toggle_terminal").to_string(),
        }
    }

    /// The title bar's terminal button: toggles the embedded terminal panel
    /// when built with `terminal`.
    #[cfg(feature = "terminal")]
    pub(super) fn terminal_button(&self, design: &Design, window: &Window) -> AnyElement {
        let open = self.terminal_open;
        let enabled = self.error.is_none() && self.terminal_working_dir().is_some();
        let entity = self.entity.clone();
        crate::ui::icon_toggle("open-terminal", IconName::SquareTerminal, open)
            .ghost()
            .w(px(32.0))
            .h(px(32.0))
            .text_color(if open { design.text } else { design.muted })
            .tooltip(self.terminal_toggle_tooltip(window))
            .disabled(!enabled)
            .on_click(move |_, window, cx| {
                if let Some(entity) = entity.upgrade() {
                    entity.update(cx, |this, cx| this.toggle_terminal(window, cx));
                }
            })
            .into_any_element()
    }

    /// The title bar's terminal action: opens a terminal in the repository's
    /// working tree when the embedded terminal feature is disabled.
    #[cfg(not(feature = "terminal"))]
    fn terminal_button(&self, design: &Design, _window: &Window) -> AnyElement {
        let enabled = self.work_dir().is_some();
        let entity = self.entity.clone();
        crate::ui::icon_button("open-terminal", IconName::SquareTerminal)
            .ghost()
            .w(px(32.0))
            .h(px(32.0))
            .text_color(design.muted)
            .tooltip(t!("action.open_terminal_here").to_string())
            .disabled(!enabled)
            .on_click(move |_, _, cx| {
                if let Some(entity) = entity.upgrade() {
                    entity.update(cx, |this, cx| this.open_terminal_here(cx));
                }
            })
            .into_any_element()
    }

    /// The title bar's Undo and Redo pair, left of the remote actions.
    ///
    /// The tooltips name the entry each button would move to, so the pair says
    /// more than "undo"; a disabled side carries the reason it cannot run.
    fn undo_redo_buttons(&self, design: &Design, window: &Window) -> AnyElement {
        let (undo_enabled, undo_tooltip) = self.undo_button_state(window);
        let (redo_enabled, redo_tooltip) = self.redo_button_state(window);
        div()
            .flex()
            .items_center()
            .gap(px(ACTION_GAP))
            .flex_none()
            .child(
                crate::ui::icon_button("undo-last-action", IconName::Undo)
                    .ghost()
                    .w(px(TOOL_BUTTON_WIDTH))
                    .h(px(TOOL_BUTTON_WIDTH))
                    .text_color(design.muted)
                    .tooltip(undo_tooltip)
                    .disabled(!undo_enabled)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(actions::UndoLastAction), cx);
                    }),
            )
            .child(
                crate::ui::icon_button("redo-last-action", IconName::Redo)
                    .ghost()
                    .w(px(TOOL_BUTTON_WIDTH))
                    .h(px(TOOL_BUTTON_WIDTH))
                    .text_color(design.muted)
                    .tooltip(redo_tooltip)
                    .disabled(!redo_enabled)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(actions::RedoLastAction), cx);
                    }),
            )
            .into_any_element()
    }
}

/// The title bar's Stash action: the same icon-over-label shape as the remote
/// buttons, but it dispatches an action instead of running a git command here.
/// Without room for the label it keeps the icon, which the tooltip names.
fn stash_button(design: &Design, label: bool) -> AnyElement {
    crate::ui::content_button("stash-button")
        .ghost()
        .w(px(if label { 64.0 } else { 32.0 }))
        .h(px(40.0))
        .tooltip(t!("action.stash_changes").to_string())
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(actions::StashChanges), cx);
        })
        .child(
            // The label size goes on the content: the button sets its own text
            // size, which would override one set on the button itself.
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(2.0))
                .text_size(px(10.0))
                .child(glyph(style::glyph::ARCHIVE, 16.0, design.text))
                .when(label, |content| {
                    content.child(t!("title_bar.stash").to_string())
                }),
        )
        .into_any_element()
}

fn search_mode_button(
    id: &'static str,
    icon: IconName,
    tooltip: impl Into<gpui::SharedString>,
    mode: crate::search::SearchMode,
    active: bool,
    entity: gpui::WeakEntity<RepoView>,
) -> AnyElement {
    crate::ui::icon_toggle(id, icon, active)
        .size(crate::ui::ButtonSize::Tiny)
        .tooltip(tooltip)
        .on_click(move |_event, _window, cx| {
            if let Some(entity) = entity.upgrade() {
                entity.update(cx, |this, cx| this.toggle_search_mode(mode, cx));
            }
        })
        .into_any_element()
}

/// A keyboard hint chip, matching the design's `Ctrl F`.
fn kbd(label: &'static str, design: &Design) -> AnyElement {
    crate::ui::kbd(label, design)
}

#[cfg(not(target_os = "macos"))]
fn search_key_label() -> &'static str {
    "Ctrl F"
}

#[cfg(target_os = "macos")]
fn search_key_label() -> &'static str {
    "⌘F"
}

#[cfg(not(target_os = "macos"))]
fn back_key_label() -> &'static str {
    "Alt+Left"
}

#[cfg(target_os = "macos")]
fn back_key_label() -> &'static str {
    "⌘["
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(fit: &TitleBarFit) -> f32 {
        fit.reserved_names() + fit.actions_width() + TITLE_BAR_CHROME + fit.search_width
    }

    /// Every width from the app's minimum to a maximised window, at 1 px
    /// steps, which covers the widths a hand check runs at.
    fn widths() -> impl Iterator<Item = f32> {
        (style::MIN_WINDOW_SIZE.0 as u32..=1920).map(|width| width as f32)
    }

    #[test]
    fn the_actions_stay_inside_the_window_at_every_width() {
        for width in widths() {
            for has_back in [false, true] {
                let fit = TitleBarFit::new(width, has_back);
                let label = if has_back {
                    "with back button"
                } else {
                    "without"
                };
                assert!(
                    used(&fit) <= width,
                    "{label}, the title bar needs {} px of a {width} px window, which pushes the actions off screen",
                    used(&fit)
                );
                assert!(
                    fit.search,
                    "{label}, the search field must survive at the {width} px app minimum"
                );
                assert!(
                    fit.search_width >= MIN_SEARCH_WIDTH,
                    "{label}, the search field collapses below a usable width at {width} px"
                );
            }
        }
    }

    #[test]
    fn a_back_button_never_costs_the_search_field_in_the_760_to_850_range() {
        // The regression: the tier that keeps the back button's label used to
        // reserve so much that the search was dropped whole, leaving a gap
        // even though a narrower field still fits.
        for width in 760..=850 {
            let fit = TitleBarFit::new(width as f32, true);
            assert!(
                fit.search && fit.search_width >= MIN_SEARCH_WIDTH,
                "the search field vanishes at {width} px with a back button"
            );
            assert!(used(&fit) <= width as f32, "{width} px");
        }
    }

    #[test]
    fn a_long_repository_or_branch_name_cannot_widen_the_bar() {
        // The chips truncate their text inside the cap the fit reserved, so a
        // long name takes the same width as a short one. What has to hold is
        // that the reservation stays within the caps and the row, both with
        // and without the back button.
        for width in widths() {
            for has_back in [false, true] {
                let fit = TitleBarFit::new(width, has_back);
                assert!(
                    (MIN_REPOSITORY_WIDTH..=220.0).contains(&fit.repository_width),
                    "repository cap {} at {width} px",
                    fit.repository_width
                );
                if fit.branch {
                    assert!(
                        (MIN_BRANCH_WIDTH..=240.0).contains(&fit.branch_width),
                        "branch cap {} at {width} px",
                        fit.branch_width
                    );
                }
                assert!(used(&fit) <= width, "{has_back} at {width} px");
            }
        }
    }

    #[test]
    fn details_come_back_as_the_window_grows() {
        assert!(
            !TitleBarFit::new(800.0, false).branch,
            "no branch chip at 800 px"
        );
        assert!(
            TitleBarFit::new(1000.0, false).branch,
            "the branch chip is back at 1000 px"
        );
        assert!(
            !TitleBarFit::new(1000.0, false).stash_label,
            "the stash action is still an icon at 1000 px"
        );
        assert!(TitleBarFit::new(1300.0, false).stash_label);
        assert!(!TitleBarFit::new(1300.0, false).wordmark);
        assert!(TitleBarFit::new(1500.0, false).wordmark);
    }

    #[test]
    fn the_undo_pair_shows_once_the_bar_has_room_for_it() {
        assert!(
            TitleBarFit::new(1360.0, false).undo_redo,
            "a default-sized window shows undo and redo beside the remote actions"
        );
        assert!(
            TitleBarFit::new(1360.0, true).undo_redo,
            "the pair survives the back button at a default size"
        );
        assert!(
            !TitleBarFit::new(style::MIN_WINDOW_SIZE.0, true).undo_redo,
            "at the app minimum the search field comes before the pair"
        );
    }

    #[test]
    fn a_narrow_window_hides_details_instead_of_overflowing() {
        let fit = TitleBarFit::new(800.0, false);
        assert!(!fit.wordmark, "the wordmark goes before the name chips");
        assert!(!fit.branch, "the branch chip goes at the narrowest widths");
        assert!(!fit.stash_label, "the stash action keeps its icon only");
        assert!(!fit.remote_compact, "800 px still has room for the labels");
        assert!(fit.search && fit.search_width >= MIN_SEARCH_WIDTH);
    }

    #[test]
    fn the_remote_buttons_drop_their_labels_in_the_narrowest_tier() {
        assert!(!TitleBarFit::new(REMOTE_ICONS_BELOW, false).remote_compact);
        assert!(TitleBarFit::new(REMOTE_ICONS_BELOW - 1.0, false).remote_compact);
        assert!(TitleBarFit::new(REMOTE_ICONS_BELOW - 1.0, true).remote_compact);

        // Next to the back button's label the actions are the cheapest thing
        // to shrink, so the icons come one tier earlier than the window does.
        let at_tier = TitleBarFit::new(REMOTE_ICONS_BELOW, true);
        assert!(at_tier.remote_compact);
        assert!(at_tier.search && at_tier.search_width >= MIN_SEARCH_WIDTH);

        // The width the icons free is what keeps a usable search field next to
        // the back button at the app's minimum window size.
        let fit = TitleBarFit::new(style::MIN_WINDOW_SIZE.0, true);
        assert!(fit.remote_compact);
        assert!(fit.search && fit.search_width >= MIN_SEARCH_WIDTH);
        assert!(used(&fit) <= style::MIN_WINDOW_SIZE.0);
    }

    #[test]
    fn a_window_below_the_minimum_keeps_the_actions_and_drops_the_search() {
        let fit = TitleBarFit::new(640.0, false);
        assert!(!fit.search, "nothing is left for the field at 640 px");
        assert!(used(&fit) <= 640.0, "the actions stay on screen");
    }

    #[test]
    fn a_wide_window_shows_the_full_bar() {
        let fit = TitleBarFit::new(1600.0, false);
        assert!(fit.wordmark);
        assert!(fit.branch && fit.track);
        assert!(fit.stash_label);
        assert!(fit.key_hint);
        assert_eq!(fit.repository_width, 220.0);
        assert_eq!(fit.branch_width, 240.0);
        assert!(
            fit.search_width > 280.0,
            "a 1600 px window has a wide search field beside the undo pair"
        );
    }

    #[test]
    fn back_button_shows_its_label_at_normal_widths_and_keeps_search_usable() {
        let default_fit = TitleBarFit::new(1360.0, true);
        assert!(default_fit.back_label);
        assert!(default_fit.branch && default_fit.track);
        assert!(default_fit.search && default_fit.search_width >= MIN_SEARCH_WIDTH);

        let narrow_fit = TitleBarFit::new(style::MIN_WINDOW_SIZE.0, true);
        assert!(
            !narrow_fit.back_label,
            "the label drops to icon-only at the app minimum"
        );
        assert!(
            narrow_fit.remote_compact,
            "the remote labels give way before the search does"
        );
        assert!(
            narrow_fit.search && narrow_fit.search_width >= MIN_SEARCH_WIDTH,
            "the compact remote row leaves the search its usable width"
        );
        assert!(used(&narrow_fit) <= style::MIN_WINDOW_SIZE.0);
    }
}
