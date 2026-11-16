//! Mutations the helper is willing to run.
//!
//! The values mirror [`lamina_core::session::Operation`]. A variant that is
//! not listed here stays off the wire, so a peer cannot ask the helper to run
//! it by sending a frame.

use serde::{Deserialize, Serialize};

use crate::codec::ProtocolError;
use crate::dto::query::ReflogEntryDto;
use crate::dto::repo::{self, ObjectId};

/// `Operation` variants this protocol does not carry, or a line index that
/// does not fit the wire integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedOperation;

impl std::fmt::Display for UnsupportedOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("operation is not available over remote")
    }
}

impl std::error::Error for UnsupportedOperation {}

/// True when `operation` can cross the wire.
///
/// The app uses this as the single gate for enabling a remote action: a
/// variant that fails conversion stays disabled.
pub fn is_supported_operation(operation: &lamina_core::session::Operation) -> bool {
    OperationDto::try_from(operation).is_ok()
}

/// Which side of a conflict to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictSideDto {
    Ours,
    Theirs,
}

impl From<lamina_core::session::ConflictSide> for ConflictSideDto {
    fn from(side: lamina_core::session::ConflictSide) -> Self {
        match side {
            lamina_core::session::ConflictSide::Ours => Self::Ours,
            lamina_core::session::ConflictSide::Theirs => Self::Theirs,
        }
    }
}

impl From<ConflictSideDto> for lamina_core::session::ConflictSide {
    fn from(side: ConflictSideDto) -> Self {
        match side {
            ConflictSideDto::Ours => Self::Ours,
            ConflictSideDto::Theirs => Self::Theirs,
        }
    }
}

/// How far a reset moves HEAD, the index and the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetModeDto {
    Soft,
    Mixed,
    Hard,
    Keep,
}

impl From<lamina_core::session::ResetMode> for ResetModeDto {
    fn from(mode: lamina_core::session::ResetMode) -> Self {
        match mode {
            lamina_core::session::ResetMode::Soft => Self::Soft,
            lamina_core::session::ResetMode::Mixed => Self::Mixed,
            lamina_core::session::ResetMode::Hard => Self::Hard,
            lamina_core::session::ResetMode::Keep => Self::Keep,
        }
    }
}

impl From<ResetModeDto> for lamina_core::session::ResetMode {
    fn from(mode: ResetModeDto) -> Self {
        match mode {
            ResetModeDto::Soft => Self::Soft,
            ResetModeDto::Mixed => Self::Mixed,
            ResetModeDto::Hard => Self::Hard,
            ResetModeDto::Keep => Self::Keep,
        }
    }
}

/// A branch on a remote, as an upstream records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBranchDto {
    pub remote: String,
    pub branch: String,
}

impl From<&lamina_core::session::RemoteBranch> for RemoteBranchDto {
    fn from(branch: &lamina_core::session::RemoteBranch) -> Self {
        Self {
            remote: branch.remote.clone(),
            branch: branch.branch.clone(),
        }
    }
}

impl From<RemoteBranchDto> for lamina_core::session::RemoteBranch {
    fn from(branch: RemoteBranchDto) -> Self {
        Self {
            remote: branch.remote,
            branch: branch.branch,
        }
    }
}

