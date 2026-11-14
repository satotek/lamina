//! Multi-selection over the commit list.
//!
//! The list keeps one focused row — the one the details pane and the single
//! commit actions use — and may hold several rows. [`CommitSelection`] is the
//! pure model: which rows are selected, which row a range extends from and how
//! a multi-pick orders them. The menu rules and the operation's skipped-commit
//! report are plain functions too, so the tests need neither a repository nor a
//! window.
//!
//! Row numbers are commit-list indices, so a higher index is an older commit:
//! history is newest first.

use std::collections::BTreeSet;

/// The rows selected in the commit list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CommitSelection {
    rows: BTreeSet<usize>,
    /// The row a shift-click or shift-arrow extends from.
    anchor: Option<usize>,
    /// The row the details pane and the single commit actions follow.
    focused: Option<usize>,
}

impl CommitSelection {
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    /// More than one row: the state that shows the summary pane and the
    /// multi-pick menu.
    pub(super) fn is_multi(&self) -> bool {
        self.rows.len() > 1
    }

    pub(super) fn contains(&self, index: usize) -> bool {
        self.rows.contains(&index)
    }

    pub(super) fn focused(&self) -> Option<usize> {
        self.focused
    }

    pub(super) fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows.iter().copied()
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Replaces the selection with one row. A plain click and every keyboard
    /// move end here, which is what keeps single selection unchanged.
    pub(super) fn select_one(&mut self, index: Option<usize>) {
        self.rows = index.into_iter().collect();
        self.anchor = index;
        self.focused = index;
    }

    /// A click on `index`; `order` is the list as displayed, so a filtered list
    /// ranges over its visible rows rather than the whole history.
    pub(super) fn click(&mut self, index: usize, shift: bool, secondary: bool, order: &[usize]) {
        if shift {
            self.extend_to(index, order);
        } else if secondary {
            self.toggle(index);
        } else {
            self.select_one(Some(index));
        }
    }

    /// Cmd/Ctrl-click: adds or removes one row without collapsing the rest.
    fn toggle(&mut self, index: usize) {
        if self.rows.remove(&index) {
            if self.rows.is_empty() {
                self.clear();
                return;
            }
            if self.anchor == Some(index) {
                self.anchor = self.rows.iter().next_back().copied();
            }
            // The focused row has to stay selected; otherwise another selected
            // row takes over as the one the details pane shows.
            if self.focused == Some(index) {
                self.focused = self.anchor;
            }
            return;
        }
        self.rows.insert(index);
        self.anchor = Some(index);
        self.focused = Some(index);
    }

    /// Shift-click: selects every displayed row between the anchor and `index`.
    fn extend_to(&mut self, index: usize, order: &[usize]) {
        let Some(target) = order.iter().position(|row| *row == index) else {
            self.select_one(Some(index));
            return;
        };
        let anchor = self
            .anchor
            .filter(|anchor| order.contains(anchor))
            .or(self.focused.filter(|focused| order.contains(focused)))
            .unwrap_or(index);
        let Some(from) = order.iter().position(|row| *row == anchor) else {
            self.select_one(Some(index));
            return;
        };
        self.anchor = Some(anchor);
        let (low, high) = if from <= target {
            (from, target)
        } else {
            (target, from)
        };
        self.rows = order[low..=high].iter().copied().collect();
        self.focused = Some(index);
    }

    /// Shift+Up/Down: moves the focused row by `delta` and extends the range
    /// from the anchor. An empty selection starts on the first row.
    pub(super) fn extend_by(&mut self, delta: isize, order: &[usize]) -> Option<usize> {
        let first = *order.first()?;
        if self.focused.is_none() {
            self.select_one(Some(first));
        }
        let current = self
            .focused
            .and_then(|focused| order.iter().position(|row| *row == focused))
            .unwrap_or(0);
        let target = (current as isize + delta).clamp(0, order.len() as isize - 1) as usize;
        self.extend_to(order[target], order);
        self.focused
    }

    /// The selected rows oldest first, which is the order a multi-pick replays
    /// them in history. Click order never decides it.
    pub(super) fn oldest_first(&self) -> Vec<usize> {
        self.rows.iter().rev().copied().collect()
    }
}

/// The compact details summary of a multi-selection: the count, the distinct
/// authors in row order and the span of author dates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SelectionSummary {
    pub(super) count: usize,
    pub(super) authors: Vec<String>,
    /// Seconds since the Unix epoch.
    pub(super) oldest: i64,
    pub(super) newest: i64,
}

