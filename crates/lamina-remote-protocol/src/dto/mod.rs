//! Wire DTOs.
//!
//! The types here mirror the `lamina-core` values the session publishes. They
//! are deliberately separate: deriving serde on the core types would freeze
//! their public shape into the protocol.

mod diff;
mod graph;
mod lfs;
mod operation;
mod query;
mod repo;
mod three_way;

pub use diff::{
    DiffLineDto, DiffOptionsDto, DiffRequestDto, DiffViewDto, FileDiffDto, GapDto,
    HighlightKindDto, HighlightSpanDto, HunkDto, ImageDiffDto, ImageFormatDto, ImageKindDto,
    ImageSideDto, LfsDiffSideDto, LfsDiffSummaryDto, LineKindDto, SpanDto,
};
pub use graph::{
    CommitChunkDto, GraphRowDto, HalfDto, PassThroughDto, SegmentDto, chunk_ids, rows_from_dto,
};
pub use lfs::{LfsInfoDto, LfsLockDto};
pub use operation::{
    ConflictSideDto, OperationDto, RemoteBranchDto, ResetModeDto, UnsupportedOperation,
    is_supported_operation,
};
pub use query::{
    BlameCommitDto, BlameDto, BlameHunkDto, CommitQueryDto, CommitSignatureDto, CommitStatDto,
    CompareFileDto, CompareModeDto, ComparisonDto, ContentSearchEntryDto, ContentSearchModeDto,
    ContentSearchQueryDto, FileStatDto, InProgressCommitDto, InProgressDetailsDto,
    PathHistoryCommitDto, QueryEvent, QueryRequest, RebaseOntoDto, ReflogEntryDto, ReflogKindDto,
    SignatureFormatDto, SignatureValidityDto, SubmoduleInfoDto, SubmoduleStatusDto,
    WorktreeInfoDto, query_from_session,
};
pub use repo::{
    ChangeKindDto, CommitDto, ConflictEntryDto, ConflictKindDto, ConflictStyleDto, FileChangeDto,
    HeadDto, ObjectId, RefInfoDto, RefKindDto, RemoteInfoDto, RepoStateDto, SnapshotDto,
    StashInfoDto, TreeEntryDto, TreeEntryKindDto, UpstreamInfoDto, WorktreeStatusDto,
};
pub use three_way::{
    FolderCompareCountsDto, FolderEntryKindDto, FolderEntryStatusDto, FolderSideInfoDto,
    RepoCompareSideDto, ThreeWayPeerStatusDto, ThreeWayTreeChangeDto, ThreeWayTreeCompareDto,
    ThreeWayTreeEntryDto,
};

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    use super::*;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes([byte; 20])
    }

    fn roundtrip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let bytes = postcard::to_allocvec(value).expect("encode");
        let decoded: T = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(&decoded, value);
        assert!(!bytes.is_empty());
    }

    #[test]
    fn every_message_dto_round_trips() {
        roundtrip(&oid(1));
        roundtrip(&RefKindDto::Tag);
        roundtrip(&ChangeKindDto::Copied);
        roundtrip(&TreeEntryKindDto::Submodule);
        roundtrip(&ConflictKindDto::DeletedByThem);
        roundtrip(&RepoStateDto::ApplyingPatches);
        roundtrip(&HalfDto::Bottom);
        roundtrip(&LineKindDto::Removed);
        roundtrip(&HighlightKindDto::Embedded);
        roundtrip(&ImageKindDto::Svg);
        roundtrip(&ImageFormatDto::Webp);

        roundtrip(&CommitDto {
            id: oid(2),
            parents: vec![oid(3), oid(4)],
            summary: "fix the parser".into(),
            author_name: "Author".into(),
            author_email: "author@example.invalid".into(),
            author_time: 1_700_000_000,
            committer_time: 1_700_000_100,
            shallow_boundary: true,
        });
        roundtrip(&RefInfoDto {
            full_name: "refs/heads/main".into(),
            short_name: "main".into(),
            kind: RefKindDto::LocalBranch,
            target: oid(5),
        });
        roundtrip(&HeadDto::Branch {
            short_name: "main".into(),
            target: Some(oid(6)),
        });
        roundtrip(&HeadDto::Detached { target: oid(7) });
        roundtrip(&FileChangeDto {
            path: "src/lib.rs".into(),
            old_path: Some("src/old.rs".into()),
            kind: ChangeKindDto::Renamed,
        });
        roundtrip(&TreeEntryDto {
            name: "lib.rs".into(),
            path: "src/lib.rs".into(),
            kind: TreeEntryKindDto::File,
            mode: 0o100_644,
            size: Some(42),
        });
        roundtrip(&WorktreeStatusDto {
            staged: vec![FileChangeDto {
                path: "a".into(),
                old_path: None,
                kind: ChangeKindDto::Modified,
            }],
            unstaged: Vec::new(),
        });
        roundtrip(&StashInfoDto {
            index: 0,
            id: oid(8),
            message: "WIP on main".into(),
            created_time: 1_700_000_000,
            branch: Some("main".into()),
        });
        roundtrip(&UpstreamInfoDto {
            name: "origin/main".into(),
            ahead: Some(2),
            behind: None,
            gone: false,
        });
        roundtrip(&RemoteInfoDto {
            name: "origin".into(),
            fetch_url: Some("git@example.invalid:repo".into()),
            push_url: None,
        });
        roundtrip(&ConflictEntryDto {
            path: "conflict.txt".into(),
            kind: ConflictKindDto::BothModified,
        });
        roundtrip(&SnapshotDto {
            generation: 9,
            head: Some(HeadDto::Detached { target: oid(10) }),
            refs: vec![RefInfoDto {
                full_name: "refs/tags/v1".into(),
                short_name: "v1".into(),
                kind: RefKindDto::Tag,
                target: oid(11),
            }],
            upstreams: vec![(
                "main".into(),
                UpstreamInfoDto {
                    name: "origin/main".into(),
                    ahead: Some(1),
                    behind: Some(3),
                    gone: false,
                },
            )],
            stashes: Vec::new(),
            remotes: Vec::new(),
            status: WorktreeStatusDto::default(),
            state: RepoStateDto::Normal,
            conflicts: Vec::new(),
            busy: Some(4),
        });

        roundtrip(&DiffRequestDto::Commit {
            commit: oid(12),
            path: "a.txt".into(),
        });
        roundtrip(&DiffRequestDto::Compare {
            base: Some(oid(13)),
            head: oid(14),
            old_path: Some("old.txt".into()),
            path: "new.txt".into(),
        });
        roundtrip(&DiffOptionsDto {
            context_lines: 5,
            ignore_whitespace: true,
        });
        roundtrip(&DiffLineDto {
            kind: LineKindDto::Added,
            old_lineno: None,
            new_lineno: Some(4),
            content: "let x = 1;".into(),
            no_newline_at_eof: false,
            emphasis: vec![SpanDto { start: 4, end: 5 }],
        });
        roundtrip(&HunkDto {
            old_start: 1,
            old_len: 2,
            new_start: 1,
            new_len: 3,
            lines: vec![DiffLineDto {
                kind: LineKindDto::Context,
                old_lineno: Some(1),
                new_lineno: Some(1),
                content: "fn main() {}".into(),
                no_newline_at_eof: true,
                emphasis: Vec::new(),
            }],
        });
        roundtrip(&FileDiffDto {
            hunks: Vec::new(),
            binary: true,
        });
        roundtrip(&GapDto {
            old_start: 2,
            new_start: 3,
            len: 10,
        });
        roundtrip(&HighlightSpanDto {
            range: SpanDto { start: 1, end: 4 },
            kind: HighlightKindDto::Keyword,
        });
        roundtrip(&LfsDiffSummaryDto {
            old: Some(LfsDiffSideDto {
                oid: "sha256:abc".into(),
                size: 10,
                present: true,
            }),
            new: None,
            text: "1 file".into(),
        });
        roundtrip(&ImageSideDto {
            bytes: Some(vec![1, 2, 3]),
            size: 3,
            format: Some(ImageFormatDto::Png),
            width: Some(1),
            height: Some(1),
        });
        roundtrip(&ImageDiffDto {
            kind: ImageKindDto::Raster,
            old: None,
            new: Some(ImageSideDto {
                bytes: None,
                size: 500,
                format: None,
                width: None,
                height: None,
            }),
        });
        roundtrip(&DiffViewDto {
            request: DiffRequestDto::Staged { path: "a".into() },
            diff: FileDiffDto::default(),
            old_highlights: vec![vec![HighlightSpanDto {
                range: SpanDto { start: 0, end: 2 },
                kind: HighlightKindDto::String,
            }]],
            new_highlights: Vec::new(),
            lfs: None,
            old_text: "old\n".into(),
            new_text: "new\n".into(),
            old_encoding: "UTF-8".into(),
            new_encoding: "Shift_JIS".into(),
            gaps: vec![GapDto {
                old_start: 1,
                new_start: 1,
                len: 0,
            }],
            image: Some(ImageDiffDto {
                kind: ImageKindDto::Svg,
                old: None,
                new: None,
            }),
        });
        roundtrip(&GraphRowDto {
            node_lane: 1,
            node_color: 2,
            width: 3,
            passes: vec![PassThroughDto { lane: 0, color: 1 }],
            edges: vec![SegmentDto {
                from_lane: 1,
                to_lane: 2,
                half: HalfDto::Top,
                color: 2,
            }],
        });
        roundtrip(&CommitChunkDto {
            generation: 1,
            commits: Vec::new(),
            rows: Vec::new(),
            complete: true,
        });
    }

    #[test]
    fn core_values_survive_the_round_trip() {
        use lamina_core::{ChangeKind, CommitInfo, FileChange, HeadInfo, RefInfo, RefKind};

        let oid = lamina_core::Oid::from_bytes_or_panic(&[9u8; 20]);
        let commit = CommitInfo {
            id: oid,
            parents: vec![lamina_core::Oid::from_bytes_or_panic(&[8u8; 20])],
            summary: "subject".into(),
            author_name: "A".into(),
            author_email: "a@example.invalid".into(),
            author_time: 10,
            committer_time: 11,
            shallow_boundary: false,
        };
        let decoded: CommitInfo = CommitDto::from(&commit).try_into().expect("convert");
        assert_eq!(decoded, commit);

        let reference = RefInfo {
            full_name: "refs/heads/main".into(),
            short_name: "main".into(),
            kind: RefKind::LocalBranch,
            target: oid,
        };
        let decoded: RefInfo = RefInfoDto::from(&reference).try_into().expect("convert");
        assert_eq!(decoded, reference);

        let head = HeadInfo::Branch {
            short_name: "main".into(),
            target: Some(oid),
        };
        let decoded: HeadInfo = HeadDto::from(&head).try_into().expect("convert");
        assert_eq!(decoded, head);

        let change = FileChange {
            path: "a.txt".into(),
            old_path: None,
            kind: ChangeKind::Modified,
        };
        let decoded: FileChange = FileChangeDto::from(&change).into();
        assert_eq!(decoded, change);
    }

    #[test]
    fn a_diff_view_survives_the_round_trip() {
        use lamina_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
        use lamina_core::session::{DiffRequest, DiffView};

        let diff = FileDiff {
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 2,
                lines: vec![DiffLine {
                    kind: LineKind::Added,
                    old_lineno: None,
                    new_lineno: Some(2),
                    content: "added".into(),
                    no_newline_at_eof: false,
                    emphasis: std::iter::once(0..5).collect(),
                }],
            }],
            binary: false,
        };
        let view = DiffView {
            request: DiffRequest::Staged { path: "a".into() },
            diff,
            old_highlights: std::sync::Arc::new(vec![Vec::new()]),
            new_highlights: std::sync::Arc::new(Vec::new()),
            lfs: None,
            old_text: std::sync::Arc::from("old\n"),
            new_text: std::sync::Arc::from("old\nadded\n"),
            old_encoding: lamina_core::encoding::TextEncoding::UTF_8,
            new_encoding: lamina_core::encoding::TextEncoding::UTF_8,
            gaps: std::sync::Arc::from(Vec::new()),
            image: None,
        };
        let decoded: DiffView = DiffViewDto::from(&view).try_into().expect("convert");
        assert_eq!(decoded.request, view.request);
        assert_eq!(decoded.diff, view.diff);
        assert_eq!(decoded.old_text, view.old_text);
        assert_eq!(decoded.new_text, view.new_text);
        assert_eq!(decoded.old_encoding, view.old_encoding);
        assert_eq!(decoded.new_encoding, view.new_encoding);
    }
}