/// One mutation. Field order follows [`lamina_core::session::Operation`].
///
/// Line selections are `(hunk, line)` pairs of `u32` so the encoding does not
/// depend on the pointer width of either machine. Stash indexes are `u64` for
/// the same reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationDto {
    StagePaths(Vec<String>),
    UnstagePaths(Vec<String>),
    DiscardPaths(Vec<String>),
    DiscardStagedPaths {
        paths: Vec<String>,
    },
    DeleteUntracked(Vec<String>),
    StageLines {
        path: String,
        selection: Vec<(u32, u32)>,
    },
    UnstageLines {
        path: String,
        selection: Vec<(u32, u32)>,
    },
    DiscardLines {
        path: String,
        selection: Vec<(u32, u32)>,
    },
    Commit {
        message: String,
        amend: bool,
    },
    Checkout {
        branch: String,
    },
    CheckoutCommit {
        commit: ObjectId,
    },
    CreateBranch {
        name: String,
        start_point: Option<String>,
        checkout: bool,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    RenameBranch {
        name: String,
        new_name: String,
        rename_upstream: Option<RemoteBranchDto>,
    },
    SetBranchUpstream {
        branch: String,
        remote: String,
        remote_branch: String,
    },
    UnsetBranchUpstream {
        branch: String,
    },
    Fetch {
        remote: Option<String>,
        prune: bool,
    },
    FetchUnshallow,
    Pull {
        rebase: Option<bool>,
    },
    Push {
        remote: Option<String>,
        branch: Option<String>,
        set_upstream: bool,
        force_with_lease: bool,
    },
    StashPush {
        message: Option<String>,
        include_untracked: bool,
    },
    StashPushPaths {
        message: Option<String>,
        include_untracked: bool,
        paths: Vec<String>,
    },
    StashPop {
        index: u64,
    },
    StashApply {
        index: u64,
    },
    StashDrop {
        index: u64,
    },
    Merge {
        branch: String,
        no_ff: bool,
    },
    MergeAbort,
    ResolveConflict {
        path: String,
        side: ConflictSideDto,
    },
    MarkResolved(Vec<String>),
    MergeContinue,
    ResolveWithContent {
        path: String,
        content: Vec<u8>,
        expected: Option<u64>,
    },
    CherryPick {
        commit: ObjectId,
    },
    CherryPickMany {
        commits: Vec<ObjectId>,
    },
    Revert {
        commit: ObjectId,
    },
    Rebase {
        onto: ObjectId,
        upstream: Option<ObjectId>,
        autostash: bool,
    },
    Reset {
        target: ObjectId,
        mode: ResetModeDto,
    },
    ContinueInProgress,
    AbortInProgress,
    /// `None` undoes the newest HEAD action the helper can restore.
    UndoReflog {
        entry: Option<ReflogEntryDto>,
    },
    RedoReflog {
        entry: ReflogEntryDto,
    },
    /// `git submodule update [--init]`. An empty `paths` updates every submodule.
    SubmoduleUpdate {
        paths: Vec<String>,
        init: bool,
    },
    /// `git worktree add` at an absolute path the user typed.
    WorktreeAdd {
        path: String,
        branch: Option<String>,
        new_branch: Option<String>,
        commit: Option<ObjectId>,
    },
    /// `git worktree remove` of an absolute path.
    WorktreeRemove {
        path: String,
        force: bool,
    },
    /// `git worktree prune`.
    WorktreePrune,
    /// `git lfs track`. `path` is a pattern or a worktree path.
    LfsTrack {
        path: String,
    },
    /// `git lfs lock`.
    LfsLock {
        path: String,
    },
    /// `git lfs unlock`.
    LfsUnlock {
        path: String,
    },
    /// `git lfs pull --include=<path>`.
    LfsPull {
        path: String,
    },
}

impl TryFrom<&lamina_core::session::Operation> for OperationDto {
    type Error = UnsupportedOperation;

