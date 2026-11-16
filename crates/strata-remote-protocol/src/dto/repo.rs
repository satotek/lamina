//! DTOs mirroring the repository data `strata-core` publishes.
//!
//! Conversions exist in both directions. Core -> DTO is infallible because
//! every core value is valid on the wire. DTO -> core is fallible for the
//! types that carry an [`ObjectId`], because a corrupted frame must be an
//! error rather than a silently truncated hash.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::codec::ProtocolError;

/// A raw object id. SHA-1 repositories are the only kind `strata-core` reads,
/// so the wire form is exactly 20 bytes instead of a hex string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObjectId([u8; 20]);

impl ObjectId {
    pub fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl From<strata_core::Oid> for ObjectId {
    fn from(oid: strata_core::Oid) -> Self {
        Self(
            oid.as_slice()
                .try_into()
                .expect("strata-core reads SHA-1 object ids"),
        )
    }
}

impl TryFrom<ObjectId> for strata_core::Oid {
    type Error = ProtocolError;

    fn try_from(value: ObjectId) -> Result<Self, ProtocolError> {
        Self::try_from(value.0.as_slice())
            .map_err(|_| ProtocolError::Malformed("object id is not 20 bytes"))
    }
}

pub(crate) fn oid(value: ObjectId) -> Result<strata_core::Oid, ProtocolError> {
    value.try_into()
}

fn oids(values: Vec<ObjectId>) -> Result<Vec<strata_core::Oid>, ProtocolError> {
    values.into_iter().map(oid).collect()
}

/// One commit, as the log renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDto {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
    pub summary: String,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub committer_time: i64,
    pub shallow_boundary: bool,
}

impl From<&strata_core::CommitInfo> for CommitDto {
    fn from(commit: &strata_core::CommitInfo) -> Self {
        Self {
            id: commit.id.into(),
            parents: commit.parents.iter().copied().map(ObjectId::from).collect(),
            summary: commit.summary.clone(),
            author_name: commit.author_name.clone(),
            author_email: commit.author_email.clone(),
            author_time: commit.author_time,
            committer_time: commit.committer_time,
            shallow_boundary: commit.shallow_boundary,
        }
    }
}

impl TryFrom<CommitDto> for strata_core::CommitInfo {
    type Error = ProtocolError;

