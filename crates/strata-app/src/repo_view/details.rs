//! The commit details column: summary, author, id and parents, the files
//! summary and the changed-file list.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Styled, Window, div, px, uniform_list,
};
use gpui_component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit_assets::IconName;
use strata_core::diff::LineKind;
use strata_core::session::QueryTicket;
use strata_core::{ChangeKind, CommitStat, FileChange, FileStat, Oid};

use super::RepoView;
use super::common::icon;
use crate::data;
use crate::scrollbar::WithScrollbar as _;
use crate::style::{self, Design};

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
    fn begin(&mut self, commit: Option<Oid>) -> Option<QueryTicket> {
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
        cx.notify();
    }

    /// The commit's summary, author, id and parents at the top of the details
    /// column. Empty while nothing is selected.
    pub(super) fn commit_details(
        &self,
        commit: Option<&strata_core::CommitInfo>,
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
            .map(|commit| {
                commit
                    .parents
                    .iter()
                    .map(|parent| data::short_oid(*parent))
                    .collect::<Vec<_>>()
            })
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
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.0))
                    .text_color(design.muted)
                    .child(crate::ui::avatar(initials.clone(), avatar_color, 18.0))
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
                    .gap(px(6.0))
                    .font_family(design.mono.clone())
                    .text_size(px(design.code))
                    .text_color(design.muted)
                    .child(id)
                    .child(
                        div()
                            .id("copy-commit-id")
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(22.0))
                            .h(px(22.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(design.field))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()));
                            })
                            .child(icon(IconName::Copy, 13.0, design.muted)),
                    )
                    .when(!parents.is_empty(), |row| {
                        row.child(div().child(format!("parent {}", parents.join(" · "))))
                    }),
            );

        if self.fake.is_some() {
            block = block.child(
                div()
                    .text_size(px(11.0))
                    .text_color(design.muted)
                    .child("synthetic diff"),
            );
        } else if self.session.is_some() {
            let entity = self.entity();
            block = block.child(
                div().flex().items_center().gap_2().child(
                    crate::ui::button("browse-files", "Browse files")
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

    /// "N files" and the commit's line counts once the stat has arrived; the
    /// selected file's counts stand in until then.
    pub(super) fn files_summary(&self, design: &Design) -> AnyElement {
        let files = self.files.as_ref().map_or(0, Vec::len);
        let mut row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3p5()
            .pt_2()
            .pb_1()
            .text_size(px(11.0));
        if files == 0 {
            return row
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(design.muted)
                        .child("0 files"),
                )
                .into_any_element();
        }
        row = row.child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(design.muted)
                .child(format!("{files} file{}", if files == 1 { "" } else { "s" })),
        );
        if let Some((added, removed)) = self
            .stat
            .get()
            .map(|stat| (stat.added, stat.removed))
            .or_else(|| {
                self.diff_stats()
                    .map(|(added, removed)| (u64::from(added), u64::from(removed)))
            })
        {
            row = row.child(
                div()
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
        row.into_any_element()
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

    pub(super) fn file_list(&self, design: &Design) -> AnyElement {
        let Some(files) = &self.files else {
            return div().into_any_element();
        };
        let entity = self.entity();
        let selected = self.selected_file;
        let blame_commit = self.selection_commit;
        let list_design = design.clone();
        let count = files.len();
        let entries: Vec<(String, ChangeKind)> = files
            .iter()
            .map(|change: &FileChange| (change.path.clone(), change.kind))
            .collect();
        // The stat is keyed by path and sorted, while the file list keeps the
        // commit's change order, so align the two once per frame.
        let counts: Vec<Option<FileStat>> = files
            .iter()
            .map(|change| {
                self.stat.get().and_then(|stat| {
                    stat.files
                        .iter()
                        .find(|file| file.path == change.path)
                        .cloned()
                })
            })
            .collect();

        uniform_list("changed-files", count, move |range, _window, _cx| {
            range
                .map(|index| {
                    let entity = entity.clone();
                    let (path, kind) = &entries[index];
                    let (name, dir) = style::path_parts(path);
                    let is_selected = selected == Some(index);
                    let row = crate::ui::list_row(
                        ("changed-file", index),
                        crate::ui::RowState::NORMAL.selected(is_selected),
                        &list_design,
                    )
                    .child(crate::ui::change_badge(*kind, &list_design))
                    .child(div().flex_1().min_w_0().truncate().child(name.to_owned()))
                    .when_some(dir, |row, dir| {
                        row.child(
                            div()
                                .max_w(px(110.0))
                                .flex_none()
                                .truncate()
                                .text_size(px(11.0))
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
                    row.cursor_pointer()
                        .on_click(move |_event, _window, cx| {
                            click_entity.update(cx, |this, cx| this.select_file(index, cx));
                        })
                        .context_menu(move |menu, _window, _cx| {
                            let entity = menu_entity.clone();
                            let path = menu_path.clone();
                            let mut menu =
                                menu.item(PopupMenuItem::new("Show file history").on_click(
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let path = path.clone();
                                        entity.update(cx, |this, cx| {
                                            this.open_file_history(path, window, cx);
                                        });
                                    },
                                ));
                            if let Some(commit) = blame_commit {
                                let entity = menu_entity.clone();
                                let path = menu_path.clone();
                                menu =
                                    menu.item(PopupMenuItem::new("Blame at this commit").on_click(
                                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                            let path = path.clone();
                                            entity.update(cx, |this, cx| {
                                                this.open_blame(commit, path, window, cx);
                                            });
                                        },
                                    ));
                            }
                            menu
                        })
                        .into_any_element()
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

/// The counts one changed file shows, or "bin" when git reports it as binary.
fn file_stat_label(file: &FileStat, design: &Design) -> AnyElement {
    if file.binary {
        return div()
            .flex_none()
            .text_size(px(10.5))
            .text_color(design.muted)
            .child("bin")
            .into_any_element();
    }
    let mut label = div().flex().flex_none().gap_1().text_size(px(10.5));
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
}