    fn try_from(operation: &lamina_core::session::Operation) -> Result<Self, Self::Error> {
        use lamina_core::session::Operation;
        Ok(match operation {
            Operation::StagePaths(paths) => Self::StagePaths(paths.clone()),
            Operation::UnstagePaths(paths) => Self::UnstagePaths(paths.clone()),
            Operation::DiscardPaths(paths) => Self::DiscardPaths(paths.clone()),
            Operation::DiscardStagedPaths { paths } => Self::DiscardStagedPaths {
                paths: paths.clone(),
            },
            Operation::DeleteUntracked(paths) => Self::DeleteUntracked(paths.clone()),
            Operation::StageLines { path, selection } => Self::StageLines {
                path: path.clone(),
                selection: line_pairs(selection)?,
            },
            Operation::UnstageLines { path, selection } => Self::UnstageLines {
                path: path.clone(),
                selection: line_pairs(selection)?,
            },
            Operation::DiscardLines { path, selection } => Self::DiscardLines {
                path: path.clone(),
                selection: line_pairs(selection)?,
            },
            Operation::Commit { message, amend } => Self::Commit {
                message: message.clone(),
                amend: *amend,
            },
            Operation::Checkout { branch } => Self::Checkout {
                branch: branch.clone(),
            },
            Operation::CheckoutCommit { commit } => Self::CheckoutCommit {
                commit: ObjectId::from(*commit),
            },
            Operation::CreateBranch {
                name,
                start_point,
                checkout,
            } => Self::CreateBranch {
                name: name.clone(),
                start_point: start_point.clone(),
                checkout: *checkout,
            },
            Operation::DeleteBranch { name, force } => Self::DeleteBranch {
                name: name.clone(),
                force: *force,
            },
            Operation::RenameBranch {
                name,
                new_name,
                rename_upstream,
            } => Self::RenameBranch {
                name: name.clone(),
                new_name: new_name.clone(),
                rename_upstream: rename_upstream.as_ref().map(RemoteBranchDto::from),
            },
            Operation::SetBranchUpstream {
                branch,
                remote,
                remote_branch,
            } => Self::SetBranchUpstream {
                branch: branch.clone(),
                remote: remote.clone(),
                remote_branch: remote_branch.clone(),
            },
            Operation::UnsetBranchUpstream { branch } => Self::UnsetBranchUpstream {
                branch: branch.clone(),
            },
            Operation::Fetch { remote, prune } => Self::Fetch {
                remote: remote.clone(),
                prune: *prune,
            },
            Operation::FetchUnshallow => Self::FetchUnshallow,
            Operation::Pull { rebase } => Self::Pull { rebase: *rebase },
            Operation::Push {
                remote,
                branch,
                set_upstream,
                force_with_lease,
            } => Self::Push {
                remote: remote.clone(),
                branch: branch.clone(),
                set_upstream: *set_upstream,
                force_with_lease: *force_with_lease,
            },
            Operation::StashPush {
                message,
                include_untracked,
            } => Self::StashPush {
                message: message.clone(),
                include_untracked: *include_untracked,
            },
            Operation::StashPushPaths {
                message,
                include_untracked,
                paths,
            } => Self::StashPushPaths {
                message: message.clone(),
                include_untracked: *include_untracked,
                paths: paths.clone(),
            },
            Operation::StashPop { index } => Self::StashPop {
                index: u64::try_from(*index).map_err(|_| UnsupportedOperation)?,
            },
            Operation::StashApply { index } => Self::StashApply {
                index: u64::try_from(*index).map_err(|_| UnsupportedOperation)?,
            },
            Operation::StashDrop { index } => Self::StashDrop {
                index: u64::try_from(*index).map_err(|_| UnsupportedOperation)?,
            },
            Operation::Merge { branch, no_ff } => Self::Merge {
                branch: branch.clone(),
                no_ff: *no_ff,
            },
            Operation::MergeAbort => Self::MergeAbort,
            Operation::ResolveConflict { path, side } => Self::ResolveConflict {
                path: path.clone(),
                side: ConflictSideDto::from(*side),
            },
            Operation::MarkResolved(paths) => Self::MarkResolved(paths.clone()),
            Operation::MergeContinue => Self::MergeContinue,
            Operation::ResolveWithContent {
                path,
                content,
                expected,
            } => Self::ResolveWithContent {
                path: path.clone(),
                content: content.clone(),
                expected: expected.map(lamina_core::merge::ContentHash::fingerprint),
            },
            Operation::CherryPick { commit } => Self::CherryPick {
                commit: ObjectId::from(*commit),
            },
            Operation::CherryPickMany { commits } => Self::CherryPickMany {
                commits: commits.iter().copied().map(ObjectId::from).collect(),
            },
            Operation::Revert { commit } => Self::Revert {
                commit: ObjectId::from(*commit),
            },
            Operation::Rebase {
                onto,
                upstream,
                autostash,
            } => Self::Rebase {
                onto: ObjectId::from(*onto),
                upstream: upstream.map(ObjectId::from),
                autostash: *autostash,
            },
            Operation::Reset { target, mode } => Self::Reset {
                target: ObjectId::from(*target),
                mode: ResetModeDto::from(*mode),
            },
            Operation::ContinueInProgress => Self::ContinueInProgress,
            Operation::AbortInProgress => Self::AbortInProgress,
            Operation::UndoReflog { entry } => Self::UndoReflog {
                entry: entry.as_ref().map(ReflogEntryDto::from),
            },
            Operation::RedoReflog { entry } => Self::RedoReflog {
                entry: ReflogEntryDto::from(entry),
            },
            Operation::SubmoduleUpdate { paths, init } => Self::SubmoduleUpdate {
                paths: paths.clone(),
                init: *init,
            },
            Operation::WorktreeAdd {
                path,
                branch,
                new_branch,
                commit,
            } => Self::WorktreeAdd {
                path: path_on_wire(path)?,
                branch: branch.clone(),
                new_branch: new_branch.clone(),
                commit: commit.map(ObjectId::from),
            },
            Operation::WorktreeRemove { path, force } => Self::WorktreeRemove {
                path: path_on_wire(path)?,
                force: *force,
            },
            Operation::WorktreePrune => Self::WorktreePrune,
            Operation::LfsTrack { path } => Self::LfsTrack { path: path.clone() },
            Operation::LfsLock { path } => Self::LfsLock { path: path.clone() },
            Operation::LfsUnlock { path } => Self::LfsUnlock { path: path.clone() },
            Operation::LfsPull { path } => Self::LfsPull { path: path.clone() },
            // Kept off the wire on purpose: the remote tab still refuses these.
            Operation::AddRemote { .. }
            | Operation::RenameRemote { .. }
            | Operation::SetRemoteUrl { .. }
            | Operation::RemoveRemote { .. }
            | Operation::OpenMergetool { .. }
            | Operation::RebaseEdit { .. }
            | Operation::ApplyPatch { .. }
            | Operation::CreateTag { .. }
            | Operation::DeleteTag { .. }
            | Operation::RewordAcrossMerges { .. }
            | Operation::RewriteAcrossMerges { .. }
            | Operation::WriteCommitGraph => return Err(UnsupportedOperation),
        })
    }
}