    fn try_from(commit: CommitDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            id: oid(commit.id)?,
            parents: oids(commit.parents)?,
            summary: commit.summary,
            author_name: commit.author_name,
            author_email: commit.author_email,
            author_time: commit.author_time,
            committer_time: commit.committer_time,
            shallow_boundary: commit.shallow_boundary,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKindDto {
    LocalBranch,
    RemoteBranch,
    Tag,
}

impl From<strata_core::RefKind> for RefKindDto {
    fn from(kind: strata_core::RefKind) -> Self {
        match kind {
            strata_core::RefKind::LocalBranch => Self::LocalBranch,
            strata_core::RefKind::RemoteBranch => Self::RemoteBranch,
            strata_core::RefKind::Tag => Self::Tag,
        }
    }
}

impl From<RefKindDto> for strata_core::RefKind {
    fn from(kind: RefKindDto) -> Self {
        match kind {
            RefKindDto::LocalBranch => Self::LocalBranch,
            RefKindDto::RemoteBranch => Self::RemoteBranch,
            RefKindDto::Tag => Self::Tag,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefInfoDto {
    pub full_name: String,
    pub short_name: String,
    pub kind: RefKindDto,
    pub target: ObjectId,
}

impl From<&strata_core::RefInfo> for RefInfoDto {
    fn from(reference: &strata_core::RefInfo) -> Self {
        Self {
            full_name: reference.full_name.clone(),
            short_name: reference.short_name.clone(),
            kind: reference.kind.into(),
            target: reference.target.into(),
        }
    }
}

impl TryFrom<RefInfoDto> for strata_core::RefInfo {
    type Error = ProtocolError;

    fn try_from(reference: RefInfoDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            full_name: reference.full_name,
            short_name: reference.short_name,
            kind: reference.kind.into(),
            target: oid(reference.target)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeadDto {
    /// `target` is `None` on an unborn branch.
    Branch {
        short_name: String,
        target: Option<ObjectId>,
    },
    Detached {
        target: ObjectId,
    },
}

impl From<&strata_core::HeadInfo> for HeadDto {
    fn from(head: &strata_core::HeadInfo) -> Self {
        match head {
            strata_core::HeadInfo::Branch { short_name, target } => Self::Branch {
                short_name: short_name.clone(),
                target: target.map(ObjectId::from),
            },
            strata_core::HeadInfo::Detached { target } => Self::Detached {
                target: ObjectId::from(*target),
            },
        }
    }
}

impl TryFrom<HeadDto> for strata_core::HeadInfo {
    type Error = ProtocolError;

    fn try_from(head: HeadDto) -> Result<Self, ProtocolError> {
        Ok(match head {
            HeadDto::Branch { short_name, target } => Self::Branch {
                short_name,
                target: target.map(oid).transpose()?,
            },
            HeadDto::Detached { target } => Self::Detached {
                target: oid(target)?,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKindDto {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

impl From<strata_core::ChangeKind> for ChangeKindDto {
    fn from(kind: strata_core::ChangeKind) -> Self {
        match kind {
            strata_core::ChangeKind::Added => Self::Added,
            strata_core::ChangeKind::Modified => Self::Modified,
            strata_core::ChangeKind::Deleted => Self::Deleted,
            strata_core::ChangeKind::Renamed => Self::Renamed,
            strata_core::ChangeKind::Copied => Self::Copied,
            strata_core::ChangeKind::TypeChanged => Self::TypeChanged,
            strata_core::ChangeKind::Untracked => Self::Untracked,
            strata_core::ChangeKind::Conflicted => Self::Conflicted,
        }
    }
}

impl From<ChangeKindDto> for strata_core::ChangeKind {
    fn from(kind: ChangeKindDto) -> Self {
        match kind {
            ChangeKindDto::Added => Self::Added,
            ChangeKindDto::Modified => Self::Modified,
            ChangeKindDto::Deleted => Self::Deleted,
            ChangeKindDto::Renamed => Self::Renamed,
            ChangeKindDto::Copied => Self::Copied,
            ChangeKindDto::TypeChanged => Self::TypeChanged,
            ChangeKindDto::Untracked => Self::Untracked,
            ChangeKindDto::Conflicted => Self::Conflicted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangeDto {
    pub path: String,
    pub old_path: Option<String>,
    pub kind: ChangeKindDto,
}

impl From<&strata_core::FileChange> for FileChangeDto {
    fn from(change: &strata_core::FileChange) -> Self {
        Self {
            path: change.path.clone(),
            old_path: change.old_path.clone(),
            kind: change.kind.into(),
        }
    }
}

impl From<FileChangeDto> for strata_core::FileChange {
    fn from(change: FileChangeDto) -> Self {
        Self {
            path: change.path,
            old_path: change.old_path,
            kind: change.kind.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreeEntryKindDto {
    File,
    Directory,
    Symlink,
    Submodule,
}

impl From<strata_core::TreeEntryKind> for TreeEntryKindDto {
    fn from(kind: strata_core::TreeEntryKind) -> Self {
        match kind {
            strata_core::TreeEntryKind::File => Self::File,
            strata_core::TreeEntryKind::Directory => Self::Directory,
            strata_core::TreeEntryKind::Symlink => Self::Symlink,
            strata_core::TreeEntryKind::Submodule => Self::Submodule,
        }
    }
}

impl From<TreeEntryKindDto> for strata_core::TreeEntryKind {
    fn from(kind: TreeEntryKindDto) -> Self {
        match kind {
            TreeEntryKindDto::File => Self::File,
            TreeEntryKindDto::Directory => Self::Directory,
            TreeEntryKindDto::Symlink => Self::Symlink,
            TreeEntryKindDto::Submodule => Self::Submodule,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntryDto {
    pub name: String,
    pub path: String,
    pub kind: TreeEntryKindDto,
    pub mode: u16,
    pub size: Option<u64>,
}

impl From<&strata_core::TreeEntry> for TreeEntryDto {
    fn from(entry: &strata_core::TreeEntry) -> Self {
        Self {
            name: entry.name.clone(),
            path: entry.path.clone(),
            kind: entry.kind.into(),
            mode: entry.mode,
            size: entry.size,
        }
    }
}

impl From<TreeEntryDto> for strata_core::TreeEntry {
    fn from(entry: TreeEntryDto) -> Self {
        Self {
            name: entry.name,
            path: entry.path,
            kind: entry.kind.into(),
            mode: entry.mode,
            size: entry.size,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatusDto {
    pub staged: Vec<FileChangeDto>,
    pub unstaged: Vec<FileChangeDto>,
}

impl From<&strata_core::WorktreeStatus> for WorktreeStatusDto {
    fn from(status: &strata_core::WorktreeStatus) -> Self {
        Self {
            staged: status.staged.iter().map(FileChangeDto::from).collect(),
            unstaged: status.unstaged.iter().map(FileChangeDto::from).collect(),
        }
    }
}

impl From<WorktreeStatusDto> for strata_core::WorktreeStatus {
    fn from(status: WorktreeStatusDto) -> Self {
        Self {
            staged: status.staged.into_iter().map(Into::into).collect(),
            unstaged: status.unstaged.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashInfoDto {
    pub index: usize,
    pub id: ObjectId,
    pub message: String,
    pub created_time: i64,
    pub branch: Option<String>,
}

impl From<&strata_core::StashInfo> for StashInfoDto {
    fn from(stash: &strata_core::StashInfo) -> Self {
        Self {
            index: stash.index,
            id: stash.id.into(),
            message: stash.message.clone(),
            created_time: stash.created_time,
            branch: stash.branch.clone(),
        }
    }
}

impl TryFrom<StashInfoDto> for strata_core::StashInfo {
    type Error = ProtocolError;

    fn try_from(stash: StashInfoDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            index: stash.index,
            id: oid(stash.id)?,
            message: stash.message,
            created_time: stash.created_time,
            branch: stash.branch,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamInfoDto {
    pub name: String,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub gone: bool,
}

impl From<&strata_core::UpstreamInfo> for UpstreamInfoDto {
    fn from(upstream: &strata_core::UpstreamInfo) -> Self {
        Self {
            name: upstream.name.clone(),
            ahead: upstream.ahead,
            behind: upstream.behind,
            gone: upstream.gone,
        }
    }
}

impl From<UpstreamInfoDto> for strata_core::UpstreamInfo {
    fn from(upstream: UpstreamInfoDto) -> Self {
        Self {
            name: upstream.name,
            ahead: upstream.ahead,
            behind: upstream.behind,
            gone: upstream.gone,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteInfoDto {
    pub name: String,
    pub fetch_url: Option<String>,
    pub push_url: Option<String>,
}

impl From<&strata_core::RemoteInfo> for RemoteInfoDto {
    fn from(remote: &strata_core::RemoteInfo) -> Self {
        Self {
            name: remote.name.clone(),
            fetch_url: remote.fetch_url.clone(),
            push_url: remote.push_url.clone(),
        }
    }
}

impl From<RemoteInfoDto> for strata_core::RemoteInfo {
    fn from(remote: RemoteInfoDto) -> Self {
        Self {
            name: remote.name,
            fetch_url: remote.fetch_url,
            push_url: remote.push_url,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoStateDto {
    Normal,
    Merging,
    Rebasing,
    CherryPicking,
    Reverting,
    Bisecting,
    ApplyingPatches,
}

impl From<strata_core::RepoState> for RepoStateDto {
    fn from(state: strata_core::RepoState) -> Self {
        match state {
            strata_core::RepoState::Normal => Self::Normal,
            strata_core::RepoState::Merging => Self::Merging,
            strata_core::RepoState::Rebasing => Self::Rebasing,
            strata_core::RepoState::CherryPicking => Self::CherryPicking,
            strata_core::RepoState::Reverting => Self::Reverting,
            strata_core::RepoState::Bisecting => Self::Bisecting,
            strata_core::RepoState::ApplyingPatches => Self::ApplyingPatches,
        }
    }
}

impl From<RepoStateDto> for strata_core::RepoState {
    fn from(state: RepoStateDto) -> Self {
        match state {
            RepoStateDto::Normal => Self::Normal,
            RepoStateDto::Merging => Self::Merging,
            RepoStateDto::Rebasing => Self::Rebasing,
            RepoStateDto::CherryPicking => Self::CherryPicking,
            RepoStateDto::Reverting => Self::Reverting,
            RepoStateDto::Bisecting => Self::Bisecting,
            RepoStateDto::ApplyingPatches => Self::ApplyingPatches,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictKindDto {
    BothModified,
    BothAdded,
    BothDeleted,
    AddedByUs,
    AddedByThem,
    DeletedByUs,
    DeletedByThem,
}

impl From<strata_core::ConflictKind> for ConflictKindDto {
    fn from(kind: strata_core::ConflictKind) -> Self {
        match kind {
            strata_core::ConflictKind::BothModified => Self::BothModified,
            strata_core::ConflictKind::BothAdded => Self::BothAdded,
            strata_core::ConflictKind::BothDeleted => Self::BothDeleted,
            strata_core::ConflictKind::AddedByUs => Self::AddedByUs,
            strata_core::ConflictKind::AddedByThem => Self::AddedByThem,
            strata_core::ConflictKind::DeletedByUs => Self::DeletedByUs,
            strata_core::ConflictKind::DeletedByThem => Self::DeletedByThem,
        }
    }
}

impl From<ConflictKindDto> for strata_core::ConflictKind {
    fn from(kind: ConflictKindDto) -> Self {
        match kind {
            ConflictKindDto::BothModified => Self::BothModified,
            ConflictKindDto::BothAdded => Self::BothAdded,
            ConflictKindDto::BothDeleted => Self::BothDeleted,
            ConflictKindDto::AddedByUs => Self::AddedByUs,
            ConflictKindDto::AddedByThem => Self::AddedByThem,
            ConflictKindDto::DeletedByUs => Self::DeletedByUs,
            ConflictKindDto::DeletedByThem => Self::DeletedByThem,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictEntryDto {
    pub path: String,
    pub kind: ConflictKindDto,
}

impl From<&strata_core::ConflictEntry> for ConflictEntryDto {
    fn from(conflict: &strata_core::ConflictEntry) -> Self {
        Self {
            path: conflict.path.clone(),
            kind: conflict.kind.into(),
        }
    }
}

impl From<ConflictEntryDto> for strata_core::ConflictEntry {
    fn from(conflict: ConflictEntryDto) -> Self {
        Self {
            path: conflict.path,
            kind: conflict.kind.into(),
        }
    }
}

/// Everything a snapshot carries except the commits.
///
/// Commits are streamed separately so a large history is not one giant frame;
/// they also grow while the helper walks, and the commit list can be appended
/// without re-sending refs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDto {
    pub generation: u64,
    pub head: Option<HeadDto>,
    pub refs: Vec<RefInfoDto>,
    /// Sorted by name; `HashMap` iteration order must not leak onto the wire.
    pub upstreams: Vec<(String, UpstreamInfoDto)>,
    pub stashes: Vec<StashInfoDto>,
    pub remotes: Vec<RemoteInfoDto>,
    pub status: WorktreeStatusDto,
    pub state: RepoStateDto,
    pub conflicts: Vec<ConflictEntryDto>,
    pub busy: Option<u64>,
}

impl From<&strata_core::session::RepoSnapshot> for SnapshotDto {
    fn from(snapshot: &strata_core::session::RepoSnapshot) -> Self {
        let mut upstreams: Vec<(String, UpstreamInfoDto)> = snapshot
            .upstreams
            .iter()
            .map(|(name, upstream)| (name.clone(), UpstreamInfoDto::from(upstream)))
            .collect();
        upstreams.sort_by(|a, b| a.0.cmp(&b.0));
        Self {
            generation: snapshot.generation,
            head: snapshot.head.as_ref().map(HeadDto::from),
            refs: snapshot.refs.iter().map(RefInfoDto::from).collect(),
            upstreams,
            stashes: snapshot.stashes.iter().map(StashInfoDto::from).collect(),
            remotes: snapshot.remotes.iter().map(RemoteInfoDto::from).collect(),
            status: WorktreeStatusDto::from(&*snapshot.status),
            state: snapshot.state.into(),
            conflicts: snapshot
                .conflicts
                .iter()
                .map(ConflictEntryDto::from)
                .collect(),
            busy: snapshot.busy.map(|id| id.0),
        }
    }
}

impl TryFrom<SnapshotDto> for strata_core::session::RepoSnapshot {
    type Error = ProtocolError;

    fn try_from(snapshot: SnapshotDto) -> Result<Self, ProtocolError> {
        let refs: Vec<strata_core::RefInfo> = snapshot
            .refs
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<_, _>>()?;
        let mut refs_by_commit: HashMap<strata_core::Oid, Vec<usize>> =
            HashMap::with_capacity(refs.len());
        for (index, reference) in refs.iter().enumerate() {
            refs_by_commit
                .entry(reference.target)
                .or_default()
                .push(index);
        }
        let upstreams: HashMap<String, strata_core::UpstreamInfo> = snapshot
            .upstreams
            .into_iter()
            .map(|(name, upstream)| (name, upstream.into()))
            .collect();
        Ok(Self {
            generation: snapshot.generation,
            head: snapshot.head.map(TryInto::try_into).transpose()?,
            refs: std::sync::Arc::new(refs),
            refs_by_commit: std::sync::Arc::new(refs_by_commit),
            commits: strata_core::session::CommitList::default(),
            commit_index: std::sync::Arc::new(HashMap::new()),
            status: std::sync::Arc::new(snapshot.status.into()),
            stashes: std::sync::Arc::new(
                snapshot
                    .stashes
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
            ),
            upstreams: std::sync::Arc::new(upstreams),
            remotes: snapshot.remotes.into_iter().map(Into::into).collect(),
            state: snapshot.state.into(),
            conflicts: std::sync::Arc::new(
                snapshot.conflicts.into_iter().map(Into::into).collect(),
            ),
            busy: snapshot.busy.map(strata_core::session::OperationId),
        })
    }
}
