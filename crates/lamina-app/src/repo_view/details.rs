//! The commit details column: summary, author, id and parents, the files
//! summary and the changed-file list.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, FontWeight, InteractiveElement,
    IntoElement, ParentElement, ScrollHandle, Styled, Window, div, px, uniform_list,
};
use gpui_component::Selectable as _;
use gpui_component::menu::PopupMenuItem;
use gpui_component::scroll::Scrollbar;
use gpui_kit_assets::IconName;
use lamina_core::diff::LineKind;
use lamina_core::session::{QueryTicket, SelectionRangeMode};
use lamina_core::{ChangeKind, CommitStat, FileChange, FileStat, HeadInfo, Oid};

use super::RepoView;
use super::commit_selection::{SelectionSummary, summarize};
use super::common::icon;
use crate::data;
use crate::data::Source;
use crate::scrollbar::WithScrollbar as _;
use crate::settings::{self, FileListMode, Settings};
use crate::style::{self, Design};
use crate::ui::file_tree::{self, TreeRow};

/// The commit-wide added/removed counts the details pane is waiting for.
///
/// The session answers with a ticket, so an answer for a commit the user has
/// already left is dropped instead of replacing the totals on screen.
#[derive(Default)]
pub(super) struct CommitStatSlot {
    commit: Option<Oid>,
    ticket: Option<QueryTicket>,
    stat: Option<Arc<CommitStat>>,
}

impl CommitStatSlot {
    /// Points the slot at `commit`, drops the previous totals and returns the
    /// ticket the session should cancel.
    pub(super) fn begin(&mut self, commit: Option<Oid>) -> Option<QueryTicket> {
        self.commit = commit;
        self.stat = None;
        self.ticket.take()
    }

    fn start(&mut self, commit: Oid, ticket: QueryTicket) {
        debug_assert_eq!(self.commit, Some(commit));
        self.ticket = Some(ticket);
    }

    /// True when `ticket` is the stat the pane is still waiting for.
    pub(super) fn accept(&mut self, ticket: QueryTicket, stat: Arc<CommitStat>) -> bool {
        if self.ticket != Some(ticket) {
            return false;
        }
        self.stat = Some(stat);
        true
    }

    fn get(&self) -> Option<&Arc<CommitStat>> {
        self.stat.as_ref()
    }
}

/// How many recent commit messages the details column keeps. Stepping through
/// history back and forth is the common case, so the last few are enough.
const MESSAGE_CACHE: usize = 12;

/// Lines the commit body shows before it folds. The column's width decides how
/// many source lines fit, so the fold is a height rather than a line count.
const BODY_COLLAPSED_LINES: usize = 6;

/// Share of the details pane an expanded body may take. The rest holds the
/// summary, the body's own controls and the changed-file list, which the body
/// would otherwise push out of the pane.
const BODY_EXPANDED_FRACTION: f32 = 0.4;

/// What the expanded body falls back to before the pane has been measured.
const BODY_EXPANDED_DEFAULT_LINES: usize = 8;

/// An expanded body never shows fewer lines than the fold does, so expanding
/// always reveals more text than it hides.
const BODY_EXPANDED_MIN_LINES: usize = BODY_COLLAPSED_LINES;

/// A tall pane still leaves the changed-file list its share instead of the
/// body taking all of it.
const BODY_EXPANDED_MAX_LINES: usize = 18;

/// The selected commit's message body, plus the recent bodies around it.
///
/// The history carries only summaries, so the body is read on demand. An answer
/// that arrives for a commit the user has already left is dropped, which the
/// epoch tells apart.
#[derive(Default)]
pub(super) struct CommitMessageSlot {
    commit: Option<Oid>,
    /// Bumped on every selection change; an answer for an older value is stale.
    epoch: u64,
    /// The body of `commit` once its read answered. `None` while it loads and
    /// for a message whose whole text is the summary.
    body: Option<Arc<str>>,
    /// Bodies read recently, oldest first. `None` marks a message that has no
    /// body, so revisiting it does not read it again.
    cache: VecDeque<(Oid, Option<Arc<str>>)>,
    /// Whether the body on screen is unfolded. Only the current commit
    /// remembers this; the next one starts folded.
    expanded: bool,
    /// Scroll offset of an expanded body. A new selection starts at the top.
    body_scroll: ScrollHandle,
}

impl CommitMessageSlot {
    /// Points the slot at `commit` and returns the read the caller has to run.
    fn begin(&mut self, commit: Option<Oid>) -> Option<(u64, Oid)> {
        self.epoch = self.epoch.wrapping_add(1);
        if self.commit != commit {
            self.expanded = false;
        }
        self.body_scroll = ScrollHandle::default();
        self.commit = commit;
        self.body = None;
        let commit = commit?;
        match self.recall(commit) {
            Some(body) => {
                self.body = body;
                None
            }
            None => Some((self.epoch, commit)),
        }
    }

    fn cache_position(&self, commit: Oid) -> Option<usize> {
        self.cache.iter().position(|(id, _)| *id == commit)
    }

    /// The cached body for `commit`, moved to the back so the least recently
    /// used entry is the one the cache drops.
    fn recall(&mut self, commit: Oid) -> Option<Option<Arc<str>>> {
        let index = self.cache_position(commit)?;
        let entry = self.cache.remove(index)?;
        let body = entry.1.clone();
        self.cache.push_back(entry);
        Some(body)
    }

    fn remember(&mut self, commit: Oid, body: Option<Arc<str>>) {
        if let Some(index) = self.cache_position(commit) {
            self.cache.remove(index);
        }
        self.cache.push_back((commit, body));
        while self.cache.len() > MESSAGE_CACHE {
            self.cache.pop_front();
        }
    }

