//! Ticketed queries the helper runs on its own session.
//!
//! The client's ticket stays on [`crate::Event::Query`]. The body is only the
//! inputs or the result, so a streaming batch and the event that ends the walk
//! share one id without repeating it inside every payload.
//!
//! Paths are the helper's strings. Nothing here canonicalises them: a remote
//! path is not a path on the machine that decoded the frame.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::ChangeKindDto;
use super::repo::{CommitDto, HeadDto, ObjectId, oid};
use crate::codec::ProtocolError;

/// `usize` counts travel as `u64` so a 32-bit and a 64-bit peer agree on the
/// frame. A value that does not fit the local pointer width is a bad frame,
/// not a silently truncated length.
pub(crate) fn wire_count(value: usize) -> u64 {
    value as u64
}

pub(crate) fn host_count(value: u64) -> Result<usize, ProtocolError> {
    usize::try_from(value).map_err(|_| ProtocolError::Malformed("integer does not fit in usize"))
}

fn wire_count_opt(value: Option<usize>) -> Option<u64> {
    value.map(wire_count)
}

fn host_count_opt(value: Option<u64>) -> Result<Option<usize>, ProtocolError> {
    value.map(host_count).transpose()
}

fn oid_opt(value: Option<ObjectId>) -> Result<Option<lamina_core::Oid>, ProtocolError> {
    value.map(oid).transpose()
}

fn oid_pairs<T, U>(
    map: &HashMap<lamina_core::Oid, T>,
    convert: impl Fn(&T) -> U,
) -> Vec<(ObjectId, U)> {
    let mut pairs = map
        .iter()
        .map(|(id, value)| (ObjectId::from(*id), convert(value)))
        .collect::<Vec<_>>();
    // HashMap iteration order is not part of the protocol. Sorting by the raw
    // id makes two peers encode the same set as the same bytes.
    pairs.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    pairs
}

fn map_from_pairs<T, U>(
    pairs: Vec<(ObjectId, T)>,
    convert: impl Fn(T) -> Result<U, ProtocolError>,
) -> Result<HashMap<lamina_core::Oid, U>, ProtocolError> {
    let mut map = HashMap::with_capacity(pairs.len());
    for (id, value) in pairs {
        map.insert(oid(id)?, convert(value)?);
    }
    Ok(map)
}

fn encoding_label(encoding: lamina_core::encoding::TextEncoding) -> String {
    encoding.label().to_owned()
}

