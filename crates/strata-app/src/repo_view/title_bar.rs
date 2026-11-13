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
    /// The actions never shrink or leave the window, so the names and the
    /// search give way first as the window narrows: see [`TitleBarFit`].
    pub(super) fn header(&self, design: &Design, window: &Window) -> AnyElement {
        let has_back = self.parent_name.is_some();
        let fit = TitleBarFit::new(window.viewport_size().width.as_f32(), has_back);
        let source = self.source();
        let head = source.as_ref().and_then(Source::head);
        let repo = match &self.fake {
            Some(_) => "synthetic".to_owned(),
            None => self.name.clone().unwrap_or_else(|| "strata".to_owned()),
        };
        let branch = match head {
            Some(HeadInfo::Branch { short_name, .. }) => short_name.clone(),
            Some(HeadInfo::Detached { .. }) => "detached".to_owned(),
            // Synthetic history has no `HeadInfo`, but its sidebar names the
            // branch it pretends to be on.
            None => source
                .as_ref()
                .and_then(|source| source.sidebar().head_branch)
                .unwrap_or_else(|| "loading".to_owned()),
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
        let back_button = self.parent_name.as_deref().map(|parent| {
            let tooltip = format!("Back to {parent} ({})", back_key_label());
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
                                    .child(format!("Back to {parent}")),
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
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .h(px(28.0))
                .px_2p5()
                .rounded(px(7.0))
                .border_1()
                .border_color(design.border)
                .bg(design.background)
                .child(glyph(style::glyph::BRANCH, 14.0, design.text))
                .child(div().max_w(px(fit.branch_width)).truncate().child(branch))
                .when(fit.track && !track.is_empty(), |chip| {
                    chip.child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(design.muted)
                            .child(track),
                    )
                })
                .child(icon(IconName::ChevronDown, 12.0, design.muted))
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
                .child("Search commits, authors, hashes")
                .into_any_element(),
        };
        let search = div()
            .id("commit-search-field")
            .flex()
            .items_center()
            .gap(px(8.0))
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
            .when(fit.key_hint, |search| {
                search.child(kbd(search_key_label(), design))
            });

        let separator = div()
            .flex_none()
            .w(px(1.0))
            .h(px(22.0))
            .mx_1p5()
            .bg(design.border);

        let nav = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .flex_none()
            .child(self.remote_buttons(
                source.as_ref(),
                &remote::Colors {
                    foreground: design.text,
                    muted_foreground: design.muted,
                    border: design.border,
                    accent: design.accent,
                    accent_foreground: design.on_accent,
                },
            ))
            .child(stash_button(design, fit.stash_label))
            .child(separator)
            .child(terminal_button(
                self.entity.clone(),
                self.work_dir().is_some(),
                design,
            ))
            .child(
                crate::ui::icon_button("open-settings", IconName::Settings)
                    .ghost()
                    .w(px(32.0))
                    .h(px(32.0))
                    .text_color(design.muted)
                    .tooltip("Settings")
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(actions::OpenSettings), cx);
                    }),
            );

        // Below the app's own minimum window size the search is the one part
        // with nowhere left to go; a field too narrow to use is worse than
        // losing it, and keeping it would push the actions off screen.
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
            .h(px(44.0))
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
                            .child(style::app_mark(16.0))
                            .when(fit.wordmark, |mark| {
                                mark.child(style::wordmark(9.5, design.muted))
                            }),
                    )
                    .child(div().w(px(1.0)).h(px(18.0)).mx_1().bg(design.border))
                    .when_some(back_button, |row, button| row.child(button))
                    .child(repository)
                    .when_some(branch_chip, |row, chip| row.child(chip)),
            )
            .when_some(search_pane, |bar, pane| bar.child(pane))
            .child(nav)
            .into_any_element()
    }
}

/// The title bar's actions at their full size: fetch, pull and push (52 px
/// each with their 2 px gaps), stash, the separator, the terminal and the
/// settings. They never shrink, so this is what the rest of the bar has to
/// leave room for.
const ACTIONS_WIDTH: f32 = 297.0;

/// The same row with the stash action drawn as an icon only.
const COMPACT_ACTIONS_WIDTH: f32 = 277.0;

