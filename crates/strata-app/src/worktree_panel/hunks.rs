//! Diff and hunk rendering for the working-tree view: the header above the
//! diff, the hunk headers with their stage/unstage/discard buttons, and line
//! picking.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Entity, FontWeight, IntoElement, ParentElement, Styled, div, px, uniform_list,
};
use gpui_component::Selectable as _;
use gpui_component::Theme;
use strata_core::ChangeKind;
use strata_core::diff::{Expansion, FileDiff, Gap, LineKind};
use strata_core::highlight::LineHighlights;
use strata_core::patch::LineSelection;
use strata_core::session::{DiffView, Operation, RepoSnapshot};

use super::{Colors, HunkAction, LineAction, PanelRow, RowPosition, Selected, Side, WorktreePanel};
use crate::diff_view::{self, DiffColors, GapHandler, Row as DiffRow, RowOptions};
use crate::repo_view::RepoView;
use crate::scrollbar::WithScrollbar as _;
use crate::style;

/// Flattens a diff, keeping the hunk/line coordinates the patch builder needs.
///
/// The staging pane builds its rows with [`panel_rows_expanded`]; this is the
/// collapsed view the tests compare it against.
#[cfg(test)]
pub(super) fn panel_rows(diff: &FileDiff) -> Vec<PanelRow> {
    panel_rows_expanded(diff, "", "", &[], &Expansion::default())
}

/// Flattens a diff with the context the user revealed.
///
/// Revealed context lines carry no pick position: the staging selection is
/// built from the real hunks, so showing more lines cannot move it.
pub(super) fn panel_rows_expanded(
    diff: &FileDiff,
    old: &str,
    new: &str,
    gaps: &[Gap],
    expansion: &Expansion,
) -> Vec<PanelRow> {
    let mut rows = Vec::with_capacity(diff.hunks.iter().map(|hunk| hunk.lines.len() + 1).sum());
    for (hunk_index, hunk) in diff.hunks.iter().enumerate() {
        push_gap_rows(
            &mut rows,
            old,
            new,
            gaps.get(hunk_index),
            hunk_index,
            expansion,
        );
        rows.push(PanelRow {
            row: DiffRow::HunkHeader {
                old_start: hunk.old_start,
                old_len: hunk.old_len,
                new_start: hunk.new_start,
                new_len: hunk.new_len,
            },
            position: RowPosition::Hunk(hunk_index),
        });
        for (line_index, line) in hunk.lines.iter().enumerate() {
            rows.push(PanelRow {
                row: DiffRow::Line(line.clone()),
                position: RowPosition::Line(
                    (line.kind != LineKind::Context).then_some((hunk_index, line_index)),
                ),
            });
        }
    }
    push_gap_rows(
        &mut rows,
        old,
        new,
        gaps.get(diff.hunks.len()),
        diff.hunks.len(),
        expansion,
    );
    rows
}

fn push_gap_rows(
    rows: &mut Vec<PanelRow>,
    old: &str,
    new: &str,
    gap: Option<&Gap>,
    index: usize,
    expansion: &Expansion,
) {
    for row in diff_view::gap_rows(old, new, gap, index, expansion) {
        rows.push(PanelRow {
            row,
            // Revealed context is not part of the hunk it sits beside.
            position: RowPosition::Line(None),
        });
    }
}

pub(super) fn picked_selection(picked: &BTreeSet<(usize, usize)>) -> LineSelection {
    LineSelection::new(picked.iter().copied())
}

/// Changed lines between two flattened rows, inclusive. Context lines and hunk
/// headers inside the range are skipped.
pub(super) fn picked_in_range(
    rows: &[PanelRow],
    first: usize,
    last: usize,
) -> BTreeSet<(usize, usize)> {
    let (low, high) = if first <= last {
        (first, last)
    } else {
        (last, first)
    };
    rows.iter()
        .enumerate()
        .filter(|(index, _)| (low..=high).contains(index))
        .filter_map(|(_, row)| row.pickable())
        .collect()
}

