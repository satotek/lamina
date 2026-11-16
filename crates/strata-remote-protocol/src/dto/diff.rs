//! DTOs for diffs, highlights and the picture/LFS cards a diff view carries.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::codec::ProtocolError;
use crate::dto::repo::{ObjectId, oid};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffRequestDto {
    Commit {
        commit: ObjectId,
        path: String,
    },
    Staged {
        path: String,
    },
    Unstaged {
        path: String,
    },
    Compare {
        base: Option<ObjectId>,
        head: ObjectId,
        old_path: Option<String>,
        path: String,
    },
    CompareWorktree {
        base: ObjectId,
        path: String,
    },
}

impl From<&strata_core::session::DiffRequest> for DiffRequestDto {
    fn from(request: &strata_core::session::DiffRequest) -> Self {
        match request {
            strata_core::session::DiffRequest::Commit { commit, path } => Self::Commit {
                commit: ObjectId::from(*commit),
                path: path.clone(),
            },
            strata_core::session::DiffRequest::Staged { path } => {
                Self::Staged { path: path.clone() }
            }
            strata_core::session::DiffRequest::Unstaged { path } => {
                Self::Unstaged { path: path.clone() }
            }
            strata_core::session::DiffRequest::Compare {
                base,
                head,
                old_path,
                path,
            } => Self::Compare {
                base: base.map(ObjectId::from),
                head: ObjectId::from(*head),
                old_path: old_path.clone(),
                path: path.clone(),
            },
            strata_core::session::DiffRequest::CompareWorktree { base, path } => {
                Self::CompareWorktree {
                    base: ObjectId::from(*base),
                    path: path.clone(),
                }
            }
        }
    }
}

impl TryFrom<DiffRequestDto> for strata_core::session::DiffRequest {
    type Error = ProtocolError;

