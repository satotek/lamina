//! Synthetic "uncommitted changes" row helpers for the History view.
//!
//! Computes presence rules, human-readable summary strings (normal worktree
//! changes and in-progress operations), HEAD/MERGE_HEAD graph geometry, and
//! the ordered working-tree section list (`Conflicted -> Unstaged -> Staged`).

use lamina_core::repo::InProgressDetails;
use lamina_core::session::RepoSnapshot;
use lamina_core::{HeadInfo, Oid, RepoState};

use super::in_progress::details_match_state;
use crate::graph_canvas::{ConnectorEnds, MergeEnd, UncommittedGraph, plan_worktree_connector};
use crate::worktree_panel::visible_unstaged;

/// Counts displayed by the synthetic row and its details summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct UncommittedCounts {
    pub conflicts: usize,
    pub unstaged: usize,
    pub staged: usize,
}

impl UncommittedCounts {
    pub fn total(self) -> usize {
        self.conflicts + self.unstaged + self.staged
    }
}

/// Returns whether the synthetic uncommitted row should appear at the top of
/// the History list for `snapshot`.
///
/// Any dirty working tree/index or any active multi-step operation warrants the
/// row so an in-progress merge with an empty index is still visible.
pub(crate) fn should_show_row(snapshot: &RepoSnapshot) -> bool {
    snapshot.state != RepoState::Normal
        || !snapshot.conflicts.is_empty()
        || !snapshot.status.staged.is_empty()
        || !snapshot.status.unstaged.is_empty()
}

pub(crate) fn counts(snapshot: &RepoSnapshot) -> UncommittedCounts {
    UncommittedCounts {
        conflicts: snapshot.conflicts.len(),
        unstaged: visible_unstaged(&snapshot.status).len(),
        staged: snapshot.status.staged.len(),
    }
}

fn head_branch_name(head: Option<&HeadInfo>) -> Option<&str> {
    match head {
        Some(HeadInfo::Branch { short_name, .. }) => Some(short_name.as_str()),
        Some(HeadInfo::Detached { .. }) | None => None,
    }
}

fn head_target(head: Option<&HeadInfo>) -> Option<Oid> {
    match head {
        Some(HeadInfo::Branch { target, .. }) => *target,
        Some(HeadInfo::Detached { target }) => Some(*target),
        None => None,
    }
}

