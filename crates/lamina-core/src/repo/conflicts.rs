//! Repository state and conflicted paths. Owned by the conflicts task.

use std::collections::BTreeMap;

use gix::bstr::{BStr, ByteSlice};
use gix::index::entry::Stage;

use super::Repo;
use crate::merge::ConflictStyle;
use crate::{ConflictEntry, ConflictKind, ConflictSides, Error, HeadInfo, RepoState, Result};

/// Index stage bits, for combining the stages present on one path.
const BASE: u8 = 1 << 0;
const OURS: u8 = 1 << 1;
const THEIRS: u8 = 1 << 2;

/// The external merge tool the repository's configuration selects.
///
/// `merge.guitool` is reported before `merge.tool` because git uses it for
/// `git mergetool --gui`, which is the mode a desktop app runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeTool {
    /// The configured tool name, e.g. `meld` or a custom `mergetool.<name>`.
    pub name: String,
    /// Whether the name came from `merge.guitool`, so git needs `--gui`.
    pub gui: bool,
}

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

    /// The merge tool `git mergetool` would run, and whether it is the GUI one.
    ///
    /// An empty value counts as unset: git treats it the same way, and a button
    /// that opened git's fallback list is exactly what the UI must not offer.
    pub fn merge_tool(&self) -> Option<MergeTool> {
        let mut repo = self.local();
        // The configuration may have changed since the repository was opened.
        let _ = repo.reload();
        let config = repo.config_snapshot();
        let read = |key: &str| {
            config
                .string(key)
                .map(|value| String::from_utf8_lossy(value.as_ref()).into_owned())
                .filter(|value| !value.trim().is_empty())
        };
        merge_tool_from(read("merge.guitool"), read("merge.tool"))
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

    /// Content of `path` for the left side of an index-to-worktree diff.
    ///
    /// A normal path answers its stage-0 entry. An unmerged path has no stage 0,
    /// so the "ours" stage stands in: that is the version the conflict markers
    /// surround, which keeps the diff to the conflicted regions instead of
    /// showing the whole file as added. A conflict without ours (deleted by us)
    /// falls back to HEAD, the last version we had.
    pub fn file_in_index_or_ours(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let repo = self.local();
        let index = repo.index_or_empty().map_err(Error::gix)?;
        let path_bstr = BStr::new(path);

        if let Some(entry) = index.entry_by_path_and_stage(path_bstr, Stage::Unconflicted) {
            return blob_content(&repo, entry);
        }

        let mut unmerged = false;
        for stage in [Stage::Base, Stage::Ours, Stage::Theirs] {
            let Some(entry) = index.entry_by_path_and_stage(path_bstr, stage) else {
                continue;
            };
            unmerged = true;
            if stage == Stage::Ours {
                return blob_content(&repo, entry);
            }
        }
        if !unmerged {
            return Ok(None);
        }

        let head = match self.head()? {
            HeadInfo::Branch {
                target: Some(commit),
                ..
            } => Some(commit),
            HeadInfo::Detached { target } => Some(target),
            _ => None,
        };
        match head {
            Some(commit) => self.file_at_commit(commit, path),
            None => Ok(None),
        }
    }
}

/// A blob's bytes, unless the entry is a submodule or a sparse directory, whose
/// "content" is not text to show.
fn blob_content(repo: &gix::Repository, entry: &gix::index::Entry) -> Result<Option<Vec<u8>>> {
    if entry.mode.is_submodule() || entry.mode.is_sparse() {
        return Ok(None);
    }
    Ok(Some(
        repo.find_blob(entry.id).map_err(Error::gix)?.data.clone(),
    ))
}

fn stage_bit(stage: Stage) -> u8 {
    match stage {
        Stage::Base => BASE,
        Stage::Ours => OURS,
        Stage::Theirs => THEIRS,
        Stage::Unconflicted => 0,
    }
}

/// The tool git would pick from the two configured names.
///
/// A GUI tool wins even when a terminal one is also set: the app runs
/// `git mergetool --gui`, which is what `merge.guitool` configures.
fn merge_tool_from(guitool: Option<String>, tool: Option<String>) -> Option<MergeTool> {
    match (guitool, tool) {
        (Some(name), _) => Some(MergeTool { name, gui: true }),
        (None, Some(name)) => Some(MergeTool { name, gui: false }),
        (None, None) => None,
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
    use crate::test_support::TestRepo;

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

    /// The header offers the external tool only when one is configured.
    #[test]
    fn merge_tool_gating_follows_the_configured_names() {
        assert_eq!(merge_tool_from(None, None), None);
        assert_eq!(
            merge_tool_from(None, Some("meld".to_owned())),
            Some(MergeTool {
                name: "meld".to_owned(),
                gui: false,
            })
        );
        assert_eq!(
            merge_tool_from(Some("kdiff3".to_owned()), None),
            Some(MergeTool {
                name: "kdiff3".to_owned(),
                gui: true,
            })
        );
        assert_eq!(
            merge_tool_from(Some("kdiff3".to_owned()), Some("meld".to_owned())),
            Some(MergeTool {
                name: "kdiff3".to_owned(),
                gui: true,
            }),
            "a GUI tool wins because the app runs git mergetool --gui"
        );
    }

    #[test]
    fn the_repository_config_selects_the_merge_tool() {
        let repo = TestRepo::new();
        repo.write("a.txt", "a\n");
        repo.commit("a");
        let open = repo.open();
        // A repository-local value beats anything the host's global config may
        // set, so this asserts the read order without depending on the machine.
        repo.git(&["config", "merge.tool", "meld"]);
        assert_eq!(
            open.merge_tool(),
            Some(MergeTool {
                name: "meld".to_owned(),
                gui: false,
            })
        );

        repo.git(&["config", "merge.guitool", "kdiff3"]);
        assert_eq!(
            open.merge_tool(),
            Some(MergeTool {
                name: "kdiff3".to_owned(),
                gui: true,
            }),
            "merge.guitool is the one --gui uses"
        );
    }
}