pub(super) fn summarize<'a>(
    entries: impl IntoIterator<Item = (&'a str, i64)>,
) -> Option<SelectionSummary> {
    let mut authors: Vec<String> = Vec::new();
    let mut oldest: Option<i64> = None;
    let mut newest: Option<i64> = None;
    let mut count = 0;
    for (author, time) in entries {
        count += 1;
        if !authors.iter().any(|known| known == author) {
            authors.push(author.to_owned());
        }
        oldest = Some(oldest.map_or(time, |oldest| oldest.min(time)));
        newest = Some(newest.map_or(time, |newest| newest.max(time)));
    }
    if count == 0 {
        return None;
    }
    Some(SelectionSummary {
        count,
        authors,
        oldest: oldest?,
        newest: newest?,
    })
}

/// What the menu rules need to know about one selected commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MultiPickRow {
    /// The commit has more than one parent.
    pub(super) merge: bool,
    /// HEAD already contains the commit, so picking it would be empty.
    pub(super) reachable: bool,
}

/// Why several commits cannot be picked right now, or `None` when they can.
///
/// The state of the repository comes before anything about the commits: a
/// running operation stops every pick, and a detached HEAD has no branch to
/// apply onto. Then the range itself has to be pickable.
pub(super) fn multi_pick_blocked(
    rows: &[MultiPickRow],
    busy: bool,
    pending: bool,
    detached: bool,
) -> Option<String> {
    if rows.is_empty() {
        return Some(t!("commit_selection.no_commits").to_string());
    }
    if busy {
        return Some(t!("branch.reason.busy").to_string());
    }
    if pending {
        return Some(t!("reflog.finish_first").to_string());
    }
    if detached {
        return Some(t!("reflog.head_detached").to_string());
    }
    if rows.iter().any(|row| row.merge) {
        return Some(t!("commit_selection.merge_selected").to_string());
    }
    if rows.iter().any(|row| row.reachable) {
        return Some(t!("commit_selection.on_branch").to_string());
    }
    None
}

/// The ids in the operation's "skipped … (already applied)" progress line.
pub(super) fn parse_skipped(text: &str) -> Option<Vec<String>> {
    let ids = text
        .trim()
        .strip_prefix("skipped ")?
        .strip_suffix(" (already applied)")?;
    let ids: Vec<String> = ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    (!ids.is_empty()).then_some(ids)
}