/// Summary label for the synthetic uncommitted row, matching the banner's
/// operation state wording when git is mid-operation.
pub(crate) fn summary_text(snapshot: &RepoSnapshot, details: Option<&InProgressDetails>) -> String {
    let counts = counts(snapshot);
    let mut parts = Vec::new();

    if snapshot.state == RepoState::Normal {
        parts.push(t!("history.uncommitted.summary").to_string());
        append_change_counts(&mut parts, counts);
        return parts.join(" · ");
    }

    let details = details.filter(|d| details_match_state(d, snapshot.state));
    let target_branch = head_branch_name(snapshot.head.as_ref());

    match (snapshot.state, details) {
        (RepoState::Merging, Some(InProgressDetails::Merge { heads, source })) => {
            let source_name = source
                .as_deref()
                .or_else(|| heads.first().map(|head| head.short_id.as_str()));
            let head = match (source_name, target_branch) {
                (Some(source), Some(target)) => t!(
                    "history.uncommitted.merging_into",
                    source = source,
                    target = target
                )
                .to_string(),
                (Some(source), None) => {
                    t!("history.uncommitted.merging", source = source).to_string()
                }
                (None, _) => t!("progress.merging").to_string(),
            };
            parts.push(head);
        }
        (
            RepoState::Rebasing,
            Some(InProgressDetails::Rebase {
                branch,
                onto,
                step,
                total_steps,
                ..
            }),
        ) => {
            let onto_label = onto.as_ref().map(|commit| {
                commit
                    .ref_name
                    .as_deref()
                    .unwrap_or(commit.short_id.as_str())
            });
            let head = match (branch.as_deref(), onto_label) {
                (Some(branch), Some(target)) => t!(
                    "history.uncommitted.rebasing_onto",
                    branch = branch,
                    target = target
                )
                .to_string(),
                (Some(branch), None) => {
                    t!("history.uncommitted.rebasing", branch = branch).to_string()
                }
                (None, _) => t!("progress.rebasing").to_string(),
            };
            parts.push(head);
            match (step, total_steps) {
                (Some(step), Some(total)) => {
                    parts.push(t!("progress.step_of", step = step, total = total).to_string())
                }
                (Some(step), None) => parts.push(t!("progress.step", step = step).to_string()),
                _ => {}
            }
        }
        (RepoState::CherryPicking, Some(InProgressDetails::CherryPick { commit, remaining })) => {
            let head = match commit {
                Some(commit) => t!(
                    "progress.cherry_picking_commit",
                    commit = commit.short_id.clone()
                )
                .to_string(),
                None => t!("progress.cherry_picking").to_string(),
            };
            parts.push(head);
            if let Some(remaining) = remaining {
                parts.push(t!("progress.remaining", count = remaining).to_string());
            }
        }
        (RepoState::Reverting, Some(InProgressDetails::Revert { commit, remaining })) => {
            let head = match commit {
                Some(commit) => t!(
                    "progress.reverting_commit",
                    commit = commit.short_id.clone()
                )
                .to_string(),
                None => t!("progress.reverting").to_string(),
            };
            parts.push(head);
            if let Some(remaining) = remaining {
                parts.push(t!("progress.remaining", count = remaining).to_string());
            }
        }
        (
            RepoState::ApplyingPatches,
            Some(InProgressDetails::ApplyingPatches {
                step, total_steps, ..
            }),
        ) => {
            parts.push(t!("progress.applying_patches").to_string());
            match (step, total_steps) {
                (Some(step), Some(total)) => {
                    parts.push(t!("progress.patch_of", step = step, total = total).to_string())
                }
                (Some(step), None) => parts.push(t!("progress.patch", step = step).to_string()),
                _ => {}
            }
        }
        (RepoState::Bisecting, _) => parts.push(t!("progress.bisecting").to_string()),
        (_, _) => parts.push(match snapshot.state {
            RepoState::Normal => unreachable!("handled above"),
            RepoState::Merging => t!("progress.merging").to_string(),
            RepoState::Rebasing => t!("progress.rebasing").to_string(),
            RepoState::CherryPicking => t!("progress.cherry_picking").to_string(),
            RepoState::Reverting => t!("progress.reverting").to_string(),
            RepoState::ApplyingPatches => t!("progress.applying_patches").to_string(),
            RepoState::Bisecting => t!("progress.bisecting").to_string(),
        }),
    }

    append_change_counts(&mut parts, counts);
    parts.join(" · ")
}

fn append_change_counts(parts: &mut Vec<String>, counts: UncommittedCounts) {
    if counts.conflicts > 0 {
        parts.push(t!("history.uncommitted.conflicts", count = counts.conflicts).to_string());
    }
    if counts.unstaged > 0 {
        parts.push(t!("history.uncommitted.unstaged", count = counts.unstaged).to_string());
    }
    if counts.staged > 0 {
        parts.push(t!("history.uncommitted.staged", count = counts.staged).to_string());
    }
}

/// HEAD's commit id and, while merging, `MERGE_HEAD`'s. The connector plan is
/// cached against this pair: a status refresh keeps the same history, so the
/// lanes do not need to be walked again.
pub(crate) fn graph_anchors(
    snapshot: &RepoSnapshot,
    details: Option<&InProgressDetails>,
) -> (Option<Oid>, Option<Oid>) {
    (
        head_target(snapshot.head.as_ref()),
        merge_tip(snapshot.state, details),
    )
}

fn merge_tip(state: RepoState, details: Option<&InProgressDetails>) -> Option<Oid> {
    if state != RepoState::Merging {
        return None;
    }
    match details.filter(|details| details_match_state(details, RepoState::Merging)) {
        Some(InProgressDetails::Merge { heads, .. }) => heads.first().and_then(|head| head.id),
        _ => None,
    }
}