impl TryFrom<OperationDto> for lamina_core::session::Operation {
    type Error = ProtocolError;

    fn try_from(operation: OperationDto) -> Result<Self, ProtocolError> {
        use lamina_core::session::Operation;
        Ok(match operation {
            OperationDto::StagePaths(paths) => Operation::StagePaths(paths),
            OperationDto::UnstagePaths(paths) => Operation::UnstagePaths(paths),
            OperationDto::DiscardPaths(paths) => Operation::DiscardPaths(paths),
            OperationDto::DiscardStagedPaths { paths } => Operation::DiscardStagedPaths { paths },
            OperationDto::DeleteUntracked(paths) => Operation::DeleteUntracked(paths),
            OperationDto::StageLines { path, selection } => Operation::StageLines {
                path,
                selection: line_selection(selection)?,
            },
            OperationDto::UnstageLines { path, selection } => Operation::UnstageLines {
                path,
                selection: line_selection(selection)?,
            },
            OperationDto::DiscardLines { path, selection } => Operation::DiscardLines {
                path,
                selection: line_selection(selection)?,
            },
            OperationDto::Commit { message, amend } => Operation::Commit { message, amend },
            OperationDto::Checkout { branch } => Operation::Checkout { branch },
            OperationDto::CheckoutCommit { commit } => Operation::CheckoutCommit {
                commit: repo::oid(commit)?,
            },
            OperationDto::CreateBranch {
                name,
                start_point,
                checkout,
            } => Operation::CreateBranch {
                name,
                start_point,
                checkout,
            },
            OperationDto::DeleteBranch { name, force } => Operation::DeleteBranch { name, force },
            OperationDto::RenameBranch {
                name,
                new_name,
                rename_upstream,
            } => Operation::RenameBranch {
                name,
                new_name,
                rename_upstream: rename_upstream.map(Into::into),
            },
            OperationDto::SetBranchUpstream {
                branch,
                remote,
                remote_branch,
            } => Operation::SetBranchUpstream {
                branch,
                remote,
                remote_branch,
            },
            OperationDto::UnsetBranchUpstream { branch } => {
                Operation::UnsetBranchUpstream { branch }
            }
            OperationDto::Fetch { remote, prune } => Operation::Fetch { remote, prune },
            OperationDto::FetchUnshallow => Operation::FetchUnshallow,
            OperationDto::Pull { rebase } => Operation::Pull { rebase },
            OperationDto::Push {
                remote,
                branch,
                set_upstream,
                force_with_lease,
            } => Operation::Push {
                remote,
                branch,
                set_upstream,
                force_with_lease,
            },
            OperationDto::StashPush {
                message,
                include_untracked,
            } => Operation::StashPush {
                message,
                include_untracked,
            },
            OperationDto::StashPushPaths {
                message,
                include_untracked,
                paths,
            } => Operation::StashPushPaths {
                message,
                include_untracked,
                paths,
            },
            OperationDto::StashPop { index } => Operation::StashPop {
                index: stash_index(index)?,
            },
            OperationDto::StashApply { index } => Operation::StashApply {
                index: stash_index(index)?,
            },
            OperationDto::StashDrop { index } => Operation::StashDrop {
                index: stash_index(index)?,
            },
            OperationDto::Merge { branch, no_ff } => Operation::Merge { branch, no_ff },
            OperationDto::MergeAbort => Operation::MergeAbort,
            OperationDto::ResolveConflict { path, side } => Operation::ResolveConflict {
                path,
                side: side.into(),
            },
            OperationDto::MarkResolved(paths) => Operation::MarkResolved(paths),
            OperationDto::MergeContinue => Operation::MergeContinue,
            OperationDto::ResolveWithContent {
                path,
                content,
                expected,
            } => Operation::ResolveWithContent {
                path,
                content,
                expected: expected.map(lamina_core::merge::ContentHash::from_fingerprint),
            },
            OperationDto::CherryPick { commit } => Operation::CherryPick {
                commit: repo::oid(commit)?,
            },
            OperationDto::CherryPickMany { commits } => Operation::CherryPickMany {
                commits: commits
                    .into_iter()
                    .map(repo::oid)
                    .collect::<Result<_, _>>()?,
            },
            OperationDto::Revert { commit } => Operation::Revert {
                commit: repo::oid(commit)?,
            },
            OperationDto::Rebase {
                onto,
                upstream,
                autostash,
            } => Operation::Rebase {
                onto: repo::oid(onto)?,
                upstream: upstream.map(repo::oid).transpose()?,
                autostash,
            },
            OperationDto::Reset { target, mode } => Operation::Reset {
                target: repo::oid(target)?,
                mode: mode.into(),
            },
            OperationDto::ContinueInProgress => Operation::ContinueInProgress,
            OperationDto::AbortInProgress => Operation::AbortInProgress,
            OperationDto::UndoReflog { entry } => Operation::UndoReflog {
                entry: entry.map(TryInto::try_into).transpose()?,
            },
            OperationDto::RedoReflog { entry } => Operation::RedoReflog {
                entry: entry.try_into()?,
            },
            OperationDto::SubmoduleUpdate { paths, init } => {
                Operation::SubmoduleUpdate { paths, init }
            }
            OperationDto::WorktreeAdd {
                path,
                branch,
                new_branch,
                commit,
            } => Operation::WorktreeAdd {
                path: std::path::PathBuf::from(path),
                branch,
                new_branch,
                commit: commit.map(repo::oid).transpose()?,
            },
            OperationDto::WorktreeRemove { path, force } => Operation::WorktreeRemove {
                path: std::path::PathBuf::from(path),
                force,
            },
            OperationDto::WorktreePrune => Operation::WorktreePrune,
            OperationDto::LfsTrack { path } => Operation::LfsTrack { path },
            OperationDto::LfsLock { path } => Operation::LfsLock { path },
            OperationDto::LfsUnlock { path } => Operation::LfsUnlock { path },
            OperationDto::LfsPull { path } => Operation::LfsPull { path },
        })
    }
}