impl WorktreePanel {
    pub(super) fn diff_pane(
        &self,
        entity: Entity<RepoView>,
        snapshot: &RepoSnapshot,
        theme: &Theme,
        colors: &Colors,
        cx: &mut App,
    ) -> AnyElement {
        let selected = self.selected.clone();
        let (path, side) = selected
            .as_ref()
            .map(|selected| (selected.path.clone(), Some(selected.side)))
            .unwrap_or_else(|| ("no file selected".to_owned(), None));
        let kind = selected.as_ref().and_then(|selected| match selected.side {
            Side::Unstaged => snapshot
                .status
                .unstaged
                .iter()
                .find(|change| change.path == selected.path)
                .map(|change| change.kind),
            Side::Staged => snapshot
                .status
                .staged
                .iter()
                .find(|change| change.path == selected.path)
                .map(|change| change.kind),
            Side::Conflict => Some(ChangeKind::Conflicted),
        });
        let (name, dir) = style::path_parts(&path);

        let mut header = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(40.0))
            .flex_none()
            .px_3p5()
            .bg(colors.background)
            .border_b_1()
            .border_color(colors.border)
            .text_size(px(12.0));
        if let Some(kind) = kind {
            header = header.child(crate::ui::change_badge(kind, &colors.design));
        }
        header = header
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .child(div().font_weight(FontWeight::MEDIUM).child(name.to_owned()))
                    .when_some(dir, |row, dir| {
                        row.child(
                            div()
                                .ml_2()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.5))
                                .text_color(colors.muted_foreground)
                                .child(dir.to_owned()),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(colors.muted_foreground)
                    .child(side.map(Side::label).unwrap_or("")),
            );

        if let (Some(selected), Some(_)) = (&selected, self.diff.view()) {
            header = header.child(self.line_actions(entity.clone(), selected, colors));
        }

        if let Some(view) = self.diff.view()
            && !view.diff.binary
        {
            let active = self.context.is_whole_file();
            let toggle_entity = entity.clone();
            header = header.child(
                crate::ui::button("worktree-toggle-whole-file", "Whole file")
                    .size(crate::ui::ButtonSize::Tiny)
                    .ghost()
                    .selected(active)
                    .tooltip(if active {
                        "Show only the changes"
                    } else {
                        "Show every line of the file"
                    })
                    .on_click(move |_event, _window, cx| {
                        toggle_entity.update(cx, |this, cx| this.toggle_worktree_whole_file(cx));
                    }),
            );
        }

        if let Some(toggle) = self
            .diff
            .view()
            .and_then(|view| crate::image_diff::svg_toggle(&self.image_diff, view, entity.clone()))
        {
            header = header.child(toggle);
        }

