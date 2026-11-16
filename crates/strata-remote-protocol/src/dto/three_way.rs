//! Three-way comparison of commits, the index and the worktree.
//!
//! The helper builds the tree. The app only receives the rows it would have
//! computed locally, so a remote path never becomes a path on this machine.

use serde::{Deserialize, Serialize};

use super::query::{host_count, wire_count};
use super::repo::{ObjectId, oid};
use crate::codec::ProtocolError;

/// One side of a repository comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoCompareSideDto {
    Commit(ObjectId),
    Index,
    Worktree,
}

impl From<strata_core::compare::RepoCompareSide> for RepoCompareSideDto {
    fn from(side: strata_core::compare::RepoCompareSide) -> Self {
        match side {
            strata_core::compare::RepoCompareSide::Commit(commit) => Self::Commit(commit.into()),
            strata_core::compare::RepoCompareSide::Index => Self::Index,
            strata_core::compare::RepoCompareSide::Worktree => Self::Worktree,
        }
    }
}

impl TryFrom<RepoCompareSideDto> for strata_core::compare::RepoCompareSide {
    type Error = ProtocolError;

    fn try_from(side: RepoCompareSideDto) -> Result<Self, ProtocolError> {
        Ok(match side {
            RepoCompareSideDto::Commit(commit) => Self::Commit(oid(commit)?),
            RepoCompareSideDto::Index => Self::Index,
            RepoCompareSideDto::Worktree => Self::Worktree,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FolderEntryKindDto {
    File,
    Directory,
    Symlink,
    Other,
}

impl From<strata_core::compare::FolderEntryKind> for FolderEntryKindDto {
    fn from(kind: strata_core::compare::FolderEntryKind) -> Self {
        match kind {
            strata_core::compare::FolderEntryKind::File => Self::File,
            strata_core::compare::FolderEntryKind::Directory => Self::Directory,
            strata_core::compare::FolderEntryKind::Symlink => Self::Symlink,
            strata_core::compare::FolderEntryKind::Other => Self::Other,
        }
    }
}

impl From<FolderEntryKindDto> for strata_core::compare::FolderEntryKind {
    fn from(kind: FolderEntryKindDto) -> Self {
        match kind {
            FolderEntryKindDto::File => Self::File,
            FolderEntryKindDto::Directory => Self::Directory,
            FolderEntryKindDto::Symlink => Self::Symlink,
            FolderEntryKindDto::Other => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FolderEntryStatusDto {
    Identical,
    Different,
    OnlyLeft,
    OnlyMiddle,
    OnlyRight,
    TypeMismatch,
}

impl From<strata_core::compare::FolderEntryStatus> for FolderEntryStatusDto {
    fn from(status: strata_core::compare::FolderEntryStatus) -> Self {
        match status {
            strata_core::compare::FolderEntryStatus::Identical => Self::Identical,
            strata_core::compare::FolderEntryStatus::Different => Self::Different,
            strata_core::compare::FolderEntryStatus::OnlyLeft => Self::OnlyLeft,
            strata_core::compare::FolderEntryStatus::OnlyMiddle => Self::OnlyMiddle,
            strata_core::compare::FolderEntryStatus::OnlyRight => Self::OnlyRight,
            strata_core::compare::FolderEntryStatus::TypeMismatch => Self::TypeMismatch,
        }
    }
}

impl From<FolderEntryStatusDto> for strata_core::compare::FolderEntryStatus {
    fn from(status: FolderEntryStatusDto) -> Self {
        match status {
            FolderEntryStatusDto::Identical => Self::Identical,
            FolderEntryStatusDto::Different => Self::Different,
            FolderEntryStatusDto::OnlyLeft => Self::OnlyLeft,
            FolderEntryStatusDto::OnlyMiddle => Self::OnlyMiddle,
            FolderEntryStatusDto::OnlyRight => Self::OnlyRight,
            FolderEntryStatusDto::TypeMismatch => Self::TypeMismatch,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreeWayPeerStatusDto {
    Same,
    LeftMiddleDiffer,
    MiddleRightDiffer,
    AllDifferent,
    MissingSomeSide,
}

impl From<strata_core::compare::ThreeWayPeerStatus> for ThreeWayPeerStatusDto {
    fn from(status: strata_core::compare::ThreeWayPeerStatus) -> Self {
        match status {
            strata_core::compare::ThreeWayPeerStatus::Same => Self::Same,
            strata_core::compare::ThreeWayPeerStatus::LeftMiddleDiffer => Self::LeftMiddleDiffer,
            strata_core::compare::ThreeWayPeerStatus::MiddleRightDiffer => Self::MiddleRightDiffer,
            strata_core::compare::ThreeWayPeerStatus::AllDifferent => Self::AllDifferent,
            strata_core::compare::ThreeWayPeerStatus::MissingSomeSide => Self::MissingSomeSide,
        }
    }
}

impl From<ThreeWayPeerStatusDto> for strata_core::compare::ThreeWayPeerStatus {
    fn from(status: ThreeWayPeerStatusDto) -> Self {
        match status {
            ThreeWayPeerStatusDto::Same => Self::Same,
            ThreeWayPeerStatusDto::LeftMiddleDiffer => Self::LeftMiddleDiffer,
            ThreeWayPeerStatusDto::MiddleRightDiffer => Self::MiddleRightDiffer,
            ThreeWayPeerStatusDto::AllDifferent => Self::AllDifferent,
            ThreeWayPeerStatusDto::MissingSomeSide => Self::MissingSomeSide,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreeWayTreeChangeDto {
    LeftChanged,
    RightChanged,
    BothAgree,
    BothDisagree,
}

impl From<strata_core::compare::ThreeWayTreeChange> for ThreeWayTreeChangeDto {
    fn from(change: strata_core::compare::ThreeWayTreeChange) -> Self {
        match change {
            strata_core::compare::ThreeWayTreeChange::LeftChanged => Self::LeftChanged,
            strata_core::compare::ThreeWayTreeChange::RightChanged => Self::RightChanged,
            strata_core::compare::ThreeWayTreeChange::BothAgree => Self::BothAgree,
            strata_core::compare::ThreeWayTreeChange::BothDisagree => Self::BothDisagree,
        }
    }
}

impl From<ThreeWayTreeChangeDto> for strata_core::compare::ThreeWayTreeChange {
    fn from(change: ThreeWayTreeChangeDto) -> Self {
        match change {
            ThreeWayTreeChangeDto::LeftChanged => Self::LeftChanged,
            ThreeWayTreeChangeDto::RightChanged => Self::RightChanged,
            ThreeWayTreeChangeDto::BothAgree => Self::BothAgree,
            ThreeWayTreeChangeDto::BothDisagree => Self::BothDisagree,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSideInfoDto {
    pub kind: FolderEntryKindDto,
    pub size: Option<u64>,
}

impl From<&strata_core::compare::FolderSideInfo> for FolderSideInfoDto {
    fn from(info: &strata_core::compare::FolderSideInfo) -> Self {
        Self {
            kind: info.kind.into(),
            size: info.size,
        }
    }
}

impl From<FolderSideInfoDto> for strata_core::compare::FolderSideInfo {
    fn from(info: FolderSideInfoDto) -> Self {
        Self {
            kind: info.kind.into(),
            size: info.size,
        }
    }
}

/// `usize` counts travel as `u64` so a 32-bit helper and a 64-bit app agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderCompareCountsDto {
    pub total: u64,
    pub identical: u64,
    pub different: u64,
    pub only_left: u64,
    pub only_middle: u64,
    pub only_right: u64,
    pub type_mismatch: u64,
}

impl From<&strata_core::compare::FolderCompareCounts> for FolderCompareCountsDto {
    fn from(counts: &strata_core::compare::FolderCompareCounts) -> Self {
        Self {
            total: wire_count(counts.total),
            identical: wire_count(counts.identical),
            different: wire_count(counts.different),
            only_left: wire_count(counts.only_left),
            only_middle: wire_count(counts.only_middle),
            only_right: wire_count(counts.only_right),
            type_mismatch: wire_count(counts.type_mismatch),
        }
    }
}

impl TryFrom<FolderCompareCountsDto> for strata_core::compare::FolderCompareCounts {
    type Error = ProtocolError;

    fn try_from(counts: FolderCompareCountsDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            total: host_count(counts.total)?,
            identical: host_count(counts.identical)?,
            different: host_count(counts.different)?,
            only_left: host_count(counts.only_left)?,
            only_middle: host_count(counts.only_middle)?,
            only_right: host_count(counts.only_right)?,
            type_mismatch: host_count(counts.type_mismatch)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreeWayTreeEntryDto {
    pub relative_path: String,
    pub kind: FolderEntryKindDto,
    pub status: FolderEntryStatusDto,
    pub change: ThreeWayTreeChangeDto,
    pub peer: ThreeWayPeerStatusDto,
    pub conflicting: bool,
    pub sides: Vec<Option<FolderSideInfoDto>>,
}

impl From<&strata_core::compare::ThreeWayTreeEntry> for ThreeWayTreeEntryDto {
    fn from(entry: &strata_core::compare::ThreeWayTreeEntry) -> Self {
        Self {
            relative_path: entry.relative_path.clone(),
            kind: entry.kind.into(),
            status: entry.status.into(),
            change: entry.change.into(),
            peer: entry.peer.into(),
            conflicting: entry.conflicting,
            sides: entry
                .sides
                .iter()
                .map(|side| side.as_ref().map(FolderSideInfoDto::from))
                .collect(),
        }
    }
}

impl From<ThreeWayTreeEntryDto> for strata_core::compare::ThreeWayTreeEntry {
    fn from(entry: ThreeWayTreeEntryDto) -> Self {
        Self {
            relative_path: entry.relative_path,
            kind: entry.kind.into(),
            status: entry.status.into(),
            change: entry.change.into(),
            peer: entry.peer.into(),
            conflicting: entry.conflicting,
            sides: entry
                .sides
                .into_iter()
                .map(|side| side.map(Into::into))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreeWayTreeCompareDto {
    pub left: RepoCompareSideDto,
    pub base: RepoCompareSideDto,
    pub right: RepoCompareSideDto,
    pub entries: Vec<ThreeWayTreeEntryDto>,
    pub counts: FolderCompareCountsDto,
    pub merge_tree_checked: bool,
}

impl From<&strata_core::compare::ThreeWayTreeCompare> for ThreeWayTreeCompareDto {
    fn from(tree: &strata_core::compare::ThreeWayTreeCompare) -> Self {
        Self {
            left: tree.left.into(),
            base: tree.base.into(),
            right: tree.right.into(),
            entries: tree.entries.iter().map(Into::into).collect(),
            counts: FolderCompareCountsDto::from(&tree.counts),
            merge_tree_checked: tree.merge_tree_checked,
        }
    }
}

impl TryFrom<ThreeWayTreeCompareDto> for strata_core::compare::ThreeWayTreeCompare {
    type Error = ProtocolError;

    fn try_from(tree: ThreeWayTreeCompareDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            left: tree.left.try_into()?,
            base: tree.base.try_into()?,
            right: tree.right.try_into()?,
            entries: tree.entries.into_iter().map(Into::into).collect(),
            counts: tree.counts.try_into()?,
            merge_tree_checked: tree.merge_tree_checked,
        })
    }
}
