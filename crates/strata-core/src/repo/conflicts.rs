//! Repository state and conflicted paths. Owned by the conflicts task.

use std::collections::BTreeMap;

use gix::bstr::{BStr, ByteSlice};
use gix::index::entry::Stage;

use super::Repo;
use crate::merge::ConflictStyle;
use crate::{ConflictEntry, ConflictKind, ConflictSides, Error, RepoState, Result};

/// Index stage bits, for combining the stages present on one path.
const BASE: u8 = 1 << 0;
const OURS: u8 = 1 << 1;
const THEIRS: u8 = 1 << 2;

impl Repo {
    /// Which multi-step operation, if any, is in progress.
    pub fn state(&self) -> Result<RepoState> {
        Ok(match self.local().state() {
            None => RepoState::Normal,
            Some(gix::state::InProgress::Merge) => RepoState::Merging,
            // gix reports a plain `rebase-apply` directory as a mailbox rebase;
            // git itself treats that directory as a rebase that stopped early.
            Some(
                gix::state::InProgress::Rebase
                | gix::state::InProgress::RebaseInteractive
                | gix::state::InProgress::ApplyMailboxRebase,
            ) => RepoState::Rebasing,
            Some(
                gix::state::InProgress::CherryPick | gix::state::InProgress::CherryPickSequence,
            ) => RepoState::CherryPicking,
            Some(gix::state::InProgress::Revert | gix::state::InProgress::RevertSequence) => {
                RepoState::Reverting
            }
            Some(gix::state::InProgress::Bisect) => RepoState::Bisecting,
            Some(gix::state::InProgress::ApplyMailbox) => RepoState::ApplyingPatches,
        })
    }

    /// Unmerged paths in the index, sorted by path.
    pub fn conflicts(&self) -> Result<Vec<ConflictEntry>> {
        let repo = self.local();
        let index = repo.index_or_empty().map_err(Error::gix)?;

        // A map rather than consecutive grouping: the answer must be sorted by
        // path even if the index on disk is not in git's usual order.
        let mut stages: BTreeMap<Vec<u8>, u8> = BTreeMap::new();
        for entry in index.entries() {
            let stage = entry.stage();
            if stage == Stage::Unconflicted || entry.mode.is_sparse() {
                continue;
            }
            *stages.entry(entry.path(&index).to_vec()).or_default() |= stage_bit(stage);
        }

        Ok(stages
            .into_iter()
            .map(|(path, stages)| ConflictEntry {
                path: path.to_str_lossy().into_owned(),
                kind: conflict_kind(stages),
            })
            .collect())
    }

    /// `merge.conflictStyle`, which only decides how unresolved conflicts are
    /// written for display.
    pub fn conflict_style(&self) -> ConflictStyle {
        let mut repo = self.local();
        // The configuration may have changed since the repository was opened.
        let _ = repo.reload();
        let config = repo.config_snapshot();
        config
            .string("merge.conflictStyle")
            .map(|value| ConflictStyle::parse(&String::from_utf8_lossy(value.as_ref())))
            .unwrap_or_default()
    }

    /// Contents of the index stages of a conflicted `path`.
    pub fn conflict_sides(&self, path: &str) -> Result<ConflictSides> {
        let repo = self.local();
        let index = repo.index_or_empty().map_err(Error::gix)?;
        let path = BStr::new(path);

        let mut sides = ConflictSides::default();
        for (stage, side) in [
            (Stage::Base, &mut sides.base),
            (Stage::Ours, &mut sides.ours),
            (Stage::Theirs, &mut sides.theirs),
        ] {
            let Some(entry) = index.entry_by_path_and_stage(path, stage) else {
                continue;
            };
            // A submodule's "content" is a commit id, not text to show.
            if entry.mode.is_submodule() || entry.mode.is_sparse() {
                continue;
            }
            *side = Some(repo.find_blob(entry.id).map_err(Error::gix)?.data.clone());
        }
        Ok(sides)
    }
}

fn stage_bit(stage: Stage) -> u8 {
    match stage {
        Stage::Base => BASE,
        Stage::Ours => OURS,
        Stage::Theirs => THEIRS,
        Stage::Unconflicted => 0,
    }
}

/// Map the stages present on a path to the kind `git status` reports for it.
fn conflict_kind(stages: u8) -> ConflictKind {
    match (stages & BASE != 0, stages & OURS != 0, stages & THEIRS != 0) {
        (true, true, true) => ConflictKind::BothModified,
        (false, true, true) => ConflictKind::BothAdded,
        (true, false, false) => ConflictKind::BothDeleted,
        (false, true, false) => ConflictKind::AddedByUs,
        (false, false, true) => ConflictKind::AddedByThem,
        (true, false, true) => ConflictKind::DeletedByUs,
        (true, true, false) => ConflictKind::DeletedByThem,
        // Every unmerged entry carries a stage, so an empty set cannot happen;
        // reporting it as a content conflict keeps the path visible.
        (false, false, false) => ConflictKind::BothModified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The combinations git writes, with the `git status` code for each.
    #[test]
    fn maps_stage_combinations_to_status_kinds() {
        assert_eq!(
            conflict_kind(BASE | OURS | THEIRS),
            ConflictKind::BothModified,
            "UU"
        );
        assert_eq!(conflict_kind(OURS | THEIRS), ConflictKind::BothAdded, "AA");
        assert_eq!(conflict_kind(BASE), ConflictKind::BothDeleted, "DD");
        assert_eq!(conflict_kind(OURS), ConflictKind::AddedByUs, "AU");
        assert_eq!(conflict_kind(THEIRS), ConflictKind::AddedByThem, "UA");
        assert_eq!(
            conflict_kind(BASE | THEIRS),
            ConflictKind::DeletedByUs,
            "DU"
        );
        assert_eq!(
            conflict_kind(BASE | OURS),
            ConflictKind::DeletedByThem,
            "UD"
        );
    }
}