    fn try_from(request: DiffRequestDto) -> Result<Self, ProtocolError> {
        Ok(match request {
            DiffRequestDto::Commit { commit, path } => Self::Commit {
                commit: oid(commit)?,
                path,
            },
            DiffRequestDto::Staged { path } => Self::Staged { path },
            DiffRequestDto::Unstaged { path } => Self::Unstaged { path },
            DiffRequestDto::Compare {
                base,
                head,
                old_path,
                path,
            } => Self::Compare {
                base: base.map(oid).transpose()?,
                head: oid(head)?,
                old_path,
                path,
            },
            DiffRequestDto::CompareWorktree { base, path } => Self::CompareWorktree {
                base: oid(base)?,
                path,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffOptionsDto {
    pub context_lines: u32,
    pub ignore_whitespace: bool,
}

impl From<&strata_core::diff::DiffOptions> for DiffOptionsDto {
    fn from(options: &strata_core::diff::DiffOptions) -> Self {
        Self {
            context_lines: options.context_lines,
            ignore_whitespace: options.ignore_whitespace,
        }
    }
}

impl From<DiffOptionsDto> for strata_core::diff::DiffOptions {
    fn from(options: DiffOptionsDto) -> Self {
        Self {
            context_lines: options.context_lines,
            ignore_whitespace: options.ignore_whitespace,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineKindDto {
    Context,
    Added,
    Removed,
}

impl From<strata_core::diff::LineKind> for LineKindDto {
    fn from(kind: strata_core::diff::LineKind) -> Self {
        match kind {
            strata_core::diff::LineKind::Context => Self::Context,
            strata_core::diff::LineKind::Added => Self::Added,
            strata_core::diff::LineKind::Removed => Self::Removed,
        }
    }
}

impl From<LineKindDto> for strata_core::diff::LineKind {
    fn from(kind: LineKindDto) -> Self {
        match kind {
            LineKindDto::Context => Self::Context,
            LineKindDto::Added => Self::Added,
            LineKindDto::Removed => Self::Removed,
        }
    }
}

/// A byte range inside one line. `u32` keeps the payload compact; offsets in a
/// line never approach that limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanDto {
    pub start: u32,
    pub end: u32,
}

impl From<&std::ops::Range<usize>> for SpanDto {
    fn from(range: &std::ops::Range<usize>) -> Self {
        Self {
            start: u32::try_from(range.start).unwrap_or(u32::MAX),
            end: u32::try_from(range.end).unwrap_or(u32::MAX),
        }
    }
}

impl From<SpanDto> for std::ops::Range<usize> {
    fn from(span: SpanDto) -> Self {
        (span.start as usize)..(span.end as usize)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLineDto {
    pub kind: LineKindDto,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    pub content: String,
    pub no_newline_at_eof: bool,
    pub emphasis: Vec<SpanDto>,
}

impl From<&strata_core::diff::DiffLine> for DiffLineDto {
    fn from(line: &strata_core::diff::DiffLine) -> Self {
        Self {
            kind: line.kind.into(),
            old_lineno: line.old_lineno,
            new_lineno: line.new_lineno,
            content: line.content.clone(),
            no_newline_at_eof: line.no_newline_at_eof,
            emphasis: line.emphasis.iter().map(SpanDto::from).collect(),
        }
    }
}

impl From<DiffLineDto> for strata_core::diff::DiffLine {
    fn from(line: DiffLineDto) -> Self {
        Self {
            kind: line.kind.into(),
            old_lineno: line.old_lineno,
            new_lineno: line.new_lineno,
            content: line.content,
            no_newline_at_eof: line.no_newline_at_eof,
            emphasis: line.emphasis.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkDto {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub lines: Vec<DiffLineDto>,
}

impl From<&strata_core::diff::Hunk> for HunkDto {
    fn from(hunk: &strata_core::diff::Hunk) -> Self {
        Self {
            old_start: hunk.old_start,
            old_len: hunk.old_len,
            new_start: hunk.new_start,
            new_len: hunk.new_len,
            lines: hunk.lines.iter().map(DiffLineDto::from).collect(),
        }
    }
}

impl From<HunkDto> for strata_core::diff::Hunk {
    fn from(hunk: HunkDto) -> Self {
        Self {
            old_start: hunk.old_start,
            old_len: hunk.old_len,
            new_start: hunk.new_start,
            new_len: hunk.new_len,
            lines: hunk.lines.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiffDto {
    pub hunks: Vec<HunkDto>,
    pub binary: bool,
}

impl From<&strata_core::diff::FileDiff> for FileDiffDto {
    fn from(diff: &strata_core::diff::FileDiff) -> Self {
        Self {
            hunks: diff.hunks.iter().map(HunkDto::from).collect(),
            binary: diff.binary,
        }
    }
}

impl From<FileDiffDto> for strata_core::diff::FileDiff {
    fn from(diff: FileDiffDto) -> Self {
        Self {
            hunks: diff.hunks.into_iter().map(Into::into).collect(),
            binary: diff.binary,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapDto {
    pub old_start: u32,
    pub new_start: u32,
    pub len: u32,
}

impl From<&strata_core::diff::Gap> for GapDto {
    fn from(gap: &strata_core::diff::Gap) -> Self {
        Self {
            old_start: gap.old_start,
            new_start: gap.new_start,
            len: gap.len,
        }
    }
}

impl From<GapDto> for strata_core::diff::Gap {
    fn from(gap: GapDto) -> Self {
        Self {
            old_start: gap.old_start,
            new_start: gap.new_start,
            len: gap.len,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HighlightKindDto {
    Keyword,
    String,
    Comment,
    Function,
    Type,
    Constant,
    Number,
    Variable,
    Property,
    Operator,
    Punctuation,
    Attribute,
    Tag,
    Label,
    Escape,
    Embedded,
}

impl From<strata_core::highlight::HighlightKind> for HighlightKindDto {
    fn from(kind: strata_core::highlight::HighlightKind) -> Self {
        use strata_core::highlight::HighlightKind as Kind;
        match kind {
            Kind::Keyword => Self::Keyword,
            Kind::String => Self::String,
            Kind::Comment => Self::Comment,
            Kind::Function => Self::Function,
            Kind::Type => Self::Type,
            Kind::Constant => Self::Constant,
            Kind::Number => Self::Number,
            Kind::Variable => Self::Variable,
            Kind::Property => Self::Property,
            Kind::Operator => Self::Operator,
            Kind::Punctuation => Self::Punctuation,
            Kind::Attribute => Self::Attribute,
            Kind::Tag => Self::Tag,
            Kind::Label => Self::Label,
            Kind::Escape => Self::Escape,
            Kind::Embedded => Self::Embedded,
        }
    }
}

impl From<HighlightKindDto> for strata_core::highlight::HighlightKind {
    fn from(kind: HighlightKindDto) -> Self {
        match kind {
            HighlightKindDto::Keyword => Self::Keyword,
            HighlightKindDto::String => Self::String,
            HighlightKindDto::Comment => Self::Comment,
            HighlightKindDto::Function => Self::Function,
            HighlightKindDto::Type => Self::Type,
            HighlightKindDto::Constant => Self::Constant,
            HighlightKindDto::Number => Self::Number,
            HighlightKindDto::Variable => Self::Variable,
            HighlightKindDto::Property => Self::Property,
            HighlightKindDto::Operator => Self::Operator,
            HighlightKindDto::Punctuation => Self::Punctuation,
            HighlightKindDto::Attribute => Self::Attribute,
            HighlightKindDto::Tag => Self::Tag,
            HighlightKindDto::Label => Self::Label,
            HighlightKindDto::Escape => Self::Escape,
            HighlightKindDto::Embedded => Self::Embedded,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightSpanDto {
    pub range: SpanDto,
    pub kind: HighlightKindDto,
}

impl
    From<&(
        std::ops::Range<usize>,
        strata_core::highlight::HighlightKind,
    )> for HighlightSpanDto
{
    fn from(
        span: &(
            std::ops::Range<usize>,
            strata_core::highlight::HighlightKind,
        ),
    ) -> Self {
        Self {
            range: SpanDto::from(&span.0),
            kind: span.1.into(),
        }
    }
}

impl From<HighlightSpanDto>
    for (
        std::ops::Range<usize>,
        strata_core::highlight::HighlightKind,
    )
{
    fn from(span: HighlightSpanDto) -> Self {
        (span.range.into(), span.kind.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfsDiffSideDto {
    pub oid: String,
    pub size: u64,
    pub present: bool,
}

impl From<&strata_core::lfs::LfsDiffSide> for LfsDiffSideDto {
    fn from(side: &strata_core::lfs::LfsDiffSide) -> Self {
        Self {
            oid: side.oid.clone(),
            size: side.size,
            present: side.present,
        }
    }
}

impl From<LfsDiffSideDto> for strata_core::lfs::LfsDiffSide {
    fn from(side: LfsDiffSideDto) -> Self {
        Self {
            oid: side.oid,
            size: side.size,
            present: side.present,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfsDiffSummaryDto {
    pub old: Option<LfsDiffSideDto>,
    pub new: Option<LfsDiffSideDto>,
    pub text: String,
}

impl From<&strata_core::lfs::LfsDiffSummary> for LfsDiffSummaryDto {
    fn from(summary: &strata_core::lfs::LfsDiffSummary) -> Self {
        Self {
            old: summary.old.as_ref().map(LfsDiffSideDto::from),
            new: summary.new.as_ref().map(LfsDiffSideDto::from),
            text: summary.text.clone(),
        }
    }
}

impl From<LfsDiffSummaryDto> for strata_core::lfs::LfsDiffSummary {
    fn from(summary: LfsDiffSummaryDto) -> Self {
        Self {
            old: summary.old.map(Into::into),
            new: summary.new.map(Into::into),
            text: summary.text,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageKindDto {
    Raster,
    Svg,
}

impl From<strata_core::image::ImageKind> for ImageKindDto {
    fn from(kind: strata_core::image::ImageKind) -> Self {
        match kind {
            strata_core::image::ImageKind::Raster => Self::Raster,
            strata_core::image::ImageKind::Svg => Self::Svg,
        }
    }
}

impl From<ImageKindDto> for strata_core::image::ImageKind {
    fn from(kind: ImageKindDto) -> Self {
        match kind {
            ImageKindDto::Raster => Self::Raster,
            ImageKindDto::Svg => Self::Svg,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormatDto {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
    Ico,
    Svg,
}

impl From<strata_core::image::ImageFormat> for ImageFormatDto {
    fn from(format: strata_core::image::ImageFormat) -> Self {
        match format {
            strata_core::image::ImageFormat::Png => Self::Png,
            strata_core::image::ImageFormat::Jpeg => Self::Jpeg,
            strata_core::image::ImageFormat::Gif => Self::Gif,
            strata_core::image::ImageFormat::Webp => Self::Webp,
            strata_core::image::ImageFormat::Bmp => Self::Bmp,
            strata_core::image::ImageFormat::Ico => Self::Ico,
            strata_core::image::ImageFormat::Svg => Self::Svg,
        }
    }
}

impl From<ImageFormatDto> for strata_core::image::ImageFormat {
    fn from(format: ImageFormatDto) -> Self {
        match format {
            ImageFormatDto::Png => Self::Png,
            ImageFormatDto::Jpeg => Self::Jpeg,
            ImageFormatDto::Gif => Self::Gif,
            ImageFormatDto::Webp => Self::Webp,
            ImageFormatDto::Bmp => Self::Bmp,
            ImageFormatDto::Ico => Self::Ico,
            ImageFormatDto::Svg => Self::Svg,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSideDto {
    /// Absent when the caller was over the size cap or the LFS object is not
    /// local; `size` still describes the file.
    pub bytes: Option<Vec<u8>>,
    pub size: u64,
    pub format: Option<ImageFormatDto>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl From<&strata_core::image::ImageSide> for ImageSideDto {
    fn from(side: &strata_core::image::ImageSide) -> Self {
        Self {
            bytes: side.bytes.as_ref().map(|bytes| bytes.to_vec()),
            size: side.size,
            format: side.format.map(Into::into),
            width: side.width,
            height: side.height,
        }
    }
}

impl From<ImageSideDto> for strata_core::image::ImageSide {
    fn from(side: ImageSideDto) -> Self {
        Self {
            bytes: side.bytes.map(|bytes| Arc::from(bytes.into_boxed_slice())),
            size: side.size,
            format: side.format.map(Into::into),
            width: side.width,
            height: side.height,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageDiffDto {
    pub kind: ImageKindDto,
    pub old: Option<ImageSideDto>,
    pub new: Option<ImageSideDto>,
}

impl From<&strata_core::image::ImageDiff> for ImageDiffDto {
    fn from(diff: &strata_core::image::ImageDiff) -> Self {
        Self {
            kind: diff.kind.into(),
            old: diff.old.as_ref().map(ImageSideDto::from),
            new: diff.new.as_ref().map(ImageSideDto::from),
        }
    }
}

impl From<ImageDiffDto> for strata_core::image::ImageDiff {
    fn from(diff: ImageDiffDto) -> Self {
        Self {
            kind: diff.kind.into(),
            old: diff.old.map(Into::into),
            new: diff.new.map(Into::into),
        }
    }
}

/// A complete diff result.
///
/// Highlight spans are included: the helper already runs the highlighter, and
/// sending spans is cheaper than parsing every file again on the app side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffViewDto {
    pub request: DiffRequestDto,
    pub diff: FileDiffDto,
    pub old_highlights: Vec<Vec<HighlightSpanDto>>,
    pub new_highlights: Vec<Vec<HighlightSpanDto>>,
    pub lfs: Option<LfsDiffSummaryDto>,
    pub old_text: String,
    pub new_text: String,
    pub old_encoding: String,
    pub new_encoding: String,
    pub gaps: Vec<GapDto>,
    pub image: Option<ImageDiffDto>,
}

impl From<&strata_core::session::DiffView> for DiffViewDto {
    fn from(view: &strata_core::session::DiffView) -> Self {
        Self {
            request: DiffRequestDto::from(&view.request),
            diff: FileDiffDto::from(&view.diff),
            old_highlights: view
                .old_highlights
                .iter()
                .map(|line| line.iter().map(HighlightSpanDto::from).collect())
                .collect(),
            new_highlights: view
                .new_highlights
                .iter()
                .map(|line| line.iter().map(HighlightSpanDto::from).collect())
                .collect(),
            lfs: view.lfs.as_ref().map(LfsDiffSummaryDto::from),
            old_text: view.old_text.to_string(),
            new_text: view.new_text.to_string(),
            old_encoding: view.old_encoding.label().to_owned(),
            new_encoding: view.new_encoding.label().to_owned(),
            gaps: view.gaps.iter().map(GapDto::from).collect(),
            image: view.image.as_ref().map(ImageDiffDto::from),
        }
    }
}

impl TryFrom<DiffViewDto> for strata_core::session::DiffView {
    type Error = ProtocolError;

    fn try_from(view: DiffViewDto) -> Result<Self, ProtocolError> {
        Ok(Self {
            request: view.request.try_into()?,
            diff: view.diff.into(),
            old_highlights: Arc::new(
                view.old_highlights
                    .into_iter()
                    .map(|line| line.into_iter().map(Into::into).collect())
                    .collect(),
            ),
            new_highlights: Arc::new(
                view.new_highlights
                    .into_iter()
                    .map(|line| line.into_iter().map(Into::into).collect())
                    .collect(),
            ),
            lfs: view.lfs.map(Into::into),
            old_text: Arc::from(view.old_text),
            new_text: Arc::from(view.new_text),
            old_encoding: encoding(&view.old_encoding),
            new_encoding: encoding(&view.new_encoding),
            gaps: Arc::from(view.gaps.into_iter().map(Into::into).collect::<Vec<_>>()),
            image: view.image.map(Into::into),
        })
    }
}

/// Unknown labels fall back to UTF-8: an encoding name this build cannot
/// resolve must not make an otherwise readable diff fail.
fn encoding(label: &str) -> strata_core::encoding::TextEncoding {
    strata_core::encoding::TextEncoding::from_label(label)
        .unwrap_or(strata_core::encoding::TextEncoding::UTF_8)
}
