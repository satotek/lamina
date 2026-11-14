//! The resolution model behind the conflict merge editor.
//!
//! The editor keeps the merged result as an ordered list of slots: one per
//! merged segment of the three-way merge, each holding the lines the result
//! currently shows there. A conflict slot also keeps the three stage texts, so
//! a per-conflict choice replaces just that stretch without recomputing the
//! merge. Manual typing is applied as a line-level edit to the same slots,
//! which is what lets the counter and "all resolved" stay correct after the
//! user edits the result by hand.
//!
//! Nothing here touches GPUI or a repository, so the app's tests can drive
//! every choice and edit without a window.

use strata_core::encoding::TextEncoding;
use strata_core::merge::{LineEnding, MergeAnalysis, Segment, SegmentKind};

/// One way to resolve the selected conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    Ours,
    Theirs,
    OursThenTheirs,
    TheirsThenOurs,
    Base,
}

impl Choice {
    /// The order the conflict bar offers them in.
    pub(crate) const ALL: [Choice; 5] = [
        Choice::Ours,
        Choice::Theirs,
        Choice::OursThenTheirs,
        Choice::TheirsThenOurs,
        Choice::Base,
    ];

    pub(crate) fn label(self) -> String {
        match self {
            Choice::Ours => t!("merge.choice.ours"),
            Choice::Theirs => t!("merge.choice.theirs"),
            Choice::OursThenTheirs => t!("merge.choice.ours_then_theirs"),
            Choice::TheirsThenOurs => t!("merge.choice.theirs_then_ours"),
            Choice::Base => t!("merge.choice.base"),
        }
        .to_string()
    }

    /// The key that applies this choice to the selected conflict.
    pub(crate) fn shortcut(self) -> &'static str {
        match self {
            Choice::Ours => "alt-1",
            Choice::Theirs => "alt-2",
            Choice::OursThenTheirs => "alt-3",
            Choice::TheirsThenOurs => "alt-4",
            Choice::Base => "alt-5",
        }
    }
}

/// What the user did with one conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Resolution {
    /// The result still holds the marker block.
    #[default]
    Unresolved,
    Ours,
    Theirs,
    OursThenTheirs,
    TheirsThenOurs,
    Base,
    /// The user typed inside the conflict, so no choice describes it.
    Manual,
}

impl Resolution {
    pub(crate) fn is_unresolved(self) -> bool {
        self == Resolution::Unresolved
    }
}

/// One visual row of the side panes: the line of each side at this position.
///
/// A side that has fewer lines than the other in a segment leaves a blank row,
/// so both panes stay aligned without any shared scroll state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MergeRow {
    /// 1-based line number in ours, for the highlight spans.
    pub ours: Option<u32>,
    /// 1-based line number in theirs.
    pub theirs: Option<u32>,
    pub kind: SegmentKind,
    /// Which conflict this row belongs to, for the counter and the selection.
    pub conflict: Option<usize>,
}

/// One stretch of the result and the stage texts a conflict can be resolved
/// from.
#[derive(Debug, Clone)]
struct Slot {
    /// The lines the result currently shows for this stretch.
    lines: Vec<String>,
    /// The marker block an unresolved conflict starts with, for undo.
    markers: Vec<String>,
    ours: Vec<String>,
    theirs: Vec<String>,
    base: Vec<String>,
    /// Which conflict this slot is; `None` for stretches that are not one.
    conflict: Option<usize>,
    resolution: Resolution,
}

/// The editable result plus what the conflict controls need to act on it.
pub(crate) struct MergeModel {
    slots: Vec<Slot>,
    /// Index of the selected conflict, not a slot index.
    selected: usize,
    conflicts: usize,
    line_ending: LineEnding,
    /// The analysis's encoding; the result is written back in it.
    encoding: TextEncoding,
    trailing_newline: bool,
    ours_trailing: bool,
    theirs_trailing: bool,
    base_trailing: bool,
}