        let image = match (&self.diff_error, self.diff.view()) {
            (None, Some(view)) => {
                crate::image_diff::body(&self.image_diff, view, &colors.design, entity.clone(), cx)
            }
            _ => None,
        };
        let body = if let Some(image) = image {
            image
        } else {
            match (&self.diff_error, self.diff.view()) {
                (Some(error), _) => {
                    crate::ui::message(error, colors.danger, &colors.design).into_any_element()
                }
                (None, None) => {
                    crate::ui::message("computing diff…", colors.muted_foreground, &colors.design)
                        .into_any_element()
                }
                (None, Some(view)) if view.lfs.is_some() => {
                    let summary = view.lfs.clone().expect("checked above");
                    let missing = summary.old.as_ref().is_some_and(|side| !side.present)
                        || summary.new.as_ref().is_some_and(|side| !side.present);
                    let mut card = crate::ui::lfs_card(summary, &colors.design);
                    if missing && let Some(selected) = &selected {
                        let path = selected.path.clone();
                        let entity = entity.clone();
                        card = card.action(
                            crate::ui::button("worktree-lfs-download", "Download")
                                .size(crate::ui::ButtonSize::Tiny)
                                .on_click(move |_, window, cx| {
                                    let path = path.clone();
                                    entity.update(cx, |this, cx| {
                                        this.run_operation(
                                            Operation::LfsPull { path },
                                            "Download LFS object",
                                            window,
                                            cx,
                                        );
                                    });
                                }),
                        );
                    }
                    let body = if !self.rows.is_empty() {
                        self.diff_list(entity.clone(), view, theme, colors)
                    } else if view.diff.binary {
                        crate::ui::message("Binary file", colors.muted_foreground, &colors.design)
                            .into_any_element()
                    } else if missing {
                        crate::ui::message(
                            "The object is not downloaded",
                            colors.muted_foreground,
                            &colors.design,
                        )
                        .into_any_element()
                    } else {
                        crate::ui::message(
                            "No textual changes",
                            colors.muted_foreground,
                            &colors.design,
                        )
                        .into_any_element()
                    };
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(card)
                        .child(body)
                        .into_any_element()
                }
                (None, Some(view)) if view.diff.binary => {
                    crate::ui::message("Binary file", colors.muted_foreground, &colors.design)
                        .into_any_element()
                }
                (None, Some(_)) if self.rows.is_empty() => crate::ui::message(
                    "No textual changes",
                    colors.muted_foreground,
                    &colors.design,
                )
                .into_any_element(),
                (None, Some(view)) => {
                    let list = self.diff_list(entity, view, theme, colors);
                    match self.context.notice() {
                        Some(notice) => div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .child(crate::ui::banner(
                                crate::ui::BannerKind::Warning,
                                notice,
                                &colors.design,
                            ))
                            .child(list)
                            .into_any_element(),
                        None => list,
                    }
                }
            }
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn line_actions(
        &self,
        entity: Entity<RepoView>,
        selected: &Selected,
        colors: &Colors,
    ) -> AnyElement {
        let count = self.picked.len();
        if count == 0 {
            return div()
                .flex_none()
                .text_size(px(11.0))
                .text_color(colors.muted_foreground)
                .child("Click lines to pick · Shift-click for a range")
                .into_any_element();
        }

        let mut actions = div().flex().items_center().gap_1();
        let (label, action) = match selected.side {
            Side::Unstaged => (format!("Stage {count} line(s)"), LineAction::Stage),
            Side::Staged => (format!("Unstage {count} line(s)"), LineAction::Unstage),
            Side::Conflict => return div().into_any_element(),
        };
        let main_entity = entity.clone();
        actions = actions.child(
            crate::ui::button("line-action-main", label)
                .kind(crate::ui::ButtonKind::Normal)
                .on_click(move |_, window, cx| {
                    main_entity.update(cx, |this, cx| this.line_operation(action, window, cx));
                }),
        );
        if selected.side == Side::Unstaged {
            let discard_entity = entity.clone();
            actions = actions.child(
                crate::ui::button("line-action-discard", "Discard")
                    .danger()
                    .on_click(move |_, window, cx| {
                        discard_entity.update(cx, |this, cx| {
                            this.line_operation(LineAction::Discard, window, cx)
                        });
                    }),
            );
        }
        actions.into_any_element()
    }

    fn diff_list(
        &self,
        entity: Entity<RepoView>,
        view: &Arc<DiffView>,
        theme: &Theme,
        colors: &Colors,
    ) -> AnyElement {
        let rows = Rc::clone(&self.rows);
        let picked = self.picked.clone();
        let diff_colors = DiffColors::from_theme(theme);
        let mono = colors.mono.clone();
        let view = Arc::clone(view);
        let side = self.selected.as_ref().map(|selected| selected.side);
        let list_colors = colors.clone();

        uniform_list("worktree-diff", rows.len(), move |range, _window, cx| {
            let style = diff_view::DiffStyle::from_settings(cx, &mono).one_line_per_row();
            range
                .map(|index| {
                    let row = &rows[index];
                    let highlights = line_highlights(&row.row, &view);
                    match &row.row {
                        DiffRow::HunkHeader {
                            old_start,
                            old_len,
                            new_start,
                            new_len,
                        } => hunk_header_row(
                            &entity,
                            index,
                            &row.position,
                            side,
                            (*old_start, *old_len, *new_start, *new_len),
                            style.row_height(),
                            &diff_colors,
                            &list_colors,
                        ),
                        DiffRow::Gap { gap, hidden } => {
                            let entity = entity.clone();
                            let on_gap: GapHandler = Rc::new(move |gap, action, _window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.expand_worktree_gap(gap, action, cx)
                                });
                            });
                            diff_view::gap_bar(*gap, *hidden, &diff_colors, &style, Some(&on_gap))
                        }
                        DiffRow::Line(_) | DiffRow::Context(_) => {
                            let pick = row.pickable().map(|key| picked.contains(&key));
                            let entity = entity.clone();
                            div()
                                .id(("worktree-line", index))
                                .w_full()
                                .cursor_pointer()
                                .on_click(move |event, _window, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.click_worktree_line(index, event.modifiers().shift, cx)
                                    });
                                })
                                .child(diff_view::render_row_with(
                                    &row.row,
                                    &diff_colors,
                                    &style,
                                    highlights,
                                    RowOptions { pick, inset: false },
                                ))
                                .into_any_element()
                        }
                    }
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.diff_scroll)
        .with_scrollbar(&self.diff_scroll)
        .flex_1()
        .min_h_0()
        .into_any_element()
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one call per hunk header; a struct would only rename the fields"
)]
fn hunk_header_row(
    entity: &Entity<RepoView>,
    row: usize,
    position: &RowPosition,
    side: Option<Side>,
    (old_start, old_len, new_start, new_len): (u32, u32, u32, u32),
    height: f32,
    diff_colors: &DiffColors,
    colors: &Colors,
) -> AnyElement {
    let RowPosition::Hunk(hunk) = *position else {
        return div().into_any_element();
    };

    let buttons = match side {
        Some(Side::Unstaged) => {
            let stage_entity = entity.clone();
            let discard_entity = entity.clone();
            let discard_hunk = hunk;
            div()
                .flex()
                .items_center()
                .gap_1()
                .pr_1()
                .child(
                    crate::ui::button(("hunk-stage", row), "Stage hunk")
                        .size(crate::ui::ButtonSize::Tiny)
                        .kind(crate::ui::ButtonKind::Normal)
                        .on_click(move |_, window, cx| {
                            stage_entity.update(cx, |this, cx| {
                                this.hunk_operation(hunk, HunkAction::Stage, window, cx)
                            });
                        }),
                )
                .child(
                    crate::ui::button(("hunk-discard", row), "Discard")
                        .size(crate::ui::ButtonSize::Tiny)
                        .danger()
                        .on_click(move |_, window, cx| {
                            discard_entity.update(cx, |this, cx| {
                                this.hunk_operation(discard_hunk, HunkAction::Discard, window, cx)
                            });
                        }),
                )
        }
        Some(Side::Staged) => {
            let unstage_entity = entity.clone();
            div().flex().items_center().gap_1().pr_1().child(
                crate::ui::button(("hunk-unstage", row), "Unstage hunk")
                    .size(crate::ui::ButtonSize::Tiny)
                    .kind(crate::ui::ButtonKind::Normal)
                    .on_click(move |_, window, cx| {
                        unstage_entity.update(cx, |this, cx| {
                            this.hunk_operation(hunk, HunkAction::Unstage, window, cx)
                        });
                    }),
            )
        }
        Some(Side::Conflict) => div(),
        None => div(),
    };

    div()
        .flex()
        .items_center()
        .h(px(height))
        .w_full()
        .bg(colors.design.panel)
        .border_t_1()
        .border_color(colors.border)
        .text_size(px(11.0))
        .text_color(diff_colors.muted)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pl_2()
                .truncate()
                .font_family(colors.mono.clone())
                .child(format!(
                    "@@ -{old_start},{old_len} +{new_start},{new_len} @@"
                )),
        )
        .child(buttons)
        .into_any_element()
}