/// Space the title bar reserves around this row: its own left padding, the
/// platform's window controls (`gpui-component` draws three 34 px buttons at
/// the far right on Linux and Windows), and the search column's side padding.
const TITLE_BAR_CHROME: f32 = 144.0;

/// The narrowest the search field gets. Below this the field cannot show what
/// is typed, so the names give way instead.
const MIN_SEARCH_WIDTH: f32 = 120.0;

/// The widest the search field grows; the design's width for it.
const MAX_SEARCH_WIDTH: f32 = 420.0;

/// What the title bar shows at a given window width.
///
/// The actions on the right always stay in the window, down to the 720 px
/// minimum size, so everything between them and the app mark gives way in
/// steps as the window narrows: the search hint, then the wordmark, the
/// upstream tracking label, and finally the branch chip. The name caps are
/// what the row reserves; a longer name truncates inside its cap.
#[derive(Clone, Copy, Debug)]
struct TitleBarFit {
    /// The search column. It is dropped only below the app's minimum window
    /// size, where keeping it would push the actions off screen.
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
    /// Whether the "Back to <parent>" label is shown beside its arrow icon.
    back_label: bool,
    back_width: f32,
    repository_width: f32,
    branch_width: f32,
    search_width: f32,
}

impl TitleBarFit {
    fn new(width: f32, has_back: bool) -> Self {
        // The caps and the details that only fit on wide windows. Each step
        // down frees roughly the width the step above it adds.
        let (repository_width, branch_width, branch, track, wordmark, back_label, back_width) =
            if !has_back {
                if width >= 1500.0 {
                    (220.0, 240.0, true, true, true, false, 0.0)
                } else if width >= 1300.0 {
                    (180.0, 200.0, true, true, false, false, 0.0)
                } else if width >= 1100.0 {
                    (160.0, 160.0, true, false, false, false, 0.0)
                } else if width >= 900.0 {
                    (100.0, 80.0, true, false, false, false, 0.0)
                } else if width >= 760.0 {
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
            } else if width >= 760.0 {
                (90.0, 0.0, false, false, false, true, 80.0)
            } else {
                (70.0, 0.0, false, false, false, false, 20.0)
            };
        let stash_label = width >= 1300.0;
        let actions = if stash_label {
            ACTIONS_WIDTH
        } else {
            COMPACT_ACTIONS_WIDTH
        };
        let names = Self::names_width(
            wordmark,
            has_back,
            back_width,
            repository_width,
            branch,
            branch_width,
            track,
        );
        let available = width - TITLE_BAR_CHROME - actions - names;
        let search = available >= MIN_SEARCH_WIDTH;
        let search_width = if search {
            available.min(MAX_SEARCH_WIDTH)
        } else {
            0.0
        };
        Self {
            search,
            wordmark,
            key_hint: width >= 1500.0,
            branch,
            track,
            stash_label,
            back_label,
            back_width,
            repository_width,
            branch_width,
            search_width,
        }
    }

    /// The widest the name chips get: the app mark and optional wordmark, the
    /// gaps and the divider, then the chips with their padding, icons and
    /// chevrons, and the tracking label when it is shown.
    fn names_width(
        wordmark: bool,
        has_back: bool,
        back_width: f32,
        repository_width: f32,
        branch: bool,
        branch_width: f32,
        track: bool,
    ) -> f32 {
        // App mark, the wordmark and its gap, three 8 px row gaps, the divider
        // with its margins, and the repository chip's own padding and icons.
        let mut width =
            16.0 + if wordmark { 71.0 } else { 0.0 } + 24.0 + 9.0 + 59.0 + repository_width;
        if has_back {
            // The row gap and the small ghost button's horizontal padding.
            width += 8.0 + 16.0 + back_width;
        }
        if branch {
            // The row gap, the branch chip's padding, glyph and chevron, then
            // the upstream label and its gap.
            width += 8.0 + 58.0 + branch_width;
            if track {
                width += 76.0;
            }
        }
        width
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
            self.report_tool_error("open a repository first".to_owned(), cx);
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
            self.report_tool_error("open a repository first".to_owned(), cx);
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
            MainMode::History => self
                .files
                .as_ref()
                .and_then(|files| self.selected_file.and_then(|index| files.get(index)))
                .map(|file| file.path.clone()),
        };
        match path {
            Some(path) => self.open_file_in_editor(&path, None, cx),
            None => self.report_tool_error("select a file to open in the editor".to_owned(), cx),
        }
    }