impl MergeModel {
    /// Builds the model an analysis describes. An analysis with no textual
    /// segments yields a model with an empty result; callers only offer the
    /// editor when [`MergeAnalysis::text`] is set.
    pub(crate) fn new(analysis: &MergeAnalysis) -> Self {
        let mut slots = Vec::with_capacity(analysis.segments.len());
        let mut conflicts = 0;
        for segment in &analysis.segments {
            let ours = analysis.ours.slice(&segment.ours).to_vec();
            let theirs = analysis.theirs.slice(&segment.theirs).to_vec();
            let base = analysis.base.slice(&segment.base).to_vec();
            let (conflict, markers, lines) = if segment.kind == SegmentKind::Conflict {
                let markers = analysis.marker_lines(segment);
                let index = conflicts;
                conflicts += 1;
                (Some(index), markers.clone(), markers)
            } else {
                let lines = match segment.kind {
                    SegmentKind::Theirs => theirs.clone(),
                    // Both identical changes take ours as well; the text is the
                    // same on either side.
                    SegmentKind::Unchanged | SegmentKind::Ours => ours.clone(),
                    SegmentKind::Conflict => unreachable!("handled above"),
                };
                (None, Vec::new(), lines)
            };
            slots.push(Slot {
                lines,
                markers,
                ours,
                theirs,
                base,
                conflict,
                resolution: Resolution::Unresolved,
            });
        }

        let trailing_newline = match analysis.segments.last() {
            Some(Segment {
                kind: SegmentKind::Theirs,
                ..
            }) => analysis.theirs.trailing_newline,
            Some(Segment {
                kind: SegmentKind::Conflict,
                ..
            }) => true,
            _ => analysis.ours.trailing_newline,
        };

        Self {
            slots,
            selected: 0,
            conflicts,
            line_ending: analysis.line_ending(),
            encoding: analysis.encoding,
            trailing_newline,
            ours_trailing: analysis.ours.trailing_newline,
            theirs_trailing: analysis.theirs.trailing_newline,
            base_trailing: analysis.base.trailing_newline,
        }
    }

    /// The visual rows of the two side panes, and the first row of each
    /// conflict, for scrolling to it.
    pub(crate) fn rows(analysis: &MergeAnalysis) -> (Vec<MergeRow>, Vec<usize>) {
        let mut rows = Vec::new();
        let mut conflict_rows = Vec::new();
        let mut conflict = 0;
        for segment in &analysis.segments {
            let is_conflict = segment.kind == SegmentKind::Conflict;
            let ours = analysis.ours.slice(&segment.ours);
            let theirs = analysis.theirs.slice(&segment.theirs);
            if is_conflict {
                conflict_rows.push(rows.len());
            }
            let count = ours.len().max(theirs.len());
            for index in 0..count {
                rows.push(MergeRow {
                    ours: (index < ours.len())
                        .then(|| segment.ours.start as u32 + index as u32 + 1),
                    theirs: (index < theirs.len())
                        .then(|| segment.theirs.start as u32 + index as u32 + 1),
                    kind: segment.kind,
                    conflict: is_conflict.then_some(conflict),
                });
            }
            if is_conflict {
                conflict += 1;
            }
        }
        (rows, conflict_rows)
    }