fn encoding_from_label(label: &str) -> Result<lamina_core::encoding::TextEncoding, ProtocolError> {
    lamina_core::encoding::TextEncoding::from_label(label)
        .ok_or(ProtocolError::Malformed("unknown text encoding"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitQueryDto {
    pub text: String,
    pub in_summary: bool,
    pub in_author: bool,
    pub in_hash: bool,
}

impl From<&lamina_core::session::CommitQuery> for CommitQueryDto {
    fn from(query: &lamina_core::session::CommitQuery) -> Self {
        Self {
            text: query.text.clone(),
            in_summary: query.in_summary,
            in_author: query.in_author,
            in_hash: query.in_hash,
        }
    }
}

impl From<CommitQueryDto> for lamina_core::session::CommitQuery {
    fn from(query: CommitQueryDto) -> Self {
        Self {
            text: query.text,
            in_summary: query.in_summary,
            in_author: query.in_author,
            in_hash: query.in_hash,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathHistoryCommitDto {
    pub commit: CommitDto,
    pub path: String,
}

impl From<&lamina_core::session::PathHistoryCommit> for PathHistoryCommitDto {
    fn from(entry: &lamina_core::session::PathHistoryCommit) -> Self {
        Self {
            commit: CommitDto::from(&entry.commit),
            path: entry.path.clone(),
        }
    }
}

impl TryFrom<PathHistoryCommitDto> for lamina_core::session::PathHistoryCommit {
    type Error = ProtocolError;

    fn try_from(entry: PathHistoryCommitDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            commit: entry.commit.try_into()?,
            path: entry.path,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStatDto {
    pub path: String,
    pub old_path: Option<String>,
    pub added: u64,
    pub removed: u64,
    pub binary: bool,
}

impl From<&lamina_core::FileStat> for FileStatDto {
    fn from(stat: &lamina_core::FileStat) -> Self {
        Self {
            path: stat.path.clone(),
            old_path: stat.old_path.clone(),
            added: stat.added,
            removed: stat.removed,
            binary: stat.binary,
        }
    }
}

impl From<FileStatDto> for lamina_core::FileStat {
    fn from(stat: FileStatDto) -> Self {
        Self {
            path: stat.path,
            old_path: stat.old_path,
            added: stat.added,
            removed: stat.removed,
            binary: stat.binary,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitStatDto {
    pub files: Vec<FileStatDto>,
    pub added: u64,
    pub removed: u64,
}

impl From<&lamina_core::CommitStat> for CommitStatDto {
    fn from(stat: &lamina_core::CommitStat) -> Self {
        Self {
            files: stat.files.iter().map(FileStatDto::from).collect(),
            added: stat.added,
            removed: stat.removed,
        }
    }
}

impl From<CommitStatDto> for lamina_core::CommitStat {
    fn from(stat: CommitStatDto) -> Self {
        Self {
            files: stat.files.into_iter().map(FileStatDto::into).collect(),
            added: stat.added,
            removed: stat.removed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareModeDto {
    TwoDot,
    ThreeDot,
}

impl From<lamina_core::repo::CompareMode> for CompareModeDto {
    fn from(mode: lamina_core::repo::CompareMode) -> Self {
        match mode {
            lamina_core::repo::CompareMode::TwoDot => Self::TwoDot,
            lamina_core::repo::CompareMode::ThreeDot => Self::ThreeDot,
        }
    }
}

impl From<CompareModeDto> for lamina_core::repo::CompareMode {
    fn from(mode: CompareModeDto) -> Self {
        match mode {
            CompareModeDto::TwoDot => Self::TwoDot,
            CompareModeDto::ThreeDot => Self::ThreeDot,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareFileDto {
    pub stat: FileStatDto,
    pub kind: ChangeKindDto,
}

impl From<&lamina_core::repo::CompareFile> for CompareFileDto {
    fn from(file: &lamina_core::repo::CompareFile) -> Self {
        Self {
            stat: FileStatDto::from(&file.stat),
            kind: ChangeKindDto::from(file.kind),
        }
    }
}

impl From<CompareFileDto> for lamina_core::repo::CompareFile {
    fn from(file: CompareFileDto) -> Self {
        Self {
            stat: file.stat.into(),
            kind: file.kind.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonDto {
    pub base: Option<ObjectId>,
    pub head: ObjectId,
    pub merge_base: Option<ObjectId>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<CompareFileDto>,
    pub added: u64,
    pub removed: u64,
}

impl From<&lamina_core::repo::Comparison> for ComparisonDto {
    fn from(comparison: &lamina_core::repo::Comparison) -> Self {
        Self {
            base: comparison.base.map(ObjectId::from),
            head: ObjectId::from(comparison.head),
            merge_base: comparison.merge_base.map(ObjectId::from),
            ahead: comparison.ahead,
            behind: comparison.behind,
            files: comparison.files.iter().map(CompareFileDto::from).collect(),
            added: comparison.added,
            removed: comparison.removed,
        }
    }
}

impl TryFrom<ComparisonDto> for lamina_core::repo::Comparison {
    type Error = ProtocolError;

    fn try_from(comparison: ComparisonDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            base: oid_opt(comparison.base)?,
            head: oid(comparison.head)?,
            merge_base: oid_opt(comparison.merge_base)?,
            ahead: comparison.ahead,
            behind: comparison.behind,
            files: comparison.files.into_iter().map(Into::into).collect(),
            added: comparison.added,
            removed: comparison.removed,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameHunkDto {
    pub commit: ObjectId,
    pub orig_path: String,
    pub orig_start: u32,
    pub start: u32,
    pub len: u32,
}

impl From<&lamina_core::repo::BlameHunk> for BlameHunkDto {
    fn from(hunk: &lamina_core::repo::BlameHunk) -> Self {
        Self {
            commit: ObjectId::from(hunk.commit),
            orig_path: hunk.orig_path.clone(),
            orig_start: hunk.orig_start,
            start: hunk.start,
            len: hunk.len,
        }
    }
}

impl TryFrom<BlameHunkDto> for lamina_core::repo::BlameHunk {
    type Error = ProtocolError;

    fn try_from(hunk: BlameHunkDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            commit: oid(hunk.commit)?,
            orig_path: hunk.orig_path,
            orig_start: hunk.orig_start,
            start: hunk.start,
            len: hunk.len,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameCommitDto {
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub summary: String,
}

impl From<&lamina_core::repo::BlameCommit> for BlameCommitDto {
    fn from(commit: &lamina_core::repo::BlameCommit) -> Self {
        Self {
            author_name: commit.author_name.clone(),
            author_email: commit.author_email.clone(),
            author_time: commit.author_time,
            summary: commit.summary.clone(),
        }
    }
}

impl From<BlameCommitDto> for lamina_core::repo::BlameCommit {
    fn from(commit: BlameCommitDto) -> Self {
        Self {
            author_name: commit.author_name,
            author_email: commit.author_email,
            author_time: commit.author_time,
            summary: commit.summary,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameDto {
    pub hunks: Vec<BlameHunkDto>,
    pub commits: Vec<(ObjectId, BlameCommitDto)>,
    pub lines: Vec<String>,
    /// [`lamina_core::encoding::TextEncoding::label`]. An unknown label is a
    /// malformed frame: guessing UTF-8 would mislabel the file the view shows.
    pub encoding: String,
}

impl From<&lamina_core::repo::Blame> for BlameDto {
    fn from(blame: &lamina_core::repo::Blame) -> Self {
        Self {
            hunks: blame.hunks.iter().map(BlameHunkDto::from).collect(),
            commits: oid_pairs(&blame.commits, |commit| BlameCommitDto::from(commit)),
            lines: blame.lines.clone(),
            encoding: encoding_label(blame.encoding),
        }
    }
}

impl TryFrom<BlameDto> for lamina_core::repo::Blame {
    type Error = ProtocolError;

    fn try_from(blame: BlameDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            hunks: blame
                .hunks
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            commits: map_from_pairs(blame.commits, |commit| Ok(commit.into()))?,
            lines: blame.lines,
            encoding: encoding_from_label(&blame.encoding)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentSearchModeDto {
    Substring(String),
    Regex(String),
}

impl From<&lamina_core::repo::ContentSearchMode> for ContentSearchModeDto {
    fn from(mode: &lamina_core::repo::ContentSearchMode) -> Self {
        match mode {
            lamina_core::repo::ContentSearchMode::Substring(text) => Self::Substring(text.clone()),
            lamina_core::repo::ContentSearchMode::Regex(text) => Self::Regex(text.clone()),
        }
    }
}

impl From<ContentSearchModeDto> for lamina_core::repo::ContentSearchMode {
    fn from(mode: ContentSearchModeDto) -> Self {
        match mode {
            ContentSearchModeDto::Substring(text) => Self::Substring(text),
            ContentSearchModeDto::Regex(text) => Self::Regex(text),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSearchQueryDto {
    pub mode: ContentSearchModeDto,
    pub path: Option<String>,
}

impl From<&lamina_core::repo::ContentSearchQuery> for ContentSearchQueryDto {
    fn from(query: &lamina_core::repo::ContentSearchQuery) -> Self {
        Self {
            mode: ContentSearchModeDto::from(&query.mode),
            path: query.path.clone(),
        }
    }
}

impl From<ContentSearchQueryDto> for lamina_core::repo::ContentSearchQuery {
    fn from(query: ContentSearchQueryDto) -> Self {
        Self {
            mode: query.mode.into(),
            path: query.path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSearchEntryDto {
    pub commit: ObjectId,
    pub matched_path: Option<String>,
}

impl From<&lamina_core::repo::ContentSearchEntry> for ContentSearchEntryDto {
    fn from(entry: &lamina_core::repo::ContentSearchEntry) -> Self {
        Self {
            commit: ObjectId::from(entry.commit),
            matched_path: entry.matched_path.clone(),
        }
    }
}

impl TryFrom<ContentSearchEntryDto> for lamina_core::repo::ContentSearchEntry {
    type Error = ProtocolError;

    fn try_from(entry: ContentSearchEntryDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            commit: oid(entry.commit)?,
            matched_path: entry.matched_path,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReflogKindDto {
    Commit,
    Amend,
    Checkout,
    Reset,
    Reword,
    RebaseStart,
    RebaseFinish,
    Merge,
    CherryPick,
    Pull,
    BranchCreate,
    BranchRename,
    Other,
}

impl From<lamina_core::ReflogKind> for ReflogKindDto {
    fn from(kind: lamina_core::ReflogKind) -> Self {
        match kind {
            lamina_core::ReflogKind::Commit => Self::Commit,
            lamina_core::ReflogKind::Amend => Self::Amend,
            lamina_core::ReflogKind::Checkout => Self::Checkout,
            lamina_core::ReflogKind::Reset => Self::Reset,
            lamina_core::ReflogKind::Reword => Self::Reword,
            lamina_core::ReflogKind::RebaseStart => Self::RebaseStart,
            lamina_core::ReflogKind::RebaseFinish => Self::RebaseFinish,
            lamina_core::ReflogKind::Merge => Self::Merge,
            lamina_core::ReflogKind::CherryPick => Self::CherryPick,
            lamina_core::ReflogKind::Pull => Self::Pull,
            lamina_core::ReflogKind::BranchCreate => Self::BranchCreate,
            lamina_core::ReflogKind::BranchRename => Self::BranchRename,
            lamina_core::ReflogKind::Other => Self::Other,
        }
    }
}

impl From<ReflogKindDto> for lamina_core::ReflogKind {
    fn from(kind: ReflogKindDto) -> Self {
        match kind {
            ReflogKindDto::Commit => Self::Commit,
            ReflogKindDto::Amend => Self::Amend,
            ReflogKindDto::Checkout => Self::Checkout,
            ReflogKindDto::Reset => Self::Reset,
            ReflogKindDto::Reword => Self::Reword,
            ReflogKindDto::RebaseStart => Self::RebaseStart,
            ReflogKindDto::RebaseFinish => Self::RebaseFinish,
            ReflogKindDto::Merge => Self::Merge,
            ReflogKindDto::CherryPick => Self::CherryPick,
            ReflogKindDto::Pull => Self::Pull,
            ReflogKindDto::BranchCreate => Self::BranchCreate,
            ReflogKindDto::BranchRename => Self::BranchRename,
            ReflogKindDto::Other => Self::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflogEntryDto {
    pub reference: String,
    pub old_id: ObjectId,
    pub new_id: ObjectId,
    pub committer_name: String,
    pub committer_email: String,
    pub time: i64,
    pub message: String,
    pub kind: ReflogKindDto,
}

impl From<&lamina_core::ReflogEntry> for ReflogEntryDto {
    fn from(entry: &lamina_core::ReflogEntry) -> Self {
        Self {
            reference: entry.reference.clone(),
            old_id: ObjectId::from(entry.old_id),
            new_id: ObjectId::from(entry.new_id),
            committer_name: entry.committer_name.clone(),
            committer_email: entry.committer_email.clone(),
            time: entry.time,
            message: entry.message.clone(),
            kind: ReflogKindDto::from(entry.kind),
        }
    }
}

impl TryFrom<ReflogEntryDto> for lamina_core::ReflogEntry {
    type Error = ProtocolError;

    fn try_from(entry: ReflogEntryDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            reference: entry.reference,
            old_id: oid(entry.old_id)?,
            new_id: oid(entry.new_id)?,
            committer_name: entry.committer_name,
            committer_email: entry.committer_email,
            time: entry.time,
            message: entry.message,
            kind: entry.kind.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InProgressCommitDto {
    pub id: Option<ObjectId>,
    pub short_id: String,
    pub summary: Option<String>,
}

impl From<&lamina_core::repo::InProgressCommit> for InProgressCommitDto {
    fn from(commit: &lamina_core::repo::InProgressCommit) -> Self {
        Self {
            id: commit.id.map(ObjectId::from),
            short_id: commit.short_id.clone(),
            summary: commit.summary.clone(),
        }
    }
}

impl TryFrom<InProgressCommitDto> for lamina_core::repo::InProgressCommit {
    type Error = ProtocolError;

    fn try_from(commit: InProgressCommitDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            id: oid_opt(commit.id)?,
            short_id: commit.short_id,
            summary: commit.summary,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebaseOntoDto {
    pub id: Option<ObjectId>,
    pub short_id: String,
    pub ref_name: Option<String>,
}

impl From<&lamina_core::repo::RebaseOnto> for RebaseOntoDto {
    fn from(onto: &lamina_core::repo::RebaseOnto) -> Self {
        Self {
            id: onto.id.map(ObjectId::from),
            short_id: onto.short_id.clone(),
            ref_name: onto.ref_name.clone(),
        }
    }
}

impl TryFrom<RebaseOntoDto> for lamina_core::repo::RebaseOnto {
    type Error = ProtocolError;

    fn try_from(onto: RebaseOntoDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            id: oid_opt(onto.id)?,
            short_id: onto.short_id,
            ref_name: onto.ref_name,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InProgressDetailsDto {
    Rebase {
        branch: Option<String>,
        onto: Option<RebaseOntoDto>,
        step: Option<u64>,
        total_steps: Option<u64>,
        current_commit: Option<InProgressCommitDto>,
    },
    Merge {
        heads: Vec<InProgressCommitDto>,
        source: Option<String>,
    },
    CherryPick {
        commit: Option<InProgressCommitDto>,
        remaining: Option<u64>,
    },
    Revert {
        commit: Option<InProgressCommitDto>,
        remaining: Option<u64>,
    },
    Bisect {
        good_count: Option<u64>,
        bad_count: Option<u64>,
        current_commit: Option<InProgressCommitDto>,
    },
    ApplyingPatches {
        step: Option<u64>,
        total_steps: Option<u64>,
        subject: Option<String>,
    },
}

impl From<&lamina_core::repo::InProgressDetails> for InProgressDetailsDto {
    fn from(details: &lamina_core::repo::InProgressDetails) -> Self {
        use lamina_core::repo::InProgressDetails;
        match details {
            InProgressDetails::Rebase {
                branch,
                onto,
                step,
                total_steps,
                current_commit,
            } => Self::Rebase {
                branch: branch.clone(),
                onto: onto.as_ref().map(RebaseOntoDto::from),
                step: wire_count_opt(*step),
                total_steps: wire_count_opt(*total_steps),
                current_commit: current_commit.as_ref().map(InProgressCommitDto::from),
            },
            InProgressDetails::Merge { heads, source } => Self::Merge {
                heads: heads.iter().map(InProgressCommitDto::from).collect(),
                source: source.clone(),
            },
            InProgressDetails::CherryPick { commit, remaining } => Self::CherryPick {
                commit: commit.as_ref().map(InProgressCommitDto::from),
                remaining: wire_count_opt(*remaining),
            },
            InProgressDetails::Revert { commit, remaining } => Self::Revert {
                commit: commit.as_ref().map(InProgressCommitDto::from),
                remaining: wire_count_opt(*remaining),
            },
            InProgressDetails::Bisect {
                good_count,
                bad_count,
                current_commit,
            } => Self::Bisect {
                good_count: wire_count_opt(*good_count),
                bad_count: wire_count_opt(*bad_count),
                current_commit: current_commit.as_ref().map(InProgressCommitDto::from),
            },
            InProgressDetails::ApplyingPatches {
                step,
                total_steps,
                subject,
            } => Self::ApplyingPatches {
                step: wire_count_opt(*step),
                total_steps: wire_count_opt(*total_steps),
                subject: subject.clone(),
            },
        }
    }
}

impl TryFrom<InProgressDetailsDto> for lamina_core::repo::InProgressDetails {
    type Error = ProtocolError;

    fn try_from(details: InProgressDetailsDto) -> Result<Self, ProtocolError> {
        Ok(match details {
            InProgressDetailsDto::Rebase {
                branch,
                onto,
                step,
                total_steps,
                current_commit,
            } => Self::Rebase {
                branch,
                onto: onto.map(TryInto::try_into).transpose()?,
                step: host_count_opt(step)?,
                total_steps: host_count_opt(total_steps)?,
                current_commit: current_commit.map(TryInto::try_into).transpose()?,
            },
            InProgressDetailsDto::Merge { heads, source } => Self::Merge {
                heads: heads
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
                source,
            },
            InProgressDetailsDto::CherryPick { commit, remaining } => Self::CherryPick {
                commit: commit.map(TryInto::try_into).transpose()?,
                remaining: host_count_opt(remaining)?,
            },
            InProgressDetailsDto::Revert { commit, remaining } => Self::Revert {
                commit: commit.map(TryInto::try_into).transpose()?,
                remaining: host_count_opt(remaining)?,
            },
            InProgressDetailsDto::Bisect {
                good_count,
                bad_count,
                current_commit,
            } => Self::Bisect {
                good_count: host_count_opt(good_count)?,
                bad_count: host_count_opt(bad_count)?,
                current_commit: current_commit.map(TryInto::try_into).transpose()?,
            },
            InProgressDetailsDto::ApplyingPatches {
                step,
                total_steps,
                subject,
            } => Self::ApplyingPatches {
                step: host_count_opt(step)?,
                total_steps: host_count_opt(total_steps)?,
                subject,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmoduleStatusDto {
    Uninitialised,
    UpToDate,
    CommitDiffers,
    Modified,
}

impl From<lamina_core::repo::SubmoduleStatus> for SubmoduleStatusDto {
    fn from(status: lamina_core::repo::SubmoduleStatus) -> Self {
        match status {
            lamina_core::repo::SubmoduleStatus::Uninitialised => Self::Uninitialised,
            lamina_core::repo::SubmoduleStatus::UpToDate => Self::UpToDate,
            lamina_core::repo::SubmoduleStatus::CommitDiffers => Self::CommitDiffers,
            lamina_core::repo::SubmoduleStatus::Modified => Self::Modified,
        }
    }
}

impl From<SubmoduleStatusDto> for lamina_core::repo::SubmoduleStatus {
    fn from(status: SubmoduleStatusDto) -> Self {
        match status {
            SubmoduleStatusDto::Uninitialised => Self::Uninitialised,
            SubmoduleStatusDto::UpToDate => Self::UpToDate,
            SubmoduleStatusDto::CommitDiffers => Self::CommitDiffers,
            SubmoduleStatusDto::Modified => Self::Modified,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmoduleInfoDto {
    pub name: String,
    pub path: String,
    pub url: Option<String>,
    pub recorded_commit: Option<ObjectId>,
    pub checked_out_commit: Option<ObjectId>,
    pub status: SubmoduleStatusDto,
}

impl From<&lamina_core::repo::SubmoduleInfo> for SubmoduleInfoDto {
    fn from(info: &lamina_core::repo::SubmoduleInfo) -> Self {
        Self {
            name: info.name.clone(),
            path: info.path.clone(),
            url: info.url.clone(),
            recorded_commit: info.recorded_commit.map(ObjectId::from),
            checked_out_commit: info.checked_out_commit.map(ObjectId::from),
            status: SubmoduleStatusDto::from(info.status),
        }
    }
}

impl TryFrom<SubmoduleInfoDto> for lamina_core::repo::SubmoduleInfo {
    type Error = ProtocolError;

    fn try_from(info: SubmoduleInfoDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            name: info.name,
            path: info.path,
            url: info.url,
            recorded_commit: oid_opt(info.recorded_commit)?,
            checked_out_commit: oid_opt(info.checked_out_commit)?,
            status: info.status.into(),
        })
    }
}

/// One worktree, as the helper listed it.
///
/// `path` is the checkout on the helper's machine. It is an opaque string:
/// `PathBuf::from` on the client does not touch the local filesystem, and
/// callers must not canonicalise or stat it either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfoDto {
    pub path: String,
    pub head: Option<HeadDto>,
    pub is_main: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
}

impl From<&lamina_core::repo::WorktreeInfo> for WorktreeInfoDto {
    fn from(info: &lamina_core::repo::WorktreeInfo) -> Self {
        Self {
            path: info.path.to_string_lossy().into_owned(),
            head: info.head.as_ref().map(HeadDto::from),
            is_main: info.is_main,
            locked: info.locked,
            lock_reason: info.lock_reason.clone(),
            prunable: info.prunable,
        }
    }
}

impl TryFrom<WorktreeInfoDto> for lamina_core::repo::WorktreeInfo {
    type Error = ProtocolError;

    fn try_from(info: WorktreeInfoDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            // No canonicalise: this names a checkout that is not on this machine.
            path: PathBuf::from(info.path),
            head: info.head.map(TryInto::try_into).transpose()?,
            is_main: info.is_main,
            locked: info.locked,
            lock_reason: info.lock_reason,
            prunable: info.prunable,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureFormatDto {
    OpenPgp,
    Ssh,
    X509,
    Unknown,
}

impl From<lamina_core::repo::SignatureFormat> for SignatureFormatDto {
    fn from(format: lamina_core::repo::SignatureFormat) -> Self {
        match format {
            lamina_core::repo::SignatureFormat::OpenPgp => Self::OpenPgp,
            lamina_core::repo::SignatureFormat::Ssh => Self::Ssh,
            lamina_core::repo::SignatureFormat::X509 => Self::X509,
            lamina_core::repo::SignatureFormat::Unknown => Self::Unknown,
        }
    }
}

impl From<SignatureFormatDto> for lamina_core::repo::SignatureFormat {
    fn from(format: SignatureFormatDto) -> Self {
        match format {
            SignatureFormatDto::OpenPgp => Self::OpenPgp,
            SignatureFormatDto::Ssh => Self::Ssh,
            SignatureFormatDto::X509 => Self::X509,
            SignatureFormatDto::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureValidityDto {
    Good,
    Bad,
    UnknownValidity,
    Expired,
    ExpiredKey,
    RevokedKey,
    CannotCheck,
    None,
}

impl From<lamina_core::repo::SignatureValidity> for SignatureValidityDto {
    fn from(validity: lamina_core::repo::SignatureValidity) -> Self {
        match validity {
            lamina_core::repo::SignatureValidity::Good => Self::Good,
            lamina_core::repo::SignatureValidity::Bad => Self::Bad,
            lamina_core::repo::SignatureValidity::UnknownValidity => Self::UnknownValidity,
            lamina_core::repo::SignatureValidity::Expired => Self::Expired,
            lamina_core::repo::SignatureValidity::ExpiredKey => Self::ExpiredKey,
            lamina_core::repo::SignatureValidity::RevokedKey => Self::RevokedKey,
            lamina_core::repo::SignatureValidity::CannotCheck => Self::CannotCheck,
            lamina_core::repo::SignatureValidity::None => Self::None,
        }
    }
}

impl From<SignatureValidityDto> for lamina_core::repo::SignatureValidity {
    fn from(validity: SignatureValidityDto) -> Self {
        match validity {
            SignatureValidityDto::Good => Self::Good,
            SignatureValidityDto::Bad => Self::Bad,
            SignatureValidityDto::UnknownValidity => Self::UnknownValidity,
            SignatureValidityDto::Expired => Self::Expired,
            SignatureValidityDto::ExpiredKey => Self::ExpiredKey,
            SignatureValidityDto::RevokedKey => Self::RevokedKey,
            SignatureValidityDto::CannotCheck => Self::CannotCheck,
            SignatureValidityDto::None => Self::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSignatureDto {
    pub format: SignatureFormatDto,
    pub validity: SignatureValidityDto,
    pub signer: Option<String>,
    pub key: Option<String>,
    pub fingerprint: Option<String>,
    pub primary_key_fingerprint: Option<String>,
    pub trust_level: Option<String>,
    pub message: String,
    pub missing_hint: Option<String>,
}

impl From<&lamina_core::repo::CommitSignature> for CommitSignatureDto {
    fn from(signature: &lamina_core::repo::CommitSignature) -> Self {
        Self {
            format: SignatureFormatDto::from(signature.format),
            validity: SignatureValidityDto::from(signature.validity),
            signer: signature.signer.clone(),
            key: signature.key.clone(),
            fingerprint: signature.fingerprint.clone(),
            primary_key_fingerprint: signature.primary_key_fingerprint.clone(),
            trust_level: signature.trust_level.clone(),
            message: signature.message.clone(),
            missing_hint: signature.missing_hint.clone(),
        }
    }
}

impl From<CommitSignatureDto> for lamina_core::repo::CommitSignature {
    fn from(signature: CommitSignatureDto) -> Self {
        Self {
            format: signature.format.into(),
            validity: signature.validity.into(),
            signer: signature.signer,
            key: signature.key,
            fingerprint: signature.fingerprint,
            primary_key_fingerprint: signature.primary_key_fingerprint,
            trust_level: signature.trust_level,
            message: signature.message,
            missing_hint: signature.missing_hint,
        }
    }
}

/// What the client asks the helper's session to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryRequest {
    Search(CommitQueryDto),
    Path {
        path: String,
    },
    StreamingPath {
        path: String,
    },
    CommitStat(ObjectId),
    Compare {
        a: ObjectId,
        b: ObjectId,
        mode: CompareModeDto,
    },
    CompareFrom {
        base: Option<ObjectId>,
        head: ObjectId,
    },
    CompareWorktree {
        base: ObjectId,
    },
    SubmodulesAndWorktrees,
    Reflog {
        limit: u64,
    },
    InProgressDetails,
    Blame {
        commit: ObjectId,
        path: String,
    },
    ContentSearch(ContentSearchQueryDto),
    Signatures {
        commits: Vec<ObjectId>,
    },
}

/// One result from a ticketed query. The ticket lives on the enclosing event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryEvent {
    Ready {
        generation: u64,
        rows: Vec<u64>,
    },
    CommitStat(CommitStatDto),
    Compare(ComparisonDto),
    BlameProgress {
        lines: Option<Vec<String>>,
        hunks: Vec<BlameHunkDto>,
        commits: Vec<(ObjectId, BlameCommitDto)>,
    },
    BlameReady(BlameDto),
    Failed {
        error: String,
    },
    PathHistoryBatch {
        entries: Vec<PathHistoryCommitDto>,
    },
    PathHistoryDone,
    ContentSearchBatch {
        matches: Vec<ContentSearchEntryDto>,
        scanned: u64,
        total: u64,
    },
    ContentSearchProgress {
        scanned: u64,
        total: u64,
    },
    ContentSearchDone {
        scanned: u64,
        total: u64,
    },
    SubmodulesAndWorktrees {
        submodules: Vec<SubmoduleInfoDto>,
        worktrees: Vec<WorktreeInfoDto>,
    },
    Reflog {
        entries: Vec<ReflogEntryDto>,
    },
    InProgress(Option<InProgressDetailsDto>),
    SignaturesDetected {
        formats: Vec<(ObjectId, SignatureFormatDto)>,
    },
    SignaturesReady {
        signatures: Vec<(ObjectId, CommitSignatureDto)>,
    },
}

impl QueryEvent {
    /// A later event for this ticket must be ignored. Streaming batches are
    /// not terminal: the route stays until the walk says it is finished.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Ready { .. }
                | Self::CommitStat(_)
                | Self::Compare(_)
                | Self::BlameReady(_)
                | Self::Failed { .. }
                | Self::PathHistoryDone
                | Self::ContentSearchDone { .. }
                | Self::SubmodulesAndWorktrees { .. }
                | Self::Reflog { .. }
                | Self::InProgress(_)
                | Self::SignaturesReady { .. }
        )
    }

    /// Rebuilds the session event the helper observed, tagged with the
    /// client's ticket rather than the helper's.
    pub fn into_session(
        self,
        ticket: lamina_core::session::QueryTicket,
    ) -> Result<lamina_core::session::SessionEvent, ProtocolError> {
        use lamina_core::session::SessionEvent;
        match self {
            Self::Ready { generation, rows } => Ok(SessionEvent::QueryReady {
                ticket,
                generation,
                rows: Arc::new(
                    rows.into_iter()
                        .map(host_count)
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            }),
            Self::CommitStat(stat) => Ok(SessionEvent::CommitStatReady {
                ticket,
                stat: Arc::new(stat.into()),
            }),
            Self::Compare(comparison) => Ok(SessionEvent::CompareReady {
                ticket,
                comparison: Arc::new(comparison.try_into()?),
            }),
            Self::BlameProgress {
                lines,
                hunks,
                commits,
            } => Ok(SessionEvent::BlameProgress {
                ticket,
                lines: lines.map(Arc::new),
                hunks: hunks
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
                commits: map_from_pairs(commits, |commit| Ok(commit.into()))?,
            }),
            Self::BlameReady(blame) => Ok(SessionEvent::BlameReady {
                ticket,
                blame: Arc::new(blame.try_into()?),
            }),
            Self::Failed { error } => Ok(SessionEvent::QueryFailed { ticket, error }),
            Self::PathHistoryBatch { entries } => Ok(SessionEvent::PathHistoryBatch {
                ticket,
                entries: Arc::new(
                    entries
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, _>>()?,
                ),
            }),
            Self::PathHistoryDone => Ok(SessionEvent::PathHistoryDone { ticket }),
            Self::ContentSearchBatch {
                matches,
                scanned,
                total,
            } => Ok(SessionEvent::ContentSearchBatch {
                ticket,
                matches: Arc::new(
                    matches
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, _>>()?,
                ),
                scanned: host_count(scanned)?,
                total: host_count(total)?,
            }),
            Self::ContentSearchProgress { scanned, total } => {
                Ok(SessionEvent::ContentSearchProgress {
                    ticket,
                    scanned: host_count(scanned)?,
                    total: host_count(total)?,
                })
            }
            Self::ContentSearchDone { scanned, total } => Ok(SessionEvent::ContentSearchDone {
                ticket,
                scanned: host_count(scanned)?,
                total: host_count(total)?,
            }),
            Self::SubmodulesAndWorktrees {
                submodules,
                worktrees,
            } => Ok(SessionEvent::SubmodulesAndWorktreesReady {
                ticket,
                submodules: Arc::new(
                    submodules
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, _>>()?,
                ),
                worktrees: Arc::new(
                    worktrees
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, _>>()?,
                ),
            }),
            Self::Reflog { entries } => Ok(SessionEvent::ReflogReady {
                ticket,
                entries: Arc::new(
                    entries
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, _>>()?,
                ),
            }),
            Self::InProgress(details) => Ok(SessionEvent::InProgressDetailsReady {
                ticket,
                details: details.map(TryInto::try_into).transpose()?,
            }),
            Self::SignaturesDetected { formats } => Ok(SessionEvent::CommitSignaturesDetected {
                ticket,
                formats: Arc::new(map_from_pairs(formats, |format| Ok(format.into()))?),
            }),
            Self::SignaturesReady { signatures } => Ok(SessionEvent::CommitSignaturesReady {
                ticket,
                signatures: Arc::new(map_from_pairs(
                    signatures,
                    |signature| Ok(signature.into()),
                )?),
            }),
        }
    }
}

/// The ticketed part of a [`lamina_core::session::SessionEvent`].
///
/// `None` for snapshots, diffs, mutations and the events that stay on the
/// helper (command log, commit-graph offer). The returned ticket is the
/// helper's; the server rewrites it to the client's before sending.
pub fn query_from_session(
    event: lamina_core::session::SessionEvent,
) -> Option<(lamina_core::session::QueryTicket, QueryEvent)> {
    use lamina_core::session::SessionEvent;
    Some(match event {
        SessionEvent::QueryReady {
            ticket,
            generation,
            rows,
        } => (
            ticket,
            QueryEvent::Ready {
                generation,
                rows: rows.iter().copied().map(wire_count).collect(),
            },
        ),
        SessionEvent::CommitStatReady { ticket, stat } => {
            (ticket, QueryEvent::CommitStat(CommitStatDto::from(&*stat)))
        }
        SessionEvent::CompareReady { ticket, comparison } => (
            ticket,
            QueryEvent::Compare(ComparisonDto::from(&*comparison)),
        ),
        SessionEvent::BlameProgress {
            ticket,
            lines,
            hunks,
            commits,
        } => (
            ticket,
            QueryEvent::BlameProgress {
                lines: lines.as_ref().map(|lines| (**lines).clone()),
                hunks: hunks.iter().map(BlameHunkDto::from).collect(),
                commits: oid_pairs(&commits, |commit| BlameCommitDto::from(commit)),
            },
        ),
        SessionEvent::BlameReady { ticket, blame } => {
            (ticket, QueryEvent::BlameReady(BlameDto::from(&*blame)))
        }
        SessionEvent::QueryFailed { ticket, error } => (ticket, QueryEvent::Failed { error }),
        SessionEvent::PathHistoryBatch { ticket, entries } => (
            ticket,
            QueryEvent::PathHistoryBatch {
                entries: entries.iter().map(PathHistoryCommitDto::from).collect(),
            },
        ),
        SessionEvent::PathHistoryDone { ticket } => (ticket, QueryEvent::PathHistoryDone),
        SessionEvent::ContentSearchBatch {
            ticket,
            matches,
            scanned,
            total,
        } => (
            ticket,
            QueryEvent::ContentSearchBatch {
                matches: matches.iter().map(ContentSearchEntryDto::from).collect(),
                scanned: wire_count(scanned),
                total: wire_count(total),
            },
        ),
        SessionEvent::ContentSearchProgress {
            ticket,
            scanned,
            total,
        } => (
            ticket,
            QueryEvent::ContentSearchProgress {
                scanned: wire_count(scanned),
                total: wire_count(total),
            },
        ),
        SessionEvent::ContentSearchDone {
            ticket,
            scanned,
            total,
        } => (
            ticket,
            QueryEvent::ContentSearchDone {
                scanned: wire_count(scanned),
                total: wire_count(total),
            },
        ),
        SessionEvent::SubmodulesAndWorktreesReady {
            ticket,
            submodules,
            worktrees,
        } => (
            ticket,
            QueryEvent::SubmodulesAndWorktrees {
                submodules: submodules.iter().map(SubmoduleInfoDto::from).collect(),
                worktrees: worktrees.iter().map(WorktreeInfoDto::from).collect(),
            },
        ),
        SessionEvent::ReflogReady { ticket, entries } => (
            ticket,
            QueryEvent::Reflog {
                entries: entries.iter().map(ReflogEntryDto::from).collect(),
            },
        ),
        SessionEvent::InProgressDetailsReady { ticket, details } => (
            ticket,
            QueryEvent::InProgress(details.as_ref().map(InProgressDetailsDto::from)),
        ),
        SessionEvent::CommitSignaturesDetected { ticket, formats } => (
            ticket,
            QueryEvent::SignaturesDetected {
                formats: oid_pairs(&formats, |format| SignatureFormatDto::from(*format)),
            },
        ),
        SessionEvent::CommitSignaturesReady { ticket, signatures } => (
            ticket,
            QueryEvent::SignaturesReady {
                signatures: oid_pairs(&signatures, |signature| CommitSignatureDto::from(signature)),
            },
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lamina_core::session::{CommitQuery, PathHistoryCommit, QueryTicket, SessionEvent};
    use lamina_core::{CommitInfo, CommitStat, FileStat, Oid, ReflogEntry, ReflogKind};
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    use super::*;

    fn oid_byte(byte: u8) -> Oid {
        Oid::from_bytes_or_panic(&[byte; 20])
    }

    fn wire_oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes([byte; 20])
    }

    fn commit(byte: u8) -> CommitInfo {
        CommitInfo {
            id: oid_byte(byte),
            parents: vec![oid_byte(byte.wrapping_add(1))],
            summary: "subject".into(),
            author_name: "Author".into(),
            author_email: "author@example.invalid".into(),
            author_time: 1_700_000_000,
            committer_time: 1_700_000_100,
            shallow_boundary: false,
        }
    }

    fn roundtrip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let bytes = postcard::to_allocvec(value).expect("encode");
        let decoded: T = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(&decoded, value);
    }

    fn assert_event_roundtrip(event: SessionEvent) {
        let (ticket, body) = query_from_session(event).expect("query event");
        let terminal = body.is_terminal();
        roundtrip(&body);
        let restored = body.into_session(ticket).expect("session event");
        let (again, body) = query_from_session(restored).expect("still a query");
        assert_eq!(again, ticket);
        assert_eq!(body.is_terminal(), terminal);
        let _ = body.into_session(again).expect("second pass");
    }

    #[test]
    fn query_payloads_round_trip() {
        roundtrip(&CommitQueryDto {
            text: "parser".into(),
            in_summary: true,
            in_author: false,
            in_hash: true,
        });
        roundtrip(&QueryRequest::Blame {
            commit: wire_oid(1),
            path: "src/lib.rs".into(),
        });
        roundtrip(&QueryRequest::Reflog { limit: 50 });
        roundtrip(&QueryRequest::ContentSearch(ContentSearchQueryDto {
            mode: ContentSearchModeDto::Regex("fn ".into()),
            path: Some("src".into()),
        }));
        roundtrip(&QueryRequest::Signatures {
            commits: vec![wire_oid(2), wire_oid(3)],
        });
        roundtrip(&QueryEvent::PathHistoryDone);
        roundtrip(&QueryEvent::Failed {
            error: "missing".into(),
        });
        roundtrip(&QueryEvent::InProgress(Some(InProgressDetailsDto::Merge {
            heads: vec![InProgressCommitDto {
                id: Some(wire_oid(4)),
                short_id: "abcd".into(),
                summary: Some("merge".into()),
            }],
            source: Some("feature".into()),
        })));
    }

    #[test]
    fn session_query_events_survive_the_wire() {
        let query = CommitQuery {
            text: "fix".into(),
            in_summary: true,
            in_author: true,
            in_hash: false,
        };
        let decoded: CommitQuery = CommitQueryDto::from(&query).into();
        assert_eq!(decoded, query);

        assert_event_roundtrip(SessionEvent::QueryReady {
            ticket: QueryTicket(4),
            generation: 2,
            rows: Arc::new(vec![0, 3, 8]),
        });
        assert_event_roundtrip(SessionEvent::CommitStatReady {
            ticket: QueryTicket(5),
            stat: Arc::new(CommitStat {
                files: vec![FileStat {
                    path: "README.md".into(),
                    old_path: None,
                    added: 3,
                    removed: 1,
                    binary: false,
                }],
                added: 3,
                removed: 1,
            }),
        });
        assert_event_roundtrip(SessionEvent::CompareReady {
            ticket: QueryTicket(6),
            comparison: Arc::new(lamina_core::repo::Comparison {
                base: Some(oid_byte(1)),
                head: oid_byte(2),
                merge_base: Some(oid_byte(1)),
                ahead: 2,
                behind: 0,
                files: vec![lamina_core::repo::CompareFile {
                    stat: FileStat {
                        path: "src/lib.rs".into(),
                        old_path: Some("src/old.rs".into()),
                        added: 4,
                        removed: 1,
                        binary: false,
                    },
                    kind: lamina_core::ChangeKind::Renamed,
                }],
                added: 4,
                removed: 1,
            }),
        });
        let mut blame_commits = HashMap::new();
        blame_commits.insert(
            oid_byte(7),
            lamina_core::repo::BlameCommit {
                author_name: "Author".into(),
                author_email: "author@example.invalid".into(),
                author_time: 10,
                summary: "introduce it".into(),
            },
        );
        assert_event_roundtrip(SessionEvent::BlameProgress {
            ticket: QueryTicket(8),
            lines: Some(Arc::new(vec!["fn main() {}".into()])),
            hunks: vec![lamina_core::repo::BlameHunk {
                commit: oid_byte(7),
                orig_path: "src/lib.rs".into(),
                orig_start: 1,
                start: 1,
                len: 1,
            }],
            commits: blame_commits.clone(),
        });
        assert_event_roundtrip(SessionEvent::BlameReady {
            ticket: QueryTicket(9),
            blame: Arc::new(lamina_core::repo::Blame {
                hunks: vec![lamina_core::repo::BlameHunk {
                    commit: oid_byte(7),
                    orig_path: "src/lib.rs".into(),
                    orig_start: 1,
                    start: 1,
                    len: 1,
                }],
                commits: blame_commits,
                lines: vec!["fn main() {}".into()],
                encoding: lamina_core::encoding::TextEncoding::from_label("UTF-8").unwrap(),
            }),
        });
        assert_event_roundtrip(SessionEvent::QueryFailed {
            ticket: QueryTicket(10),
            error: "not a file".into(),
        });
        assert_event_roundtrip(SessionEvent::PathHistoryBatch {
            ticket: QueryTicket(11),
            entries: Arc::new(vec![PathHistoryCommit {
                commit: commit(12),
                path: "README.md".into(),
            }]),
        });
        assert_event_roundtrip(SessionEvent::PathHistoryDone {
            ticket: QueryTicket(11),
        });
        assert!(
            !QueryEvent::PathHistoryBatch {
                entries: Vec::new()
            }
            .is_terminal()
        );
        assert!(QueryEvent::PathHistoryDone.is_terminal());

        assert_event_roundtrip(SessionEvent::ContentSearchBatch {
            ticket: QueryTicket(12),
            matches: Arc::new(vec![lamina_core::repo::ContentSearchEntry {
                commit: oid_byte(13),
                matched_path: Some("src/lib.rs".into()),
            }]),
            scanned: 4,
            total: 9,
        });
        assert_event_roundtrip(SessionEvent::ContentSearchProgress {
            ticket: QueryTicket(12),
            scanned: 5,
            total: 9,
        });
        assert_event_roundtrip(SessionEvent::ContentSearchDone {
            ticket: QueryTicket(12),
            scanned: 9,
            total: 9,
        });
        assert_event_roundtrip(SessionEvent::SubmodulesAndWorktreesReady {
            ticket: QueryTicket(13),
            submodules: Arc::new(vec![lamina_core::repo::SubmoduleInfo {
                name: "libs/parser".into(),
                path: "libs/parser".into(),
                url: Some("../parser.git".into()),
                recorded_commit: Some(oid_byte(14)),
                checked_out_commit: None,
                status: lamina_core::repo::SubmoduleStatus::Uninitialised,
            }]),
            worktrees: Arc::new(vec![lamina_core::repo::WorktreeInfo {
                path: PathBuf::from("/srv/repo"),
                head: Some(lamina_core::HeadInfo::Detached {
                    target: oid_byte(15),
                }),
                is_main: true,
                locked: false,
                lock_reason: None,
                prunable: false,
            }]),
        });
        assert_event_roundtrip(SessionEvent::ReflogReady {
            ticket: QueryTicket(14),
            entries: Arc::new(vec![ReflogEntry {
                reference: "HEAD".into(),
                old_id: oid_byte(1),
                new_id: oid_byte(2),
                committer_name: "Author".into(),
                committer_email: "author@example.invalid".into(),
                time: 20,
                message: "commit: subject".into(),
                kind: ReflogKind::Commit,
            }]),
        });
        assert_event_roundtrip(SessionEvent::InProgressDetailsReady {
            ticket: QueryTicket(15),
            details: Some(lamina_core::repo::InProgressDetails::Rebase {
                branch: Some("feature".into()),
                onto: Some(lamina_core::repo::RebaseOnto {
                    id: Some(oid_byte(3)),
                    short_id: "abc1234".into(),
                    ref_name: Some("main".into()),
                }),
                step: Some(2),
                total_steps: Some(5),
                current_commit: Some(lamina_core::repo::InProgressCommit {
                    id: Some(oid_byte(4)),
                    short_id: "def5678".into(),
                    summary: Some("wip".into()),
                }),
            }),
        });
        assert_event_roundtrip(SessionEvent::InProgressDetailsReady {
            ticket: QueryTicket(16),
            details: None,
        });
        let mut formats = HashMap::new();
        formats.insert(oid_byte(9), lamina_core::repo::SignatureFormat::Ssh);
        assert_event_roundtrip(SessionEvent::CommitSignaturesDetected {
            ticket: QueryTicket(17),
            formats: Arc::new(formats),
        });
        let mut signatures = HashMap::new();
        signatures.insert(
            oid_byte(9),
            lamina_core::repo::CommitSignature {
                format: lamina_core::repo::SignatureFormat::Ssh,
                validity: lamina_core::repo::SignatureValidity::Good,
                signer: Some("Author".into()),
                key: Some("SHA256:abc".into()),
                fingerprint: Some("SHA256:abc".into()),
                primary_key_fingerprint: None,
                trust_level: Some("ultimate".into()),
                message: "Good signature".into(),
                missing_hint: None,
            },
        );
        assert_event_roundtrip(SessionEvent::CommitSignaturesReady {
            ticket: QueryTicket(17),
            signatures: Arc::new(signatures),
        });
    }

    #[test]
    fn an_unknown_blame_encoding_is_rejected() {
        let event = QueryEvent::BlameReady(BlameDto {
            hunks: Vec::new(),
            commits: Vec::new(),
            lines: Vec::new(),
            encoding: "not-a-real-encoding".into(),
        });
        let error = event
            .into_session(QueryTicket(1))
            .expect_err("unknown encoding");
        assert!(error.to_string().contains("encoding"), "{error}");
    }

    #[test]
    fn a_worktree_path_is_not_resolved_locally() {
        let info = lamina_core::repo::WorktreeInfo {
            path: PathBuf::from("/no/such/checkout"),
            head: None,
            is_main: false,
            locked: true,
            lock_reason: Some("busy".into()),
            prunable: true,
        };
        let decoded: lamina_core::repo::WorktreeInfo =
            WorktreeInfoDto::from(&info).try_into().expect("convert");
        assert_eq!(decoded.path, PathBuf::from("/no/such/checkout"));
        assert_eq!(decoded, info);
    }

    #[test]
    fn snapshots_are_not_query_events() {
        assert!(query_from_session(SessionEvent::SignaturesInvalidated).is_none());
        assert!(
            query_from_session(SessionEvent::LoadFailed {
                error: "gone".into()
            })
            .is_none()
        );
    }
}