    fn report_tool_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.status_error = Some(error);
        cx.notify();
    }
}

/// The title bar's terminal action: opens a terminal in the repository's
/// working tree, using the command from the settings when one is set.
fn terminal_button(
    entity: gpui::WeakEntity<RepoView>,
    enabled: bool,
    design: &Design,
) -> AnyElement {
    crate::ui::icon_button("open-terminal", IconName::SquareTerminal)
        .ghost()
        .w(px(32.0))
        .h(px(32.0))
        .text_color(design.muted)
        .tooltip("Open terminal here")
        .disabled(!enabled)
        .on_click(move |_, _, cx| {
            if let Some(entity) = entity.upgrade() {
                entity.update(cx, |this, cx| this.open_terminal_here(cx));
            }
        })
        .into_any_element()
}

/// The title bar's Stash action: the same icon-over-label shape as the remote
/// buttons, but it dispatches an action instead of running a git command here.
/// Without room for the label it keeps the icon, which the tooltip names.
fn stash_button(design: &Design, label: bool) -> AnyElement {
    crate::ui::content_button("stash-button")
        .ghost()
        .w(px(if label { 52.0 } else { 32.0 }))
        .h(px(40.0))
        .tooltip("Stash changes")
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
                .when(label, |content| content.child("Stash")),
        )
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
        let names = TitleBarFit::names_width(
            fit.wordmark,
            fit.back_width > 0.0,
            fit.back_width,
            fit.repository_width,
            fit.branch,
            fit.branch_width,
            fit.track,
        );
        let actions = if fit.stash_label {
            ACTIONS_WIDTH
        } else {
            COMPACT_ACTIONS_WIDTH
        };
        names + actions + TITLE_BAR_CHROME + fit.search_width
    }

    #[test]
    fn the_actions_stay_inside_the_window_at_every_width() {
        // The app's 720 px minimum up to a maximised window, which includes
        // the 800, 1000, 1100, 1300 and 1600 px widths the title bar is
        // checked at by hand.
        for width in (720..=1920).map(|width| width as f32) {
            let fit = TitleBarFit::new(width, false);
            assert!(
                used(&fit) <= width,
                "the title bar needs {} px of a {width} px window, which pushes the actions off screen",
                used(&fit)
            );
            assert!(
                !fit.search || fit.search_width >= MIN_SEARCH_WIDTH,
                "the search field collapses below a usable width at {width} px"
            );
            assert!(
                fit.search || width < 720.0,
                "the search stays in the bar at the app's minimum window size"
            );

            let fit_with_back = TitleBarFit::new(width, true);
            assert!(
                used(&fit_with_back) <= width,
                "with back button, title bar needs {} px of a {width} px window",
                used(&fit_with_back)
            );
            assert!(
                !fit_with_back.search || fit_with_back.search_width >= MIN_SEARCH_WIDTH,
                "with back button, search field collapses below a usable width at {width} px"
            );
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
    fn a_narrow_window_hides_details_instead_of_overflowing() {
        let fit = TitleBarFit::new(800.0, false);
        assert!(!fit.wordmark, "the wordmark goes before the name chips");
        assert!(!fit.branch, "the branch chip goes at the narrowest widths");
        assert!(!fit.stash_label, "the stash action keeps its icon only");
        assert!(fit.search && fit.search_width >= MIN_SEARCH_WIDTH);
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
            fit.search_width > 300.0,
            "a 1600 px window has room for a wide search field"
        );
    }

    #[test]
    fn back_button_shows_its_label_at_normal_widths_and_keeps_search_usable() {
        let default_fit = TitleBarFit::new(1360.0, true);
        assert!(default_fit.back_label);
        assert!(default_fit.branch && default_fit.track);
        assert!(default_fit.search && default_fit.search_width >= MIN_SEARCH_WIDTH);

        let narrow_fit = TitleBarFit::new(720.0, true);
        assert!(!narrow_fit.back_label, "label drops to icon-only at 720 px");
        assert!(used(&narrow_fit) <= 720.0);
    }
}