/// The highlights for the side of the line that is drawn, as in the history
/// diff: context lines exist on both sides, so the new side is preferred.
fn line_highlights<'a>(row: &DiffRow, view: &'a DiffView) -> Option<&'a LineHighlights> {
    let (DiffRow::Line(line) | DiffRow::Context(line)) = row else {
        return None;
    };
    let (highlights, number) = match line.kind {
        LineKind::Removed => (&view.old_highlights, line.old_lineno),
        LineKind::Added => (&view.new_highlights, line.new_lineno),
        LineKind::Context => (&view.new_highlights, line.new_lineno.or(line.old_lineno)),
    };
    highlights.get(number?.checked_sub(1)? as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::diff::{DiffLine, Hunk};

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_lineno: old,
            new_lineno: new,
            content: text.to_owned(),
            no_newline_at_eof: false,
            emphasis: Vec::new(),
        }
    }

    fn sample_diff() -> FileDiff {
        FileDiff {
            hunks: vec![
                Hunk {
                    old_start: 1,
                    old_len: 2,
                    new_start: 1,
                    new_len: 2,
                    lines: vec![
                        line(LineKind::Context, Some(1), Some(1), "unchanged"),
                        line(LineKind::Removed, Some(2), None, "old"),
                        line(LineKind::Added, None, Some(2), "new"),
                    ],
                },
                Hunk {
                    old_start: 10,
                    old_len: 1,
                    new_start: 10,
                    new_len: 2,
                    lines: vec![
                        line(LineKind::Added, None, Some(10), "added"),
                        line(LineKind::Context, Some(10), Some(11), "tail"),
                    ],
                },
            ],
            binary: false,
        }
    }

    #[test]
    fn rows_map_back_to_hunk_and_line_indices() {
        let rows = panel_rows(&sample_diff());
        assert_eq!(rows.len(), 7, "two headers plus five lines");
        assert!(matches!(rows[0].position, RowPosition::Hunk(0)));
        assert_eq!(rows[1].pickable(), None, "context lines cannot be picked");
        assert_eq!(rows[2].pickable(), Some((0, 1)));
        assert_eq!(rows[3].pickable(), Some((0, 2)));
        assert!(matches!(rows[4].position, RowPosition::Hunk(1)));
        assert_eq!(rows[5].pickable(), Some((1, 0)));
        assert_eq!(rows[6].pickable(), None);
    }

    #[test]
    fn a_range_picks_only_changed_lines_and_handles_both_directions() {
        let rows = panel_rows(&sample_diff());

        let forward = picked_in_range(&rows, 0, 6);
        assert_eq!(forward, BTreeSet::from([(0, 1), (0, 2), (1, 0)]));

        let backward = picked_in_range(&rows, 6, 0);
        assert_eq!(backward, forward, "dragging up picks the same lines");

        let inside_one_hunk = picked_in_range(&rows, 4, 6);
        assert_eq!(inside_one_hunk, BTreeSet::from([(1, 0)]));
        assert!(
            picked_in_range(&rows, 0, 1).is_empty(),
            "header and context only"
        );
    }

    #[test]
    fn picked_lines_become_the_patch_selection() {
        let selection = picked_selection(&BTreeSet::from([(1, 2), (0, 4)]));
        let mut lines: Vec<(usize, usize)> = selection.iter().collect();
        lines.sort_unstable();
        assert_eq!(lines, [(0, 4), (1, 2)]);
        assert!(!selection.is_empty());
    }

    /// Revealed context is drawn beside the real hunks, so it must not become
    /// part of a pick: line and hunk staging keep building the same patch.
    #[test]
    fn expanded_context_keeps_staging_picks_and_patches_unchanged() {
        use strata_core::diff::{DiffOptions, diff_text, gaps, line_count};
        use strata_core::patch::{PatchFile, PatchMode, build_patch};

        let old: String = (1..=60).map(|line| format!("line {line}\n")).collect();
        let new = old
            .replace("line 10\n", "TEN\n")
            .replace("line 50\n", "FIFTY\n");
        let diff = diff_text(old.as_bytes(), new.as_bytes(), &DiffOptions::default());
        let gaps = gaps(&diff, line_count(&old), line_count(&new));

        let collapsed = panel_rows(&diff);
        let mut expansion = Expansion::default();
        expansion.set_whole_file(true);
        let expanded = panel_rows_expanded(&diff, &old, &new, &gaps, &expansion);
        assert!(
            expanded.len() > collapsed.len(),
            "the whole-file view must add the hidden lines"
        );
        assert!(
            !expanded
                .iter()
                .any(|row| matches!(row.row, DiffRow::Gap { .. })),
            "nothing is left hidden in whole-file mode"
        );

        // A range that spans everything picks the changed lines and only them.
        let all = picked_in_range(&collapsed, 0, collapsed.len() - 1);
        let expanded_all = picked_in_range(&expanded, 0, expanded.len() - 1);
        assert!(!all.is_empty());
        assert_eq!(expanded_all, all);

        let patch = |picks: &BTreeSet<(usize, usize)>| {
            build_patch(
                &PatchFile::new("src/lib.rs", &diff),
                &picked_selection(picks),
                PatchMode::Stage,
            )
        };
        assert!(patch(&all).is_some());
        assert_eq!(patch(&expanded_all), patch(&all));

        // A range wholly inside revealed context picks nothing, and a range
        // over one hunk header is that hunk's selection.
        let bar_rows = panel_rows_expanded(&diff, &old, &new, &gaps, &Expansion::default());
        let bar = bar_rows
            .iter()
            .position(|row| matches!(row.row, DiffRow::Gap { .. }))
            .expect("a collapsed diff has hidden-lines bars");
        assert!(
            picked_in_range(&bar_rows, bar.saturating_sub(1), bar + 1).is_empty(),
            "revealed lines and the bar are not pickable"
        );

        let header = expanded
            .iter()
            .position(|row| matches!(row.position, RowPosition::Hunk(1)))
            .expect("the second hunk still has its header");
        let next = expanded[header + 1..]
            .iter()
            .position(|row| matches!(row.position, RowPosition::Hunk(_)))
            .map_or(expanded.len(), |offset| header + 1 + offset);
        let hunk_picks = picked_in_range(&expanded, header, next - 1);
        let expected = LineSelection::all_of_hunk(&diff, 1);
        let expected_picks: BTreeSet<(usize, usize)> = expected.iter().collect();
        assert_eq!(picked_selection(&hunk_picks), expected);
        assert_eq!(patch(&hunk_picks), patch(&expected_picks));
    }
}