    pub(crate) fn conflict_count(&self) -> usize {
        self.conflicts
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    /// Selects conflict `conflict`; out-of-range indices are ignored.
    pub(crate) fn select(&mut self, conflict: usize) -> bool {
        if conflict >= self.conflicts || conflict == self.selected {
            return false;
        }
        self.selected = conflict;
        true
    }

    /// "Conflict 2 of 5", or `None` when the merge has no conflicts.
    pub(crate) fn counter(&self) -> Option<String> {
        (self.conflicts > 0).then(|| {
            t!(
                "merge.counter",
                index = self.selected + 1,
                count = self.conflicts
            )
            .to_string()
        })
    }

    /// Moves to the next conflict, wrapping around.
    pub(crate) fn next_conflict(&mut self) -> bool {
        if self.conflicts == 0 {
            return false;
        }
        self.selected = (self.selected + 1) % self.conflicts;
        true
    }

    /// Moves to the previous conflict, wrapping around.
    pub(crate) fn previous_conflict(&mut self) -> bool {
        if self.conflicts == 0 {
            return false;
        }
        self.selected = (self.selected + self.conflicts - 1) % self.conflicts;
        true
    }

    pub(crate) fn selected_resolution(&self) -> Option<Resolution> {
        self.conflict_slot(self.selected)
            .map(|index| self.slots[index].resolution)
    }

    /// The number of conflicts that still hold their markers.
    pub(crate) fn unresolved_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.resolution.is_unresolved() && slot.conflict.is_some())
            .count()
    }

    pub(crate) fn all_resolved(&self) -> bool {
        self.unresolved_count() == 0
    }

    /// Whether the result text still carries git's conflict markers.
    ///
    /// Like git's own scan this also matches a line of `=======`, so a rule of
    /// equals signs in the text asks for confirmation; that errs on the safe
    /// side of staging a half-merged file.
    pub(crate) fn has_markers(&self) -> bool {
        self.slots.iter().flat_map(|slot| &slot.lines).any(|line| {
            line.starts_with("<<<<<<<")
                || line.starts_with("|||||||")
                || line.starts_with("=======")
                || line.starts_with(">>>>>>>")
        })
    }

    /// The text the result pane edits.
    pub(crate) fn text(&self) -> String {
        let mut text = String::new();
        let mut first = true;
        for line in self.slots.iter().flat_map(|slot| &slot.lines) {
            if !first {
                text.push('\n');
            }
            text.push_str(line);
            first = false;
        }
        if self.trailing_newline && !first {
            text.push('\n');
        }
        text
    }

    /// The text as worktree bytes, in the file's own line ending.
    pub(crate) fn bytes(&self) -> Vec<u8> {
        let lines: Vec<String> = self
            .slots
            .iter()
            .flat_map(|slot| slot.lines.iter().cloned())
            .collect();
        let bytes = self.line_ending.join(&lines, self.trailing_newline);
        if self.encoding.is_utf8() {
            bytes
        } else {
            self.encoding.encode(&String::from_utf8_lossy(&bytes))
        }
    }

    /// Applies `choice` to the selected conflict. Returns whether the result
    /// changed.
    pub(crate) fn apply(&mut self, choice: Choice) -> bool {
        let Some(index) = self.conflict_slot(self.selected) else {
            return false;
        };
        let (lines, resolution, trailing) = match choice {
            Choice::Ours => (
                self.slots[index].ours.clone(),
                Resolution::Ours,
                self.ours_trailing,
            ),
            Choice::Theirs => (
                self.slots[index].theirs.clone(),
                Resolution::Theirs,
                self.theirs_trailing,
            ),
            Choice::OursThenTheirs => {
                let mut lines = self.slots[index].ours.clone();
                lines.extend(self.slots[index].theirs.iter().cloned());
                (lines, Resolution::OursThenTheirs, self.theirs_trailing)
            }
            Choice::TheirsThenOurs => {
                let mut lines = self.slots[index].theirs.clone();
                lines.extend(self.slots[index].ours.iter().cloned());
                (lines, Resolution::TheirsThenOurs, self.ours_trailing)
            }
            Choice::Base => (
                self.slots[index].base.clone(),
                Resolution::Base,
                self.base_trailing,
            ),
        };
        if self.slots[index].lines == lines {
            // The same choice twice must still record which one it is, but the
            // result text did not change.
            self.slots[index].resolution = resolution;
            return false;
        }
        self.slots[index].lines = lines;
        self.slots[index].resolution = resolution;
        if index + 1 == self.slots.len() {
            self.trailing_newline = trailing;
        }
        true
    }

    /// Puts the selected conflict back to its unresolved marker block.
    pub(crate) fn reset(&mut self) -> bool {
        let Some(index) = self.conflict_slot(self.selected) else {
            return false;
        };
        if self.slots[index].resolution.is_unresolved() {
            return false;
        }
        self.slots[index].lines = self.slots[index].markers.clone();
        self.slots[index].resolution = Resolution::Unresolved;
        if index + 1 == self.slots.len() {
            self.trailing_newline = true;
        }
        true
    }

    /// Applies an edit made in the result pane.
    ///
    /// The changed span is located by the lines the new text shares with the
    /// old one, then written into the slot(s) it covers. A conflict that the
    /// edit touched becomes [`Resolution::Manual`]; a conflict the edit did not
    /// touch stays unresolved.
    pub(crate) fn apply_text(&mut self, text: &str) -> bool {
        if text == self.text() {
            return false;
        }
        let trailing_newline = text.ends_with('\n');
        let mut edited: Vec<&str> = text.split('\n').collect();
        if trailing_newline {
            // The final newline is a terminator, not an extra empty line.
            edited.pop();
        }
        let current: Vec<&str> = self
            .slots
            .iter()
            .flat_map(|slot| slot.lines.iter().map(String::as_str))
            .collect();

        let old_len = current.len();
        let new_len = edited.len();
        let mut prefix = 0;
        while prefix < old_len && prefix < new_len && current[prefix] == edited[prefix] {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < old_len - prefix
            && suffix < new_len - prefix
            && current[old_len - 1 - suffix] == edited[new_len - 1 - suffix]
        {
            suffix += 1;
        }
        let old_end = old_len - suffix;
        let new_end = new_len - suffix;
        let replacement: Vec<String> = edited[prefix..new_end]
            .iter()
            .map(|line| (*line).to_owned())
            .collect();

        let mut spans = Vec::with_capacity(self.slots.len());
        let mut at = 0;
        for slot in &self.slots {
            spans.push(at..at + slot.lines.len());
            at += slot.lines.len();
        }
        let touched: Vec<usize> = if prefix < old_end {
            spans
                .iter()
                .enumerate()
                .filter(|(_, span)| span.start < old_end && span.end > prefix)
                .map(|(index, _)| index)
                .collect()
        } else if spans.is_empty() {
            Vec::new()
        } else {
            // A pure insertion: put it at the end of the stretch the position
            // falls in, so typing after a conflict does not rewrite the
            // conflict's own text.
            let target = if prefix == 0 {
                0
            } else {
                spans
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, span)| span.start < prefix && prefix <= span.end)
                    .map(|(index, _)| index)
                    .unwrap_or(0)
            };
            vec![target]
        };

        if let Some(&first) = touched.first() {
            let last = *touched.last().expect("touched is not empty");
            let first_span = spans[first].clone();
            let last_span = spans[last].clone();
            let local_start = prefix
                .saturating_sub(first_span.start)
                .min(first_span.len());
            let local_end = old_end.saturating_sub(last_span.start).min(last_span.len());

            let mut lines = Vec::with_capacity(local_start + replacement.len());
            lines.extend_from_slice(&self.slots[first].lines[..local_start]);
            lines.extend(replacement);
            lines.extend_from_slice(&self.slots[last].lines[local_end..]);
            self.slots[first].lines = lines;
            for index in touched.iter().skip(1) {
                self.slots[*index].lines.clear();
            }
            for index in &touched {
                if self.slots[*index].conflict.is_some() {
                    self.slots[*index].resolution = Resolution::Manual;
                }
            }
        }

        self.trailing_newline = trailing_newline;
        true
    }

    fn conflict_slot(&self, conflict: usize) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.conflict == Some(conflict))
    }
}

