//! The title bar: the app mark and wordmark, the repository and branch chips,
//! the commit search, and the remote, stash and settings actions.

use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Styled, div,
    px,
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
    pub(super) fn header(&self, design: &Design) -> AnyElement {
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
                    .max_w(px(220.0))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(repo),
            )
            .child(icon(IconName::ChevronDown, 12.0, design.muted));

        let branch_chip = div()
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
            .child(div().max_w(px(240.0)).truncate().child(branch))
            .when(!track.is_empty(), |chip| {
                chip.child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .text_color(design.muted)
                        .child(track),
                )
            })
            .child(icon(IconName::ChevronDown, 12.0, design.muted));

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
            .w(px(420.0))
            .max_w_full()
            .h(px(28.0))
            .px_2p5()
            .rounded(px(7.0))
            .bg(design.field)
            .text_color(design.muted)
            .cursor_text()
            .child(icon(IconName::Search, 14.0, design.muted))
            .child(search_field)
            .child(kbd(search_key_label(), design));

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
            .child(stash_button(design))
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
                    // with it.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .child(style::app_mark(16.0))
                            .child(style::wordmark(9.5, design.muted)),
                    )
                    .child(div().w(px(1.0)).h(px(18.0)).mx_1().bg(design.border))
                    .child(repository)
                    .child(branch_chip),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .justify_center()
                    .px_4()
                    .child(search),
            )
            .child(nav)
            .into_any_element()
    }
}

impl RepoView {
    /// The working tree root, or `None` for the synthetic benchmark and the
    /// error screen, which have no directory to open.
    fn work_dir(&self) -> Option<PathBuf> {
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
fn stash_button(design: &Design) -> AnyElement {
    crate::ui::content_button("stash-button")
        .ghost()
        .w(px(52.0))
        .h(px(40.0))
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
                .child("Stash"),
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