    /// True when `message` was read for the commit still on screen.
    fn accept(&mut self, epoch: u64, commit: Oid, message: &str) -> bool {
        if self.epoch != epoch || self.commit != Some(commit) {
            return false;
        }
        let body = message_body(message).map(Arc::from);
        self.body = body.clone();
        self.remember(commit, body);
        true
    }

    fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    fn expanded(&self) -> bool {
        self.expanded
    }

    fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
    }
}

impl RepoView {
    /// Requests the selected commit's totals. The details pane keeps showing
    /// the selected file's counts until the answer arrives.
    pub(super) fn load_commit_stat(&mut self, commit: Option<Oid>, cx: &mut Context<Self>) {
        let previous = self.stat.begin(commit);
        if let (Some(session), Some(ticket)) = (self.session.as_ref(), previous) {
            session.cancel_query(ticket);
        }
        if self.smoke {
            return;
        }
        let (Some(commit), Some(session)) = (commit, self.session.as_ref()) else {
            return;
        };
        let ticket = session.request_commit_stat(commit);
        self.stat.start(commit, ticket);
        self.notify_if_active(cx);
    }

    /// Reads the selected commit's full message off the UI thread. The details
    /// column already has the summary; this fills in the body below it.
    pub(super) fn load_commit_message(&mut self, commit: Option<Oid>, cx: &mut Context<Self>) {
        let Some((epoch, commit)) = self.commit_message.begin(commit) else {
            return;
        };
        // The smoke run measures the renderer, not git reads.
        if self.smoke {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let repo = session.reader();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.commit_message(commit) })
                .await;
            // A failed read leaves the summary alone; a banner is not worth a
            // body that was only extra context.
            if let Ok(message) = result {
                this.update(cx, |this, cx| {
                    if this.commit_message.accept(epoch, commit, &message) {
                        this.notify_if_active(cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// The commit's summary, author, id and parents at the top of the details
    /// column. Empty while nothing is selected.
    pub(super) fn commit_details(
        &self,
        commit: Option<&lamina_core::CommitInfo>,
        design: &Design,
    ) -> AnyElement {
        let Some(source) = self.source() else {
            return div().into_any_element();
        };
        let Some(row) = self.selected.and_then(|index| source.row(index)) else {
            return div().into_any_element();
        };
        let now = style::now_epoch();
        let date = style::date_label(row.time, now, design.date_format);
        let initials = style::initials(&row.author);
        let avatar_color = design.avatar_color(&row.email);
        let id = row.id.to_string();
        let copy_id = id.clone();
        let parents = commit
            .map(|commit| commit.parents.clone())
            .unwrap_or_default();

        let mut block = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(design.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(gpui::relative(1.35))
                    .child(row.summary.to_string()),
            );
        if let Some(body) = self.commit_message.body() {
            block = block.child(self.commit_body(body, design));
        }
        block = block
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(design.px(12.0))
                    .text_color(design.muted)
                    .child(crate::ui::avatar(
                        initials.clone(),
                        avatar_color,
                        design.scale(18.0),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(format!("{} · {date}", row.author)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(design.px(6.0))
                    // The details column can be as narrow as its split's
                    // minimum; the id and the parent list truncate there
                    // instead of running into the diff.
                    .min_w_0()
                    .font_family(design.mono.clone())
                    .text_size(px(design.code))
                    .text_color(design.muted)
                    .child(div().flex_none().child(id))
                    .child(
                        div()
                            .id("copy-commit-id")
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .w(design.px(22.0))
                            .h(design.px(22.0))
                            .rounded(design.px(5.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(design.field))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()));
                            })
                            .child(icon(IconName::Copy, design.scale(13.0), design.muted)),
                    )
                    .when(!parents.is_empty(), |row| {
                        let entity = self.entity();
                        let hover = design.field;
                        let mut links = div()
                            .flex()
                            .min_w_0()
                            .items_center()
                            .gap(px(4.0))
                            .child(t!("history.parent").to_string());
                        for (slot, parent) in parents.iter().copied().enumerate() {
                            let click = entity.clone();
                            links = links.child(
                                div()
                                    .id(("commit-parent-link", slot))
                                    .flex_none()
                                    .rounded(px(3.0))
                                    .px(px(2.0))
                                    .cursor_pointer()
                                    .text_color(design.accent)
                                    .hover(move |style| style.bg(hover))
                                    .on_click(move |_, window, cx| {
                                        click.update(cx, |view, cx| {
                                            view.navigate_to_commit(parent, window, cx);
                                        });
                                    })
                                    .child(data::short_oid(parent)),
                            );
                        }
                        row.child(links)
                    }),
            );

        if design.show_commit_signatures
            && let Some(commit_id) = commit.map(|c| c.id).or(self.selection_commit)
            && let Some(sig_state) = self.commit_signature_state(commit_id)
        {
            let entity = self.entity();
            if let Some(details) = crate::ui::signature_details(
                &sig_state,
                self.signatures.details_expanded,
                move |_window, cx| {
                    entity.update(cx, |this, cx| this.toggle_signature_details_expanded(cx));
                },
                design,
            ) {
                block = block.child(details);
            }
        }

        if self.fake.is_some() {
            block = block.child(
                div()
                    .text_size(design.px(11.0))
                    .text_color(design.muted)
                    .child(t!("history.synthetic_diff").to_string()),
            );
        } else if self.session.is_some() {
            if commit.is_some_and(|commit| commit.shallow_boundary) {
                let entity = self.entity();
                block = block.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(design.px(11.0))
                                .text_color(design.muted)
                                .child(t!("history.shallow_note").to_string()),
                        )
                        .child(
                            crate::ui::button(
                                "fetch-full-history",
                                t!("history.fetch_full").to_string(),
                            )
                            .size(crate::ui::ButtonSize::Small)
                            .on_click(move |_, _window, cx| {
                                entity.update(cx, |this, cx| this.fetch_full_history(cx));
                            }),
                        ),
                );
            }
            let entity = self.entity();
            block = block.child(
                div().flex().items_center().gap_2().child(
                    crate::ui::button("browse-files", t!("history.browse_files").to_string())
                        .icon(IconName::Folder)
                        .size(crate::ui::ButtonSize::Small)
                        .on_click(move |_, window, cx| {
                            entity.update(cx, |this, cx| this.open_tree(window, cx));
                        }),
                ),
            );
        }
        block.into_any_element()
    }

    /// The body under the summary: muted wrapping plain text, folded to a few
    /// lines until the user asks for the rest. No markdown is interpreted,
    /// because a commit message is text rather than documentation.
    ///
    /// The unfolded body scrolls inside a cap taken from the pane's height. A
    /// message long enough would otherwise grow the header past the pane and
    /// push the toggle, author, id and parents out of sight, leaving no way to
    /// fold it again with the mouse.
    fn commit_body(&self, body: &str, design: &Design) -> AnyElement {
        let expanded = self.commit_message.expanded();
        let long = body_needs_toggle(body);
        // A fixed line height makes the folded height exactly a line count; the
        // column's width decides how much text that height holds.
        let line_height = design.px(18.0);
        let mut text = div()
            .min_w_0()
            .text_size(design.px(12.0))
            .line_height(line_height)
            .text_color(design.muted)
            .whitespace_normal()
            .child(body.to_owned());
        if long && !expanded {
            text = text
                .max_h(line_height * BODY_COLLAPSED_LINES as f32)
                .overflow_hidden();
        }

        let body_block = if long && expanded {
            let handle = &self.commit_message.body_scroll;
            let height = expanded_body_height(self.details_height.get(), f32::from(line_height));
            let scroller = div()
                .id("commit-body-scroll")
                .w_full()
                .max_h(px(height))
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(handle)
                .child(text);
            // The wrapper is positioned for the overlay scrollbar, the same
            // way the virtualised lists draw theirs.
            div()
                .relative()
                .w_full()
                .min_w_0()
                .min_h_0()
                .child(scroller)
                .child(
                    div().absolute().inset_0().child(
                        Scrollbar::vertical(handle)
                            .id("commit-body-scrollbar")
                            .viewport_from_layout(),
                    ),
                )
                .into_any_element()
        } else {
            text.into_any_element()
        };

        let copy = body.to_owned();
        let mut footer = div().flex().items_center().gap_1().child(
            crate::ui::icon_button("copy-commit-body", IconName::Copy)
                .size(crate::ui::ButtonSize::Tiny)
                .ghost()
                .tooltip(t!("details.copy_body").to_string())
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                }),
        );
        if long {
            let label = if expanded {
                t!("details.body_show_less").to_string()
            } else {
                t!("details.body_show_more").to_string()
            };
            let entity = self.entity();
            footer = footer.child(
                crate::ui::button("commit-body-toggle", label)
                    .size(crate::ui::ButtonSize::Tiny)
                    .ghost()
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.commit_message.toggle_expanded();
                            cx.notify();
                        });
                    }),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(body_block)
            .child(footer)
            .into_any_element()
    }

    /// The compact details block for a multi-selection. It replaces one
    /// commit's details, because the files and totals below belong to a single
    /// commit and would pick the focused row arbitrarily.
    pub(super) fn commit_selection_summary(&self, design: &Design) -> AnyElement {
        let Some(source) = self.source() else {
            return div().into_any_element();
        };
        let entries: Vec<(String, i64)> = self
            .selection
            .rows()
            .filter_map(|index| source.row(index))
            .map(|row| (row.author.to_string(), row.time))
            .collect();
        let Some(summary) = summarize(
            entries
                .iter()
                .map(|(author, time)| (author.as_str(), *time)),
        ) else {
            return div().into_any_element();
        };
        self.selection_summary_block(&summary, design)
    }

    fn selection_summary_block(&self, summary: &SelectionSummary, design: &Design) -> AnyElement {
        let now = style::now_epoch();
        let newest = style::date_label(summary.newest, now, design.date_format);
        let oldest = style::date_label(summary.oldest, now, design.date_format);
        let range = if oldest == newest {
            oldest
        } else {
            format!("{oldest} – {newest}")
        };
        let mut block = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(design.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(gpui::relative(1.35))
                    .child(
                        t!(
                            &crate::i18n::count_key("history.commits_selected", summary.count),
                            count = summary.count
                        )
                        .to_string(),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(design.px(12.0))
                    .text_color(design.muted)
                    .child(summary.authors.join(", ")),
            )
            .child(
                div()
                    .text_size(design.px(11.0))
                    .text_color(design.muted)
                    .child(range),
            );

        // What the file list and the diff below actually compare, and the ways
        // to change or widen that pair.
        if self.selection_diff.plan().is_some() {
            if self.selection_diff.diverged() {
                block = block.child(range_note(
                    t!("details.not_one_line").to_string(),
                    design.muted,
                    design,
                ));
            } else if let Some(skipped) = self.selection_diff.skipped() {
                block = block.child(range_note(
                    t!(
                        &crate::i18n::count_key("details.range_skipped", skipped),
                        count = skipped
                    )
                    .to_string(),
                    design.muted,
                    design,
                ));
            }
            if self.selection_diff.offers_mode_toggle() {
                block = block.child(self.selection_range_toggle());
            }
            let entity = self.entity();
            block = block.child(
                div().flex().items_center().child(
                    crate::ui::button(
                        "selection-open-compare",
                        t!("details.open_in_compare").to_string(),
                    )
                    .icon(IconName::Replace)
                    .size(crate::ui::ButtonSize::Small)
                    .on_click(move |_event, window, cx| {
                        entity.update(cx, |this, cx| {
                            this.open_selection_range_compare(window, cx);
                        });
                    }),
                ),
            );
        }
        if let Some(error) = self.selection_diff.error() {
            block = block.child(range_note(error.to_owned(), design.remove_text, design));
        }

        block
            .child(
                div()
                    .text_size(design.px(11.0))
                    .text_color(design.muted)
                    .child(t!("history.cherry_pick_hint").to_string()),
            )
            .into_any_element()
    }

    /// The two-commit choice between the net range and the two endpoints.
    fn selection_range_toggle(&self) -> AnyElement {
        let current = self.selection_diff.mode();
        let mut row = div().flex().items_center().gap_1();
        for (mode, label, tooltip, id) in [
            (
                SelectionRangeMode::Range,
                t!("details.range_changes").to_string(),
                t!("details.range_changes_tooltip").to_string(),
                "selection-range-mode",
            ),
            (
                SelectionRangeMode::BetweenEndpoints,
                t!("details.between_two").to_string(),
                t!("details.between_two_tooltip").to_string(),
                "selection-endpoints-mode",
            ),
        ] {
            let entity = self.entity();
            row = row.child(
                crate::ui::button(id, label)
                    .size(crate::ui::ButtonSize::Tiny)
                    .ghost()
                    .selected(current == mode)
                    .tooltip(tooltip)
                    .on_click(move |_event, _window, cx| {
                        entity.update(cx, |this, cx| {
                            this.set_selection_range_mode(mode, cx);
                        });
                    }),
            );
        }
        row.into_any_element()
    }

    /// "N files" and the commit's line counts once the stat has arrived; the
    /// selected file's counts stand in until then. The trailing buttons switch
    /// the list below between one row per file and a grouped tree.
    pub(super) fn files_summary(&self, design: &Design, cx: &App) -> AnyElement {
        let files = self.files.as_ref().map_or(0, Vec::len);
        let row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3p5()
            .pt_2()
            .pb_1()
            .text_size(design.px(11.0));
        if files == 0 {
            return row
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(design.muted)
                        .child(t!("history.files_changed.other", count = 0).to_string()),
                )
                .into_any_element();
        }
        let mut left = div().flex().items_center().gap_2().min_w_0().child(
            div()
                .flex_none()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(design.muted)
                .child(if files == 1 {
                    t!("history.files_changed.one", count = files).to_string()
                } else {
                    t!("history.files_changed.other", count = files).to_string()
                }),
        );
        // The range's totals replace the focused commit's; the diff on screen
        // stands in only for a single commit until its stat arrives.
        let totals = if self.range_diff_active() {
            self.selection_diff
                .comparison()
                .map(|comparison| (comparison.added, comparison.removed))
        } else {
            self.stat
                .get()
                .map(|stat| (stat.added, stat.removed))
                .or_else(|| {
                    self.diff_stats()
                        .map(|(added, removed)| (u64::from(added), u64::from(removed)))
                })
        };
        if let Some((added, removed)) = totals {
            left = left.child(
                div()
                    .flex_none()
                    .flex()
                    .gap_1()
                    .child(div().text_color(design.add_text).child(format!("+{added}")))
                    .child(
                        div()
                            .text_color(design.remove_text)
                            .child(format!("−{removed}")),
                    ),
            );
        }
        row.child(left)
            .child(self.file_list_mode_toggle(Settings::get(cx).history.file_list_mode))
            .into_any_element()
    }

    /// The two-state list/tree switch above the changed files.
    fn file_list_mode_toggle(&self, current: FileListMode) -> AnyElement {
        let mut group = div().flex().items_center().gap_1();
        for (mode, icon_name, tooltip, id) in [
            (
                FileListMode::List,
                IconName::List,
                t!("history.file_list.list").to_string(),
                "file-list-mode-list",
            ),
            (
                FileListMode::Tree,
                IconName::FolderTree,
                t!("history.file_list.tree").to_string(),
                "file-list-mode-tree",
            ),
        ] {
            let entity = self.entity();
            group = group.child(
                crate::ui::icon_button(id, icon_name)
                    .size(crate::ui::ButtonSize::Tiny)
                    .ghost()
                    .selected(current == mode)
                    .tooltip(tooltip)
                    .on_click(move |_event, _window, cx| {
                        entity.update(cx, |this, cx| this.set_file_list_mode(mode, cx));
                    }),
            );
        }
        group.into_any_element()
    }

    /// The user's list/tree choice is a preference, not per-commit state.
    fn set_file_list_mode(&mut self, mode: FileListMode, cx: &mut Context<Self>) {
        if Settings::get(cx).history.file_list_mode == mode {
            return;
        }
        settings::update(cx, |settings| settings.history.file_list_mode = mode);
        cx.notify();
    }

    /// Opens or closes one directory in the tree view of the changed files.
    fn toggle_file_tree_dir(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.file_tree_collapsed.remove(&path) {
            self.file_tree_collapsed.insert(path);
        }
        cx.notify();
    }

    /// Added and removed lines in the diff on screen. The session loads one
    /// file at a time, so this is the selected file's contribution.
    fn diff_stats(&self) -> Option<(u32, u32)> {
        let view = self.diff.view()?;
        let mut added = 0;
        let mut removed = 0;
        for hunk in &view.diff.hunks {
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Added => added += 1,
                    LineKind::Removed => removed += 1,
                    LineKind::Context => {}
                }
            }
        }
        Some((added, removed))
    }

    pub(super) fn file_list(&self, design: &Design, cx: &App) -> AnyElement {
        let Some(files) = &self.files else {
            return div().into_any_element();
        };
        let entity = self.entity();
        let selected = self.selected_file;
        // A range shows files from two trees, not from the focused commit, so
        // blame and the "at HEAD" editor wording would name the wrong commit.
        let range_active = self.range_diff_active();
        let blame_commit = if range_active {
            None
        } else {
            self.selection_commit
        };
        // The editor opens the working copy whatever commit is selected, so
        // the entry says when that copy is not what the diff shows.
        let at_head = match (blame_commit, head_oid(self.source().as_ref())) {
            (Some(selected), Some(head)) => selected == head,
            _ => true,
        };
        let work_dir = Arc::new(self.work_dir());
        let lfs = Arc::clone(&self.files_lfs);
        let list_design = design.clone();
        let entries: Arc<Vec<(String, ChangeKind)>> = Arc::new(
            files
                .iter()
                .map(|change: &FileChange| (change.path.clone(), change.kind))
                .collect(),
        );
        // The stat is keyed by path and sorted, while the file list keeps the
        // commit's change order, so align the two once per frame. A range gets
        // its counts from the comparison, which already lists matching files.
        let counts: Vec<Option<FileStat>> = if range_active {
            self.selection_diff
                .comparison()
                .map(|comparison| {
                    comparison
                        .files
                        .iter()
                        .map(|file| Some(file.stat.clone()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            files
                .iter()
                .map(|change| {
                    self.stat.get().and_then(|stat| {
                        stat.files
                            .iter()
                            .find(|file| file.path == change.path)
                            .cloned()
                    })
                })
                .collect()
        };
        let counts = Arc::new(counts);

        let tree_mode = Settings::get(cx).history.file_list_mode == FileListMode::Tree;
        // Both modes draw the same rows; the list mode simply has no directory
        // rows, so the selection stays an index into `files` either way.
        let rows: Arc<Vec<TreeRow>> = Arc::new(if tree_mode {
            let indexed = entries
                .iter()
                .enumerate()
                .map(|(index, (path, _))| (index, path.as_str()))
                .collect::<Vec<_>>();
            file_tree::rows(&indexed, &self.file_tree_collapsed)
        } else {
            entries
                .iter()
                .enumerate()
                .map(|(index, (path, _))| {
                    let (name, _) = style::path_parts(path);
                    TreeRow::File {
                        index,
                        path: path.clone(),
                        name: name.to_owned(),
                        depth: 0,
                    }
                })
                .collect()
        });

        uniform_list("changed-files", rows.len(), move |range, _window, _cx| {
            range
                .map(|row_index| match &rows[row_index] {
                    TreeRow::Directory {
                        path,
                        name,
                        depth,
                        collapsed,
                    } => {
                        let entity = entity.clone();
                        let dir = path.clone();
                        let chevron = if *collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        };
                        crate::ui::list_row(
                            ("changed-file-dir", row_index),
                            crate::ui::RowState::NORMAL,
                            &list_design,
                        )
                        .pl(list_design.px(8.0 + *depth as f32 * 12.0))
                        .cursor_pointer()
                        .on_click(move |_event, _window, cx| {
                            let dir = dir.clone();
                            entity.update(cx, |this, cx| this.toggle_file_tree_dir(dir, cx));
                        })
                        .child(icon(chevron, list_design.scale(12.0), list_design.muted))
                        .child(icon(
                            IconName::Folder,
                            list_design.scale(14.0),
                            list_design.muted,
                        ))
                        .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                        .into_any_element()
                    }
                    TreeRow::File {
                        index,
                        path,
                        name,
                        depth,
                    } => {
                        let index = *index;
                        let entity = entity.clone();
                        let kind = entries[index].1;
                        // The directory trails the name in the flat list only;
                        // in the tree its row already says where the file is.
                        let dir = (!tree_mode).then(|| style::path_parts(path).1).flatten();
                        let is_selected = selected == Some(index);
                        let row = crate::ui::list_row(
                            ("changed-file", index),
                            crate::ui::RowState::NORMAL.selected(is_selected),
                            &list_design,
                        )
                        .pl(list_design.px(8.0 + *depth as f32 * 12.0))
                        .child(crate::ui::change_badge(kind, &list_design))
                        .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                        .when(lfs.contains_key(path), |row| {
                            row.child(crate::ui::lfs_badge(&list_design))
                        })
                        .when_some(dir, |row, dir| {
                            row.child(
                                div()
                                    .max_w(list_design.px(110.0))
                                    .flex_none()
                                    .truncate()
                                    .text_size(list_design.px(11.0))
                                    .text_color(list_design.muted)
                                    .child(dir.to_owned()),
                            )
                        })
                        .when_some(counts.get(index).and_then(Option::as_ref), |row, file| {
                            row.child(file_stat_label(file, &list_design))
                        });
                        let click_entity = entity.clone();
                        let menu_entity = entity;
                        let menu_path = path.clone();
                        let menu_work_dir = work_dir.clone();
                        let trigger = row.cursor_pointer().on_click(move |_event, _window, cx| {
                            click_entity.update(cx, |this, cx| {
                                // The file tree stands in for the diff pane; a
                                // click on a changed file asks for its diff.
                                this.tree.close();
                                this.select_file(index, cx);
                            });
                        });
                        crate::ui::context_menu(
                            ("changed-file-menu", index),
                            &list_design,
                            trigger,
                            move |menu, window, cx| {
                                let variables = menu_entity.read(cx).custom_variables();
                                let entity = menu_entity.clone();
                                let path = menu_path.clone();
                                let present = in_working_tree(menu_work_dir.as_deref(), &path);
                                let mut menu = menu.item(
                                    PopupMenuItem::new(editor_entry_label(at_head, present))
                                        .disabled(!present)
                                        .on_click(
                                            move |_: &ClickEvent,
                                                  _window: &mut Window,
                                                  cx: &mut App| {
                                                let path = path.clone();
                                                entity.update(cx, |this, cx| {
                                                    this.open_file_in_editor(&path, None, cx);
                                                });
                                            },
                                        ),
                                );
                                let entity = menu_entity.clone();
                                let path = menu_path.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(t!("history.show_file_history").to_string())
                                        .on_click(
                                            move |_: &ClickEvent,
                                                  window: &mut Window,
                                                  cx: &mut App| {
                                                let path = path.clone();
                                                entity.update(cx, |this, cx| {
                                                    this.open_file_history(path, window, cx);
                                                });
                                            },
                                        ),
                                );
                                if let Some(commit) = blame_commit {
                                    let entity = menu_entity.clone();
                                    let path = menu_path.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(t!("history.blame_at_commit").to_string())
                                            .on_click(
                                            move |_: &ClickEvent,
                                                  window: &mut Window,
                                                  cx: &mut App| {
                                                let path = path.clone();
                                                entity.update(cx, |this, cx| {
                                                    this.open_blame(commit, path, window, cx);
                                                });
                                            },
                                        ),
                                    );
                                    if let Some(item) = crate::github::file_item(
                                        &menu_entity,
                                        commit,
                                        &menu_path,
                                        None,
                                        cx,
                                    ) {
                                        menu = menu.item(item);
                                    }
                                }
                                crate::commands::submenu(
                                    menu.separator(),
                                    crate::commands::Context::File,
                                    &variables,
                                    window,
                                    cx,
                                )
                            },
                        )
                        .into_any_element()
                    }
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.file_scroll)
        .with_scrollbar(&self.file_scroll)
        .flex_1()
        .min_h_0()
        .into_any_element()
    }
}

/// One explanation line inside the multi-selection summary.
fn range_note(
    text: impl Into<gpui::SharedString>,
    color: gpui::Hsla,
    design: &Design,
) -> AnyElement {
    div()
        .min_w_0()
        .text_size(design.px(11.0))
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

/// The part of `message` below the summary and the blank line that separates
/// it, with trailing whitespace removed. `None` when the whole message is its
/// subject: git keeps everything before the first blank line as the subject, so
/// a message without that line has no body either.
fn message_body(message: &str) -> Option<String> {
    let mut lines = message
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line));
    lines.next()?;
    lines.by_ref().find(|line| line.trim().is_empty())?;
    // Blank lines right after the separator would draw an empty gap under the
    // summary rather than say anything.
    let body = lines
        .skip_while(|line| line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim_end();
    (!body.is_empty()).then(|| body.to_owned())
}

/// Whether `body` is taller than the folded block can show.
///
/// The column's width changes with the split, so this assumes the narrowest
/// column: a body that might wrap past the folded height gets the toggle
/// instead of being cut off without a way to read the rest.
fn body_needs_toggle(body: &str) -> bool {
    /// Characters that fit in the narrowest details column at the body size.
    const NARROW_COLUMN_CHARS: usize = 40;
    let mut lines = 0;
    for line in body.split('\n') {
        lines += line.chars().count().div_ceil(NARROW_COLUMN_CHARS).max(1);
        if lines > BODY_COLLAPSED_LINES {
            return true;
        }
    }
    false
}

/// The height an expanded body may take, in pixels.
///
/// Until the pane is measured it falls back to a fixed line count; afterwards
/// it takes a share of the pane, clamped so expanding always shows more than
/// the fold and a tall pane still leaves the file list room.
fn expanded_body_height(pane_height: Option<f32>, line_height: f32) -> f32 {
    let min = line_height * BODY_EXPANDED_MIN_LINES as f32;
    let max = line_height * BODY_EXPANDED_MAX_LINES as f32;
    let wanted = pane_height.map_or(line_height * BODY_EXPANDED_DEFAULT_LINES as f32, |height| {
        height * BODY_EXPANDED_FRACTION
    });
    wanted.clamp(min, max)
}

/// The commit HEAD points at, when the source knows it.
pub(super) fn head_oid(source: Option<&Source>) -> Option<Oid> {
    match source?.head()? {
        HeadInfo::Branch { target, .. } => *target,
        HeadInfo::Detached { target } => Some(*target),
    }
}

/// Whether the working tree at `root` has `path`. A final symlink counts as
/// present even when it dangles, because git tracks the link itself; a
/// directory does not, because there is no file for the editor to open.
pub(super) fn in_working_tree(root: Option<&Path>, path: &str) -> bool {
    root.is_some_and(|root| {
        std::fs::symlink_metadata(root.join(path))
            .map(|metadata| !metadata.is_dir())
            .unwrap_or(false)
    })
}

/// The editor entry's label. The editor opens the working copy, so a commit
/// other than HEAD says so, and a missing path names the reason it is disabled.
fn editor_entry_label(at_head: bool, in_working_tree: bool) -> String {
    match (at_head, in_working_tree) {
        (true, true) => t!("history.open_in_editor"),
        (true, false) => t!("history.open_in_editor_missing"),
        (false, true) => t!("history.open_working_copy"),
        (false, false) => t!("history.open_working_copy_missing"),
    }
    .to_string()
}

/// The counts one changed file shows, or "bin" when git reports it as binary.
pub(super) fn file_stat_label(file: &FileStat, design: &Design) -> AnyElement {
    if file.binary {
        return div()
            .flex_none()
            .text_size(design.px(10.5))
            .text_color(design.muted)
            .child(t!("history.binary").to_string())
            .into_any_element();
    }
    let mut label = div().flex().flex_none().gap_1().text_size(design.px(10.5));
    if let Some((added, removed)) = line_counts(file) {
        if added > 0 {
            label = label.child(div().text_color(design.add_text).child(format!("+{added}")));
        }
        if removed > 0 {
            label = label.child(
                div()
                    .text_color(design.remove_text)
                    .child(format!("−{removed}")),
            );
        }
    }
    label.into_any_element()
}

/// The line counts worth painting: binary files and mode-only changes have
/// nothing to show.
fn line_counts(file: &FileStat) -> Option<(u64, u64)> {
    if file.binary || (file.added == 0 && file.removed == 0) {
        return None;
    }
    Some((file.added, file.removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> Oid {
        format!("{byte:02x}")
            .repeat(20)
            .parse()
            .expect("hex object id")
    }

    fn stat(added: u64, removed: u64) -> Arc<CommitStat> {
        Arc::new(CommitStat {
            added,
            removed,
            files: Vec::new(),
        })
    }

    #[test]
    fn a_late_stat_for_an_old_ticket_is_ignored() {
        let mut slot = CommitStatSlot::default();
        assert_eq!(slot.begin(Some(oid(1))), None);
        slot.start(oid(1), QueryTicket(1));
        slot.begin(Some(oid(2)));
        slot.start(oid(2), QueryTicket(2));

        assert!(!slot.accept(QueryTicket(1), stat(1, 1)));
        assert!(slot.get().is_none(), "the stale stat must not be stored");

        assert!(slot.accept(QueryTicket(2), stat(4, 2)));
        assert_eq!(
            slot.get().map(|stat| (stat.added, stat.removed)),
            Some((4, 2))
        );
    }

    #[test]
    fn changing_the_selection_drops_the_previous_totals() {
        let mut slot = CommitStatSlot::default();
        slot.begin(Some(oid(1)));
        slot.start(oid(1), QueryTicket(7));
        assert!(slot.accept(QueryTicket(7), stat(3, 1)));

        assert_eq!(
            slot.begin(Some(oid(2))),
            Some(QueryTicket(7)),
            "an answered ticket is still cancelled for the old commit"
        );
        assert!(slot.get().is_none());
        assert!(!slot.accept(QueryTicket(7), stat(9, 9)));
    }

    #[test]
    fn a_commit_away_from_head_offers_the_working_copy_in_the_editor() {
        assert_eq!(editor_entry_label(true, true), "Open in Editor");
        assert_eq!(
            editor_entry_label(false, true),
            "Open Working Copy in Editor",
            "the editor opens the working copy, not the commit on screen"
        );
    }

    #[test]
    fn a_path_missing_from_the_working_tree_names_the_reason() {
        assert_eq!(
            editor_entry_label(true, false),
            "Open in Editor (not in working tree)"
        );
        assert_eq!(
            editor_entry_label(false, false),
            "Open Working Copy in Editor (not in working tree)"
        );
    }

    #[test]
    fn the_working_tree_check_finds_files_and_links_but_not_directories() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let root = dir.path();
        std::fs::write(root.join("present.txt"), "x").expect("file");
        std::fs::create_dir(root.join("submodule")).expect("directory");
        #[cfg(unix)]
        std::os::unix::fs::symlink("missing-target", root.join("link")).expect("symlink");

        assert!(in_working_tree(Some(root), "present.txt"));
        assert!(
            !in_working_tree(Some(root), "submodule"),
            "a directory has no file to open"
        );
        assert!(!in_working_tree(Some(root), "absent.txt"));
        assert!(!in_working_tree(None, "present.txt"));
        #[cfg(unix)]
        assert!(
            in_working_tree(Some(root), "link"),
            "git tracks the link itself, so a dangling one is still present"
        );
    }

    #[test]
    fn binary_and_empty_stats_have_no_line_counts() {
        let file = |added, removed, binary| FileStat {
            path: "assets/logo.png".into(),
            old_path: None,
            added,
            removed,
            binary,
        };
        assert_eq!(line_counts(&file(0, 0, true)), None);
        assert_eq!(line_counts(&file(0, 0, false)), None);
        assert_eq!(line_counts(&file(3, 0, false)), Some((3, 0)));
        assert_eq!(line_counts(&file(0, 2, false)), Some((0, 2)));
        assert_eq!(line_counts(&file(5, 4, false)), Some((5, 4)));
    }

    #[test]
    fn the_body_is_what_follows_the_summary_and_its_blank_line() {
        assert_eq!(message_body("Fix the thing\n"), None);
        assert_eq!(message_body("Fix the thing"), None);
        assert_eq!(
            message_body("Fix the thing\n\nWhy it broke.\nHow it changed.\n"),
            Some("Why it broke.\nHow it changed.".to_owned())
        );
        assert_eq!(
            message_body("Fix the thing\r\n\r\nWhy it broke.\r\n"),
            Some("Why it broke.".to_owned()),
            "a CRLF message keeps its lines without the carriage returns"
        );
        assert_eq!(
            message_body("Fix the thing\n\nWhy it broke.\n\n\n"),
            Some("Why it broke.".to_owned()),
            "trailing blank lines are not part of the body"
        );
        assert_eq!(
            message_body("Fix the thing\nWhy it broke.\n"),
            None,
            "without the blank separator the whole message is the subject"
        );
        assert_eq!(
            message_body("Fix the thing\n\n   \n\t\n"),
            None,
            "a body of blanks has nothing to show"
        );
    }

    #[test]
    fn a_late_message_for_a_commit_the_user_left_is_dropped() {
        let mut slot = CommitMessageSlot::default();
        let (epoch, commit) = slot
            .begin(Some(oid(1)))
            .expect("a read for the first commit");
        assert!(
            slot.begin(Some(oid(2))).is_some(),
            "the second commit needs its own read"
        );

        assert!(!slot.accept(epoch, commit, "one\n\nold body"));
        assert!(slot.body().is_none(), "the stale body must not be shown");

        let (epoch, commit) = slot
            .begin(Some(oid(2)))
            .expect("a read for the second commit");
        assert!(slot.accept(epoch, commit, "two\n\nnew body"));
        assert_eq!(slot.body(), Some("new body"));
    }

    #[test]
    fn a_message_already_read_is_not_read_again() {
        let mut slot = CommitMessageSlot::default();
        let (epoch, commit) = slot
            .begin(Some(oid(1)))
            .expect("a read for the first commit");
        assert!(slot.accept(epoch, commit, "one\n\nbody"));

        assert!(slot.begin(Some(oid(2))).is_some());
        assert!(
            slot.begin(Some(oid(1))).is_none(),
            "the body came from the cache instead of another read"
        );
        assert_eq!(slot.body(), Some("body"));
    }

    #[test]
    fn only_the_current_commit_remembers_its_expanded_body() {
        let mut slot = CommitMessageSlot::default();
        let (epoch, commit) = slot
            .begin(Some(oid(1)))
            .expect("a read for the first commit");
        assert!(slot.accept(epoch, commit, "one\n\nbody"));
        slot.toggle_expanded();
        assert!(slot.expanded());

        slot.begin(Some(oid(2)));
        assert!(!slot.expanded(), "the next commit starts folded");
    }

    #[test]
    fn a_body_past_the_fold_gets_the_toggle() {
        assert!(!body_needs_toggle("one\ntwo\nthree"));
        assert!(!body_needs_toggle("one\ntwo\nthree\nfour\nfive\nsix"));
        assert!(body_needs_toggle("one\ntwo\nthree\nfour\nfive\nsix\nseven"));
        assert!(
            body_needs_toggle(&"x".repeat(40 * 7)),
            "one long paragraph that wraps past the fold gets the toggle too"
        );
    }

    #[test]
    fn an_expanded_body_takes_a_capped_share_of_the_pane() {
        let line = 18.0;
        let height = expanded_body_height(Some(365.0), line);
        assert!(
            (height - 365.0 * BODY_EXPANDED_FRACTION).abs() < 1e-3,
            "the body takes its share of the pane, not a fixed count: {height}"
        );
        assert!(
            height < 365.0 - line * BODY_COLLAPSED_LINES as f32,
            "the cap must leave the summary and controls below it their room"
        );

        assert_eq!(
            expanded_body_height(None, line),
            line * BODY_EXPANDED_DEFAULT_LINES as f32,
            "before the pane is measured the body falls back to a line count"
        );
        assert_eq!(
            expanded_body_height(Some(160.0), line),
            line * BODY_COLLAPSED_LINES as f32,
            "a short pane never expands to less than the fold shows"
        );
        assert_eq!(
            expanded_body_height(Some(4000.0), line),
            line * BODY_EXPANDED_MAX_LINES as f32,
            "a tall pane keeps room for the changed-file list"
        );
    }

    /// The details block, hosted alone in a pane the size reported for the bug,
    /// must not grow past it: the toggle, author, id and parents all sit inside
    /// that block, so a block that fits keeps them on screen and clickable.
    #[gpui::test]
    fn an_expanded_body_stays_inside_a_short_details_pane(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use gpui::{Bounds, Entity, Pixels, QuitMode, Render, VisualTestContext};
        use gpui_component::Root;

        use crate::repo_view::Launch;

        const PANE_HEIGHT: f32 = 365.0;

        cx.update(|cx| {
            cx.set_quit_mode(QuitMode::Explicit);
            gpui_component::init(cx);
            cx.set_global(Settings::default());
        });

        let body = (0..25)
            .map(|line| {
                format!("line {line}: a sentence long enough to wrap in the details column")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body_needs_toggle(&body), "the fixture must need the toggle");

        let window = cx.add_window(|window, cx| RepoView::new(Launch::fake(8), window, cx));
        let view = window.root(cx).expect("the window's root view");
        cx.update(|cx| {
            view.update(cx, |view, _| {
                view.selected = Some(0);
                view.commit_message.body = Some(std::sync::Arc::from(body.as_str()));
                view.details_height.set(Some(PANE_HEIGHT));
            });
        });

        struct DetailsProbe {
            repo: Entity<RepoView>,
            bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
        }

        impl Render for DetailsProbe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let slot = self.bounds.clone();
                let details = {
                    let repo = self.repo.read(cx);
                    let source = repo.source();
                    let commit = source.as_ref().and_then(|source| source.commit(0));
                    repo.commit_details(commit, &Design::default())
                };
                div().w(px(320.0)).h(px(PANE_HEIGHT)).child(
                    div()
                        .on_children_prepainted(move |children: Vec<Bounds<Pixels>>, _, _| {
                            *slot.borrow_mut() = children.first().copied();
                        })
                        .child(details),
                )
            }
        }

        let measured: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::new(RefCell::new(None));
        let probe_slot = measured.clone();
        let probe_repo = view.clone();
        let probe_window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let probe = cx.new(|_| DetailsProbe {
                    repo: probe_repo,
                    bounds: probe_slot,
                });
                cx.new(|cx| Root::new(probe, window, cx))
            })
            .expect("probe window")
        });
        let mut pcx = VisualTestContext::from_window(probe_window.into(), cx);
        pcx.update(|window, cx| window.draw(cx).clear(cx));
        let collapsed = measured
            .borrow()
            .expect("the folded details block was laid out")
            .size
            .height;
        let collapsed = f32::from(collapsed);

        pcx.update(|_, cx| {
            view.update(cx, |view, _| view.commit_message.expanded = true);
        });
        pcx.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = measured
            .borrow()
            .expect("the expanded details block was laid out");
        let height = f32::from(bounds.size.height);

        assert!(
            collapsed > 0.0,
            "the details block must actually render for this test to mean anything"
        );
        assert!(
            height > collapsed,
            "the expanded body must show more than the folded one: {height} px vs {collapsed} px"
        );
        assert!(
            height <= PANE_HEIGHT,
            "an expanded body pushed the details block past its {PANE_HEIGHT} px pane: {height} px"
        );
    }
}