/// What the status bar says once a multi-pick ends, or `None` when every pick
/// applied and there is nothing to report.
pub(super) fn applied_note(total: usize, skipped: &[String]) -> Option<String> {
    if total == 0 || skipped.is_empty() {
        return None;
    }
    let applied = total.saturating_sub(skipped.len());
    let commits = if applied == 1 { "commit" } else { "commits" };
    Some(format!(
        "{applied} {commits} applied, {} skipped (already applied): {}",
        skipped.len(),
        skipped.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(values: &[usize]) -> Vec<usize> {
        values.to_vec()
    }

    #[test]
    fn a_plain_click_resets_a_multi_selection() {
        let mut selection = CommitSelection::default();
        let order = rows(&[0, 1, 2, 3]);
        selection.click(1, false, true, &order);
        selection.click(2, false, true, &order);
        assert!(selection.is_multi());
        assert_eq!(selection.focused(), Some(2));

        selection.click(3, false, false, &order);
        assert_eq!(
            selection.rows().collect::<Vec<_>>(),
            vec![3],
            "a plain click collapses onto one row"
        );
        assert_eq!(selection.focused(), Some(3));
        assert_eq!(selection.oldest_first(), vec![3]);
    }

    #[test]
    fn cmd_click_toggles_rows_and_keeps_one_focused() {
        let mut selection = CommitSelection::default();
        let order = rows(&[0, 1, 2, 3]);
        selection.click(1, false, false, &order);
        selection.click(3, false, true, &order);
        assert_eq!(selection.rows().collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(selection.focused(), Some(3));

        selection.click(3, false, true, &order);
        assert_eq!(
            selection.focused(),
            Some(1),
            "another selected row takes over"
        );
        assert_eq!(selection.rows().collect::<Vec<_>>(), vec![1]);

        selection.click(1, false, true, &order);
        assert_eq!(selection.len(), 0);
        assert_eq!(selection.focused(), None);
    }

    #[test]
    fn shift_click_selects_the_range_between_the_anchor_and_the_row() {
        let mut selection = CommitSelection::default();
        let order = rows(&[0, 1, 2, 3, 4]);
        selection.click(1, false, false, &order);
        selection.click(3, true, false, &order);
        assert_eq!(selection.rows().collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(selection.focused(), Some(3));

        selection.click(0, true, false, &order);
        assert_eq!(
            selection.rows().collect::<Vec<_>>(),
            vec![0, 1],
            "extending backwards starts again at the anchor"
        );
        assert_eq!(selection.anchor, Some(1));
    }

    #[test]
    fn shift_arrows_extend_from_the_anchor_and_stop_at_the_ends() {
        let mut selection = CommitSelection::default();
        let order = rows(&[0, 1, 2]);
        selection.click(0, false, false, &order);
        assert_eq!(selection.extend_by(1, &order), Some(1));
        assert_eq!(selection.rows().collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(selection.extend_by(-1, &order), Some(0));
        assert_eq!(selection.rows().collect::<Vec<_>>(), vec![0]);
        assert_eq!(
            selection.extend_by(-1, &order),
            Some(0),
            "clamped at the top"
        );
        assert_eq!(
            selection.extend_by(9, &order),
            Some(2),
            "clamped at the bottom"
        );
        assert_eq!(selection.rows().collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn a_range_follows_the_display_order_of_a_filtered_list() {
        let mut selection = CommitSelection::default();
        let order = rows(&[0, 7, 12]);
        selection.click(0, false, false, &order);
        selection.click(12, true, false, &order);
        assert_eq!(selection.rows().collect::<Vec<_>>(), vec![0, 7, 12]);
    }

    #[test]
    fn a_multi_pick_always_applies_oldest_first() {
        let mut selection = CommitSelection::default();
        let order = rows(&[0, 1, 2, 3, 4]);
        // Clicked newest first, then oldest, then a middle row.
        selection.click(1, false, false, &order);
        selection.click(4, false, true, &order);
        selection.click(2, false, true, &order);
        assert_eq!(selection.oldest_first(), vec![4, 2, 1]);
    }

    #[test]
    fn the_summary_counts_distinct_authors_and_the_date_span() {
        let entries = [("Ada", 300), ("Grace", 100), ("Ada", 200)];
        let summary = summarize(entries).expect("a summary");
        assert_eq!(summary.count, 3);
        assert_eq!(summary.authors, vec!["Ada".to_owned(), "Grace".to_owned()]);
        assert_eq!(summary.oldest, 100);
        assert_eq!(summary.newest, 300);
        assert!(summarize(Vec::<(&str, i64)>::new()).is_none());
    }

    fn pick(merge: bool, reachable: bool) -> MultiPickRow {
        MultiPickRow { merge, reachable }
    }

    #[test]
    fn a_pick_is_refused_with_the_reason_that_stops_it() {
        assert_eq!(
            multi_pick_blocked(&[pick(false, false)], true, false, false).as_deref(),
            Some("an operation is running")
        );
        assert_eq!(
            multi_pick_blocked(&[pick(false, false)], false, true, false).as_deref(),
            Some("finish the operation in progress first")
        );
        assert_eq!(
            multi_pick_blocked(&[pick(false, false)], false, false, true).as_deref(),
            Some("HEAD is detached")
        );
        assert_eq!(
            multi_pick_blocked(
                &[pick(false, false), pick(true, false)],
                false,
                false,
                false
            )
            .as_deref(),
            Some("a merge commit is selected")
        );
        assert_eq!(
            multi_pick_blocked(
                &[pick(false, true), pick(false, false)],
                false,
                false,
                false
            )
            .as_deref(),
            Some("already on the current branch")
        );
        assert_eq!(
            multi_pick_blocked(&[pick(false, false)], false, false, false),
            None
        );
        assert_eq!(
            multi_pick_blocked(&[], false, false, false).as_deref(),
            Some("no commits are selected")
        );
    }

    #[test]
    fn the_skipped_commits_are_read_from_the_progress_line() {
        assert_eq!(
            parse_skipped("skipped 1a2b3c4, 5d6e7f8 (already applied)"),
            Some(vec!["1a2b3c4".to_owned(), "5d6e7f8".to_owned()])
        );
        assert_eq!(
            parse_skipped("  skipped 1a2b3c4 (already applied)  "),
            Some(vec!["1a2b3c4".to_owned()])
        );
        assert_eq!(parse_skipped("nothing to see"), None);
        assert_eq!(parse_skipped("skipped (already applied)"), None);
    }

    #[test]
    fn the_status_note_names_what_applied_and_what_was_skipped() {
        assert_eq!(
            applied_note(3, &["1a2b3c4".to_owned()]),
            Some("2 commits applied, 1 skipped (already applied): 1a2b3c4".to_owned())
        );
        assert_eq!(
            applied_note(1, &["1a2b3c4".to_owned()]),
            Some("0 commits applied, 1 skipped (already applied): 1a2b3c4".to_owned())
        );
        assert_eq!(
            applied_note(2, &["1a2b3c4".to_owned(), "5d6e7f8".to_owned()]),
            Some("0 commits applied, 2 skipped (already applied): 1a2b3c4, 5d6e7f8".to_owned())
        );
        assert_eq!(applied_note(3, &[]), None);
        assert_eq!(applied_note(0, &["1a2b3c4".to_owned()]), None);
    }
}