/// Worktree paths cross the wire as text. A path that is not Unicode cannot
/// be named in a frame, so the operation stays local.
fn path_on_wire(path: &std::path::Path) -> Result<String, UnsupportedOperation> {
    path.to_str().map(str::to_owned).ok_or(UnsupportedOperation)
}

fn line_pairs(
    selection: &lamina_core::patch::LineSelection,
) -> Result<Vec<(u32, u32)>, UnsupportedOperation> {
    selection
        .iter()
        .map(|(hunk, line)| {
            Ok((
                u32::try_from(hunk).map_err(|_| UnsupportedOperation)?,
                u32::try_from(line).map_err(|_| UnsupportedOperation)?,
            ))
        })
        .collect()
}

fn line_selection(
    pairs: Vec<(u32, u32)>,
) -> Result<lamina_core::patch::LineSelection, ProtocolError> {
    let mut lines = Vec::with_capacity(pairs.len());
    for (hunk, line) in pairs {
        let hunk = usize::try_from(hunk)
            .map_err(|_| ProtocolError::Malformed("hunk index does not fit"))?;
        let line = usize::try_from(line)
            .map_err(|_| ProtocolError::Malformed("line index does not fit"))?;
        lines.push((hunk, line));
    }
    Ok(lamina_core::patch::LineSelection::new(lines))
}