/// Resolves the synthetic row's graph lane, HEAD dashed edge, and optional
/// MERGE_HEAD dashed edge directly from `snapshot` without altering core graph
/// types.
///
/// The core layout is left alone on purpose. Whether the working tree is dirty
/// flips on every edit, and feeding a synthetic node into that layout would
/// rebuild every row. The connector is chosen here, once per history, and the
/// rows only shift while they are painted.
pub(crate) fn graph_info(
    snapshot: &RepoSnapshot,
    details: Option<&InProgressDetails>,
) -> UncommittedGraph {
    let head_oid = head_target(snapshot.head.as_ref());
    let head_index = head_oid.and_then(|oid| snapshot.commit_index.get(&oid).copied());
    let (head_lane, head_color) = head_index
        .and_then(|index| snapshot.commits.get(index))
        .map(|(_, row)| (row.node_lane, row.node_color))
        .unwrap_or((0, 0));

    // The row, not only the lane: the dashed edge has to run down through the
    // rows in between, so it needs to know where the target node sits.
    let merge = merge_tip(snapshot.state, details).and_then(|oid| {
        let index = *snapshot.commit_index.get(&oid)?;
        let (_, row) = snapshot.commits.get(index)?;
        Some(MergeEnd {
            index,
            lane: row.node_lane,
            color: row.node_color,
        })
    });

    plan_worktree_connector(
        snapshot.commits.len(),
        |index| snapshot.commits.get(index).map(|(_, row)| row),
        ConnectorEnds {
            head_index,
            head_lane,
            head_color,
            head_present: head_oid.is_some(),
            merge,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use lamina_core::graph::{GraphLayout, GraphRows};
    use lamina_core::repo::{InProgressCommit, InProgressDetails};
    use lamina_core::session::{CommitList, RepoSnapshot};
    use lamina_core::test_support::TestRepo;
    use lamina_core::{
        ChangeKind, ConflictEntry, ConflictKind, FileChange, HeadInfo, RepoState, WorktreeStatus,
    };

    use super::*;

    fn empty_snapshot() -> RepoSnapshot {
        RepoSnapshot {
            generation: 1,
            head: None,
            refs: Arc::new(Vec::new()),
            refs_by_commit: Arc::new(HashMap::new()),
            commits: CommitList::default(),
            commit_index: Arc::new(HashMap::new()),
            status: Arc::new(WorktreeStatus::default()),
            stashes: Arc::new(Vec::new()),
            upstreams: Arc::new(HashMap::new()),
            remotes: Vec::new(),
            state: RepoState::Normal,
            conflicts: Arc::new(Vec::new()),
            busy: None,
        }
    }

    fn change(path: &str, kind: ChangeKind) -> FileChange {
        FileChange {
            path: path.to_owned(),
            old_path: None,
            kind,
        }
    }

    fn snapshot_from_repo(repo: &TestRepo) -> RepoSnapshot {
        let opened = repo.open();
        let head = opened.head().ok();
        let status = opened.status().expect("status");
        let conflicts = opened.conflicts().expect("conflicts");
        let state = opened.state().expect("state");
        let refs = opened.refs().unwrap_or_default();

        let mut layout = GraphLayout::new();
        let mut commit_vec = Vec::new();
        let mut rows = GraphRows::default();
        let mut commit_index = HashMap::new();
        for (i, item) in opened.walk_commits().expect("walk").enumerate() {
            let commit = item.expect("commit");
            layout.push(commit.id, &commit.parents, &mut rows);
            commit_index.insert(commit.id, i);
            commit_vec.push(commit);
        }
        let mut commits = CommitList::default();
        commits.extend(commit_vec, rows);
        commits.complete = true;

        RepoSnapshot {
            generation: 1,
            head,
            refs: Arc::new(refs),
            refs_by_commit: Arc::new(HashMap::new()),
            commits,
            commit_index: Arc::new(commit_index),
            status: Arc::new(status),
            stashes: Arc::new(Vec::new()),
            upstreams: Arc::new(HashMap::new()),
            remotes: Vec::new(),
            state,
            conflicts: Arc::new(conflicts),
            busy: None,
        }
    }

    #[test]
    fn presence_rules_follow_tree_index_and_state() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("initial");

        let clean = snapshot_from_repo(&repo);
        assert!(
            !should_show_row(&clean),
            "clean repository must not show synthetic row"
        );

        repo.write("a.txt", "two\n");
        let unstaged = snapshot_from_repo(&repo);
        assert!(
            should_show_row(&unstaged),
            "unstaged modification must show synthetic row"
        );

        repo.git(&["add", "a.txt"]);
        let staged = snapshot_from_repo(&repo);
        assert!(
            should_show_row(&staged),
            "staged modification must show synthetic row"
        );

        let mut merge_clean = empty_snapshot();
        merge_clean.state = RepoState::Merging;
        assert!(
            should_show_row(&merge_clean),
            "merge in progress with clean tree must show synthetic row"
        );
    }

    #[test]
    fn summary_text_formats_normal_and_merge_states() {
        rust_i18n::set_locale("en");
        let mut snap = empty_snapshot();
        snap.head = Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: Some("0000000000000000000000000000000000000001".parse().unwrap()),
        });
        snap.status = Arc::new(WorktreeStatus {
            unstaged: vec![
                change("a.rs", ChangeKind::Modified),
                change("b.rs", ChangeKind::Modified),
                change("c.rs", ChangeKind::Untracked),
                change("conflict.rs", ChangeKind::Conflicted),
            ],
            staged: vec![change("d.rs", ChangeKind::Added)],
        });
        assert_eq!(
            summary_text(&snap, None),
            "Uncommitted changes · 3 unstaged · 1 staged"
        );

        snap.state = RepoState::Merging;
        snap.status = Arc::new(WorktreeStatus::default());
        snap.conflicts = Arc::new(vec![
            ConflictEntry {
                path: "x.rs".into(),
                kind: ConflictKind::BothModified,
            },
            ConflictEntry {
                path: "y.rs".into(),
                kind: ConflictKind::BothModified,
            },
            ConflictEntry {
                path: "z.rs".into(),
                kind: ConflictKind::BothAdded,
            },
        ]);
        let merge_details = InProgressDetails::Merge {
            heads: vec![InProgressCommit {
                id: None,
                short_id: "abcdef1".into(),
                summary: None,
            }],
            source: Some("feature".into()),
        };
        assert_eq!(
            summary_text(&snap, Some(&merge_details)),
            "Merging: feature → main · 3 unresolved"
        );

        rust_i18n::set_locale("ja");
        assert_eq!(
            summary_text(&snap, Some(&merge_details)),
            "Merge 中: feature → main · 未解決 3"
        );
        rust_i18n::set_locale("en");
    }

    #[test]
    fn merge_head_edge_resolves_source_branch_tip_in_real_conflict() {
        let repo = TestRepo::new();
        repo.write("shared.txt", "base\n");
        repo.commit("base");

        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("shared.txt", "feature line\n");
        let feature_tip = repo.commit("feature change");

        repo.git(&["checkout", "-q", "main"]);
        repo.write("shared.txt", "main line\n");
        let main_tip = repo.commit("main change");

        let output = repo.git_output(&["merge", "feature"]);
        assert!(
            !output.status.success(),
            "merge should stop on conflict so MERGE_HEAD is populated"
        );

        let snap = snapshot_from_repo(&repo);
        let opened = repo.open();
        let details = opened.in_progress_details().expect("details");
        assert_eq!(snap.state, RepoState::Merging);
        assert!(should_show_row(&snap));

        let graph = graph_info(&snap, details.as_ref());
        assert!(graph.has_head_edge);
        let head_idx = snap.commit_index[&main_tip];
        let feature_idx = snap.commit_index[&feature_tip];
        let (_, head_row) = snap.commits.get(head_idx).unwrap();
        let (_, feature_row) = snap.commits.get(feature_idx).unwrap();
        assert_eq!(graph.head_lane, head_row.node_lane);
        assert_eq!(graph.head_color, head_row.node_color);
        assert_eq!(
            graph.merge_target,
            Some((feature_row.node_lane, feature_row.node_color))
        );
        assert_eq!(
            graph.merge_target_index,
            Some(feature_idx),
            "the edge has to know which row holds the target node"
        );
        for index in 0..feature_idx {
            assert_eq!(
                graph.merge_edge_at(index).map(|edge| edge.to_node),
                Some(false),
                "row {index} passes the dashed edge on"
            );
        }
        assert_eq!(
            graph.merge_edge_at(feature_idx).map(|edge| edge.to_node),
            Some(true)
        );
        assert_eq!(
            graph.merge_edge_at(feature_idx + 1),
            None,
            "the edge ends at the MERGE_HEAD node"
        );
        assert_ne!(
            head_row.node_lane, feature_row.node_lane,
            "diverged branches occupy distinct lanes"
        );
        assert_eq!(
            graph.lane_shift, 0,
            "HEAD is the newest commit and MERGE_HEAD's lane is free above it"
        );
        assert_eq!(graph.head_index, Some(head_idx));
        assert_eq!(graph.connector_lane, Some(head_row.node_lane));
        assert!(!graph.connector_muted);
        let edge = graph
            .worktree_edge_at(head_idx)
            .expect("HEAD's row finishes the join the circle's row stopped at its bottom edge");
        assert!(edge.to_node);
        assert_eq!(edge.lane, graph.connector_lane.unwrap());
        assert_eq!(graph.merge_lane, Some(feature_row.node_lane));
    }

    #[test]
    fn a_newer_side_commit_puts_the_worktree_connector_on_its_own_lane() {
        let repo = TestRepo::new();
        repo.write("a.txt", "base\n");
        let head = repo.commit("head");
        repo.git(&["checkout", "-q", "-b", "side"]);
        repo.write("a.txt", "side\n");
        repo.commit("side");
        repo.git(&["checkout", "-q", "main"]);

        let snap = snapshot_from_repo(&repo);
        let graph = graph_info(&snap, None);
        let head_index = snap.commit_index[&head];
        assert!(head_index > 0, "the newer side commit sorts above HEAD");
        assert_eq!(graph.head_index, Some(head_index));
        assert!(graph.lane_shift >= 1);
        assert_eq!(graph.connector_lane, Some(0));
        assert!(graph.connector_muted);
        for index in 0..=head_index {
            let edge = graph.worktree_edge_at(index).expect("connector");
            assert_eq!(edge.lane, 0);
            assert_eq!(edge.to_node, index == head_index);
        }
        assert!(graph.worktree_edge_at(head_index + 1).is_none());
        for index in 0..snap.commits.len() {
            let (_, row) = snap.commits.get(index).unwrap();
            let occupies = |lane: u16| lane.saturating_add(graph.lane_shift) == 0;
            assert!(
                !occupies(row.node_lane)
                    && row.passes.iter().all(|pass| !occupies(pass.lane))
                    && row
                        .edges
                        .iter()
                        .all(|edge| !occupies(edge.from_lane) && !occupies(edge.to_lane)),
                "row {index} must leave the connector lane empty"
            );
        }
    }

    #[test]
    fn a_head_that_is_not_loaded_draws_no_connector() {
        let mut snap = empty_snapshot();
        snap.head = Some(HeadInfo::Branch {
            short_name: "main".into(),
            target: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap()),
        });
        let graph = graph_info(&snap, None);
        assert!(graph.has_head_edge);
        assert_eq!(graph.head_index, None);
        assert_eq!(graph.connector_lane, None);
        assert_eq!(graph.lane_shift, 0);
        assert!(graph.worktree_edge_at(0).is_none());
    }
}