#[cfg(test)]
mod tests {
    use strata_core::ConflictSides;
    use strata_core::merge::ConflictStyle;

    use super::*;

    fn sides(base: &str, ours: &str, theirs: &str) -> ConflictSides {
        ConflictSides {
            base: Some(base.as_bytes().to_vec()),
            ours: Some(ours.as_bytes().to_vec()),
            theirs: Some(theirs.as_bytes().to_vec()),
        }
    }

    fn model(base: &str, ours: &str, theirs: &str) -> MergeModel {
        let analysis = MergeAnalysis::analyze(&sides(base, ours, theirs), ConflictStyle::Merge);
        MergeModel::new(&analysis)
    }

    /// Two conflicts: line 1 changed by both, line 3 changed by both.
    fn two_conflicts() -> MergeModel {
        model(
            "one\ntwo\nthree\nfour\n",
            "one\nTWO-a\nthree\nFOUR-a\n",
            "one\nTWO-b\nthree\nFOUR-b\n",
        )
    }

    #[test]
    fn a_fresh_model_shows_markers_for_each_conflict() {
        let model = two_conflicts();
        assert_eq!(model.conflict_count(), 2);
        assert_eq!(model.counter().as_deref(), Some("Conflict 1 of 2"));
        assert_eq!(model.unresolved_count(), 2);
        assert!(!model.all_resolved());
        assert!(model.has_markers());
        let text = model.text();
        assert_eq!(text.matches("<<<<<<< ours").count(), 2);
        assert!(text.contains("TWO-a") && text.contains("TWO-b"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn taking_a_side_replaces_only_that_conflict() {
        let mut model = two_conflicts();
        assert!(model.apply(Choice::Ours));
        assert_eq!(model.unresolved_count(), 1);
        assert_eq!(model.selected_resolution(), Some(Resolution::Ours));
        let text = model.text();
        assert!(text.contains("TWO-a"));
        assert!(!text.contains("TWO-b"), "the other conflict is untouched");
        assert!(
            text.contains("<<<<<<< ours"),
            "the second conflict keeps markers"
        );
        assert!(text.contains("FOUR-a") && text.contains("FOUR-b"));
    }

    #[test]
    fn both_orders_concatenate_the_two_sides() {
        let mut ours_first = two_conflicts();
        ours_first.apply(Choice::OursThenTheirs);
        assert!(ours_first.text().contains("TWO-a\nTWO-b"));
        assert_eq!(
            ours_first.selected_resolution(),
            Some(Resolution::OursThenTheirs)
        );

        let mut theirs_first = two_conflicts();
        theirs_first.apply(Choice::TheirsThenOurs);
        assert!(theirs_first.text().contains("TWO-b\nTWO-a"));
    }

    #[test]
    fn the_counter_moves_and_wraps() {
        let mut model = two_conflicts();
        assert!(model.next_conflict());
        assert_eq!(model.counter().as_deref(), Some("Conflict 2 of 2"));
        assert!(model.next_conflict(), "next wraps to the first");
        assert_eq!(model.counter().as_deref(), Some("Conflict 1 of 2"));
        assert!(model.previous_conflict(), "previous wraps to the last");
        assert_eq!(model.counter().as_deref(), Some("Conflict 2 of 2"));
    }

    #[test]
    fn all_resolved_needs_every_conflict() {
        let mut model = two_conflicts();
        model.apply(Choice::Ours);
        model.next_conflict();
        assert!(!model.all_resolved());
        model.apply(Choice::Theirs);
        assert!(model.all_resolved());
        assert!(!model.has_markers());
        assert_eq!(model.unresolved_count(), 0);
    }

    #[test]
    fn taking_the_base_leaves_no_side_text() {
        let mut model = two_conflicts();
        model.apply(Choice::Base);
        let text = model.text();
        assert!(text.contains("two\n"));
        assert!(!text.contains("TWO-a"));
    }

    #[test]
    fn reset_restores_the_marker_block() {
        let mut model = two_conflicts();
        model.apply(Choice::Ours);
        assert!(model.reset());
        assert_eq!(model.unresolved_count(), 2);
        assert!(model.has_markers());
        assert!(!model.reset(), "an unresolved conflict has nothing to undo");
    }

    #[test]
    fn editing_inside_a_conflict_marks_it_manual() {
        let mut model = two_conflicts();
        let text = model.text().replace("TWO-a\n=======\nTWO-b", "TWO-merged");
        assert!(model.apply_text(&text));
        assert_eq!(model.selected_resolution(), Some(Resolution::Manual));
        assert_eq!(
            model.unresolved_count(),
            1,
            "the second conflict is untouched"
        );
        assert!(model.text().contains("TWO-merged"));
        assert!(model.text().contains("FOUR-b"));
    }

    #[test]
    fn editing_outside_the_conflicts_keeps_them_unresolved() {
        let mut model = two_conflicts();
        let mut text = model.text();
        text.insert_str(0, "header\n");
        assert!(model.apply_text(&text));
        assert_eq!(model.unresolved_count(), 2);
        assert!(model.text().starts_with("header\n"));
    }

    #[test]
    fn deleting_a_marker_block_resolves_that_conflict() {
        let mut model = two_conflicts();
        let text = model
            .text()
            .replace("<<<<<<< ours\nTWO-a\n=======\nTWO-b\n>>>>>>> theirs\n", "");
        assert!(model.apply_text(&text));
        assert_eq!(model.unresolved_count(), 1);
        assert_eq!(model.selected_resolution(), Some(Resolution::Manual));
    }

    #[test]
    fn applying_the_same_text_again_is_not_a_change() {
        let mut model = two_conflicts();
        assert!(!model.apply_text(&model.text()));
    }

    #[test]
    fn replacing_the_whole_result_keeps_exactly_what_was_typed() {
        let mut model = two_conflicts();
        assert!(model.apply_text("only this"));
        assert_eq!(model.text(), "only this");
        assert_eq!(model.bytes(), b"only this");
        assert_eq!(model.unresolved_count(), 0);
        assert!(!model.has_markers());
    }

    #[test]
    fn bytes_keep_crlf_and_a_missing_final_newline() {
        let analysis = MergeAnalysis::analyze(
            &sides("one\r\ntwo\r\n", "one\r\nTWO-a\r\n", "one\r\nTWO-b"),
            ConflictStyle::Merge,
        );
        let model = MergeModel::new(&analysis);
        assert_eq!(
            model.bytes(),
            b"one\r\n<<<<<<< ours\r\nTWO-a\r\n=======\r\nTWO-b\r\n>>>>>>> theirs\r\n"
        );
    }

    #[test]
    fn rows_align_steps_and_number_conflicts() {
        let analysis = MergeAnalysis::analyze(
            &sides("a\nb\n", "a\nb\nc\n", "a\nB\n"),
            ConflictStyle::Merge,
        );
        let (rows, conflict_rows) = MergeModel::rows(&analysis);
        assert_eq!(conflict_rows, vec![1], "the conflict starts at row 1");
        assert_eq!(rows[0].ours, Some(1));
        assert_eq!(rows[0].theirs, Some(1));
        assert_eq!(rows[0].conflict, None);
        assert_eq!(rows[1].conflict, Some(0));
        assert_eq!(rows[1].ours, Some(2));
        assert_eq!(rows[1].theirs, Some(2));
        assert_eq!(rows[2].ours, Some(3), "our inserted line gets its own row");
        assert_eq!(rows[2].theirs, None, "theirs has no line there");
    }

    #[test]
    fn a_legacy_encoded_merge_writes_the_same_encoding_back() {
        let shift_jis =
            strata_core::encoding::TextEncoding::from_label("Shift_JIS").expect("Shift_JIS");
        let base = shift_jis.encode("設定\n");
        let ours = shift_jis.encode("設定\n速度は速い\n");
        let theirs = ours.clone();
        let analysis = MergeAnalysis::analyze(
            &ConflictSides {
                base: Some(base),
                ours: Some(ours.clone()),
                theirs: Some(theirs),
            },
            ConflictStyle::Merge,
        );
        assert!(analysis.text, "legacy text is editable");
        assert!(!analysis.binary);
        assert_eq!(analysis.encoding.label(), "Shift_JIS");
        assert_eq!(analysis.ours.lines, ["設定", "速度は速い"]);

        let model = MergeModel::new(&analysis);
        assert_eq!(
            model.bytes(),
            ours,
            "the merge result must not turn into UTF-8"
        );
    }
}