fn stash_index(index: u64) -> Result<usize, ProtocolError> {
    usize::try_from(index).map_err(|_| ProtocolError::Malformed("stash index does not fit"))
}

#[cfg(test)]
mod tests {
    use lamina_core::merge::ContentHash;
    use lamina_core::patch::LineSelection;
    use lamina_core::session::{
        AcrossMergesAction, ConflictSide, Operation, RemoteBranch, ResetMode,
    };
    use lamina_core::{ReflogEntry, ReflogKind};

    use super::*;

    fn oid() -> lamina_core::Oid {
        lamina_core::Oid::from_bytes_or_panic(&[7u8; 20])
    }

    fn assert_roundtrip(operation: &Operation) {
        let dto = OperationDto::try_from(operation).expect("supported");
        let bytes = postcard::to_allocvec(&dto).expect("encode");
        let decoded: OperationDto = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, dto);
        let restored = Operation::try_from(decoded).expect("restore");
        assert_eq!(format!("{restored:?}"), format!("{operation:?}"));
    }

    #[test]
    fn supported_operations_round_trip() {
        let selection = LineSelection::new([(1usize, 2usize), (0, 4)]);
        let commit = oid();
        let operations = vec![
            Operation::StagePaths(vec!["a".into(), "b".into()]),
            Operation::UnstagePaths(vec!["a".into()]),
            Operation::DiscardPaths(vec!["a".into()]),
            Operation::DiscardStagedPaths {
                paths: vec!["a".into()],
            },
            Operation::DeleteUntracked(vec!["tmp".into()]),
            Operation::StageLines {
                path: "a".into(),
                selection: selection.clone(),
            },
            Operation::UnstageLines {
                path: "a".into(),
                selection: selection.clone(),
            },
            Operation::DiscardLines {
                path: "a".into(),
                selection,
            },
            Operation::Commit {
                message: "subject\n\nbody".into(),
                amend: true,
            },
            Operation::Checkout {
                branch: "feature".into(),
            },
            Operation::CheckoutCommit { commit },
            Operation::CreateBranch {
                name: "feature".into(),
                start_point: Some("main".into()),
                checkout: true,
            },
            Operation::DeleteBranch {
                name: "feature".into(),
                force: true,
            },
            Operation::RenameBranch {
                name: "old".into(),
                new_name: "new".into(),
                rename_upstream: Some(RemoteBranch {
                    remote: "origin".into(),
                    branch: "old".into(),
                }),
            },
            Operation::SetBranchUpstream {
                branch: "main".into(),
                remote: "origin".into(),
                remote_branch: "main".into(),
            },
            Operation::UnsetBranchUpstream {
                branch: "main".into(),
            },
            Operation::Fetch {
                remote: Some("origin".into()),
                prune: true,
            },
            Operation::FetchUnshallow,
            Operation::Pull {
                rebase: Some(false),
            },
            Operation::Push {
                remote: Some("origin".into()),
                branch: Some("main".into()),
                set_upstream: true,
                force_with_lease: true,
            },
            Operation::StashPush {
                message: Some("wip".into()),
                include_untracked: true,
            },
            Operation::StashPushPaths {
                message: None,
                include_untracked: false,
                paths: vec!["a".into()],
            },
            Operation::StashPop { index: 2 },
            Operation::StashApply { index: 0 },
            Operation::StashDrop { index: 1 },
            Operation::Merge {
                branch: "feature".into(),
                no_ff: true,
            },
            Operation::MergeAbort,
            Operation::ResolveConflict {
                path: "a".into(),
                side: ConflictSide::Theirs,
            },
            Operation::MarkResolved(vec!["a".into()]),
            Operation::MergeContinue,
            Operation::ResolveWithContent {
                path: "a".into(),
                content: b"resolved".to_vec(),
                expected: Some(ContentHash::of(b"old")),
            },
            Operation::CherryPick { commit },
            Operation::CherryPickMany {
                commits: vec![commit, oid()],
            },
            Operation::Revert { commit },
            Operation::Rebase {
                onto: commit,
                upstream: Some(commit),
                autostash: true,
            },
            Operation::Reset {
                target: commit,
                mode: ResetMode::Hard,
            },
            Operation::Reset {
                target: commit,
                mode: ResetMode::Soft,
            },
            Operation::Reset {
                target: commit,
                mode: ResetMode::Mixed,
            },
            Operation::Reset {
                target: commit,
                mode: ResetMode::Keep,
            },
            Operation::ContinueInProgress,
            Operation::AbortInProgress,
            Operation::UndoReflog { entry: None },
            Operation::UndoReflog {
                entry: Some(ReflogEntry {
                    reference: "HEAD".into(),
                    old_id: commit,
                    new_id: oid(),
                    committer_name: "Lamina Test".into(),
                    committer_email: "lamina@example.invalid".into(),
                    time: 1_700_000_000,
                    message: "commit: second".into(),
                    kind: ReflogKind::Commit,
                }),
            },
            Operation::RedoReflog {
                entry: ReflogEntry {
                    reference: "HEAD".into(),
                    old_id: commit,
                    new_id: oid(),
                    committer_name: "Lamina Test".into(),
                    committer_email: "lamina@example.invalid".into(),
                    time: 1_700_000_000,
                    message: "commit: second".into(),
                    kind: ReflogKind::Commit,
                },
            },
            Operation::SubmoduleUpdate {
                paths: vec!["libs/ui".into()],
                init: true,
            },
            Operation::SubmoduleUpdate {
                paths: Vec::new(),
                init: false,
            },
            Operation::WorktreeAdd {
                path: std::path::PathBuf::from("/tmp/repo-feature"),
                branch: Some("feature".into()),
                new_branch: None,
                commit: None,
            },
            Operation::WorktreeAdd {
                path: std::path::PathBuf::from("/tmp/repo-topic"),
                branch: None,
                new_branch: Some("topic".into()),
                commit: Some(commit),
            },
            Operation::WorktreeRemove {
                path: std::path::PathBuf::from("/tmp/repo-feature"),
                force: true,
            },
            Operation::WorktreePrune,
            Operation::LfsTrack {
                path: "*.bin".into(),
            },
            Operation::LfsLock {
                path: "data.bin".into(),
            },
            Operation::LfsUnlock {
                path: "dir/data.bin".into(),
            },
            Operation::LfsPull {
                path: "data.bin".into(),
            },
        ];
        for operation in &operations {
            assert!(is_supported_operation(operation), "{operation:?}");
            assert_roundtrip(operation);
        }
    }

    #[test]
    fn excluded_operations_stay_off_the_wire() {
        let commit = oid();
        let excluded = vec![
            Operation::AddRemote {
                name: "origin".into(),
                url: "https://example.invalid/repo.git".into(),
                fetch: false,
            },
            Operation::OpenMergetool { path: "a".into() },
            Operation::ApplyPatch { patch: vec![1] },
            Operation::CreateTag {
                name: "v1".into(),
                target: commit,
                message: None,
            },
            Operation::DeleteTag { name: "v1".into() },
            Operation::RewordAcrossMerges {
                commit,
                message: "reworded".into(),
            },
            Operation::RewriteAcrossMerges {
                commit,
                action: AcrossMergesAction::Drop,
            },
            Operation::WriteCommitGraph,
        ];
        for operation in &excluded {
            assert!(
                OperationDto::try_from(operation).is_err(),
                "{operation:?} must stay unsupported"
            );
            assert!(!is_supported_operation(operation));
        }
    }

    #[test]
    #[cfg(target_pointer_width = "32")]
    fn a_stash_index_that_does_not_fit_is_rejected() {
        let dto = OperationDto::StashPop { index: u64::MAX };
        let error = Operation::try_from(dto).expect_err("usize cannot hold u64::MAX");
        assert!(matches!(error, ProtocolError::Malformed(_)), "{error}");
    }
}
