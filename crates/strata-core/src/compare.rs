//! Read-only comparison of arbitrary files and folders, independent of any git
//! repository.
//!
//! Two-way file compares produce the same [`FileDiff`], [`Gap`] runs, syntax
//! highlights and [`ImageDiff`] payload that the app's diff view already draws.
//! Three-way file compares reuse [`crate::merge::align_three_way`] so regions
//! mark whether left, right or both sides differ from the middle, and whether
//! both changed sides agree. Folder compares walk two or three directory trees
//! recursively, skipping `.git`, optionally honouring `.gitignore`, comparing
//! files by size before streaming their content hash, and cancelling promptly
//! via an [`AtomicBool`] flag.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gix::bstr::BStr;
use gix::glob::pattern::Case;
use gix::ignore::Search;

use crate::diff::{self, DiffOptions, FileDiff, Gap};
use crate::encoding::{TextEncoding, decode_for_display};
use crate::highlight::{Highlighter, LineHighlights, language_for_path};
use crate::image::{self, ImageDiff, ImageKind, ImageSide};
use crate::merge::{TextFile, ThreeWayRegionKind, align_three_way};
use crate::{Error, Oid, Result};

/// Guard against reading arbitrarily huge non-image files into memory for a
/// line-by-line text diff.
pub const MAX_TEXT_COMPARE_BYTES: usize = 32 * 1024 * 1024;
/// Matches the repository diff worker cap so highlighting large files never
/// stalls a background task.
pub const MAX_HIGHLIGHT_BYTES: usize = 1024 * 1024;
/// Buffer size for streaming file hashes during folder comparison.
const HASH_CHUNK_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Two-way file compare
// ---------------------------------------------------------------------------

/// Result of a two-way file comparison, carrying everything the diff pane or
/// image diff view needs without requiring a git repository or [`DiffRequest`].
#[derive(Debug, Clone)]
pub struct TwoWayFileCompare {
    pub left_path: PathBuf,
    pub right_path: PathBuf,
    pub diff: FileDiff,
    pub left_highlights: Arc<Vec<LineHighlights>>,
    pub right_highlights: Arc<Vec<LineHighlights>>,
    pub left_text: Arc<str>,
    pub right_text: Arc<str>,
    pub left_encoding: TextEncoding,
    pub right_encoding: TextEncoding,
    pub gaps: Arc<[Gap]>,
    pub image: Option<ImageDiff>,
    /// True when at least one side exceeded [`MAX_TEXT_COMPARE_BYTES`].
    pub too_large: bool,
}

/// Compares two files on disk. Returns `Ok(None)` if `cancel` is signalled.
pub fn compare_two_files(
    left: &Path,
    right: &Path,
    options: &DiffOptions,
    cancel: &AtomicBool,
) -> Result<Option<TwoWayFileCompare>> {
    compare_two_optional_files(Some(left), Some(right), options, cancel)
}

/// Compares two optional file paths on disk (`None` represents a missing side,
/// such as an entry that exists only in one directory). Returns `Ok(None)` if
/// `cancel` is signalled.
pub fn compare_two_optional_files(
    left: Option<&Path>,
    right: Option<&Path>,
    options: &DiffOptions,
    cancel: &AtomicBool,
) -> Result<Option<TwoWayFileCompare>> {
    if is_cancelled(cancel) {
        return Ok(None);
    }

    let left_path = left.map(Path::to_path_buf).unwrap_or_default();
    let right_path = right.map(Path::to_path_buf).unwrap_or_default();
    let image_kind = shared_image_kind(&[left, right]);

    if image_kind == Some(ImageKind::Raster) {
        let old = read_image_side(left, ImageKind::Raster, cancel)?;
        if is_cancelled(cancel) {
            return Ok(None);
        }
        let new = read_image_side(right, ImageKind::Raster, cancel)?;
        if is_cancelled(cancel) {
            return Ok(None);
        }
        return Ok(Some(TwoWayFileCompare {
            left_path,
            right_path,
            diff: FileDiff {
                hunks: Vec::new(),
                binary: true,
            },
            left_highlights: Arc::new(Vec::new()),
            right_highlights: Arc::new(Vec::new()),
            left_text: Arc::from(""),
            right_text: Arc::from(""),
            left_encoding: TextEncoding::UTF_8,
            right_encoding: TextEncoding::UTF_8,
            gaps: Arc::from(Vec::new()),
            image: Some(ImageDiff {
                kind: ImageKind::Raster,
                old,
                new,
            }),
            too_large: false,
        }));
    }

    let (left_bytes, left_too_large) = read_optional_text_bytes(left, cancel)?;
    if is_cancelled(cancel) {
        return Ok(None);
    }
    let (right_bytes, right_too_large) = read_optional_text_bytes(right, cancel)?;
    if is_cancelled(cancel) {
        return Ok(None);
    }

    if left_too_large || right_too_large {
        return Ok(Some(TwoWayFileCompare {
            left_path,
            right_path,
            diff: FileDiff {
                hunks: Vec::new(),
                binary: false,
            },
            left_highlights: Arc::new(Vec::new()),
            right_highlights: Arc::new(Vec::new()),
            left_text: Arc::from(""),
            right_text: Arc::from(""),
            left_encoding: TextEncoding::UTF_8,
            right_encoding: TextEncoding::UTF_8,
            gaps: Arc::from(Vec::new()),
            image: None,
            too_large: true,
        }));
    }

    Ok(compare_two_bytes(
        &left_path,
        left_bytes.as_deref(),
        &right_path,
        right_bytes.as_deref(),
        options,
        cancel,
    ))
}

/// Compares two in-memory byte buffers using path hints for image detection
/// and syntax highlighting. Returns `None` if `cancel` is signalled.
pub fn compare_two_bytes(
    left_path: &Path,
    left_bytes: Option<&[u8]>,
    right_path: &Path,
    right_bytes: Option<&[u8]>,
    options: &DiffOptions,
    cancel: &AtomicBool,
) -> Option<TwoWayFileCompare> {
    if is_cancelled(cancel) {
        return None;
    }

    let left_hint = left_bytes.map(|_| left_path);
    let right_hint = right_bytes.map(|_| right_path);
    let image_kind = shared_image_kind(&[left_hint, right_hint]);

    if image_kind == Some(ImageKind::Raster) {
        let old = left_bytes.map(|bytes| {
            ImageSide::new(
                ImageKind::Raster,
                Some(bytes.to_vec()),
                bytes.len() as u64,
                image::MAX_IMAGE_BYTES,
            )
        });
        let new = right_bytes.map(|bytes| {
            ImageSide::new(
                ImageKind::Raster,
                Some(bytes.to_vec()),
                bytes.len() as u64,
                image::MAX_IMAGE_BYTES,
            )
        });
        return Some(TwoWayFileCompare {
            left_path: left_path.to_path_buf(),
            right_path: right_path.to_path_buf(),
            diff: FileDiff {
                hunks: Vec::new(),
                binary: true,
            },
            left_highlights: Arc::new(Vec::new()),
            right_highlights: Arc::new(Vec::new()),
            left_text: Arc::from(""),
            right_text: Arc::from(""),
            left_encoding: TextEncoding::UTF_8,
            right_encoding: TextEncoding::UTF_8,
            gaps: Arc::from(Vec::new()),
            image: Some(ImageDiff {
                kind: ImageKind::Raster,
                old,
                new,
            }),
            too_large: false,
        });
    }

    let left_present = left_bytes.is_some();
    let right_present = right_bytes.is_some();
    let left_raw = left_bytes.unwrap_or_default();
    let right_raw = right_bytes.unwrap_or_default();

    let (left_decoded, left_encoding) = decode_for_display(left_raw, || None);
    let (right_decoded, right_encoding) = decode_for_display(right_raw, || None);

    if is_binary_bytes(left_raw, left_encoding) || is_binary_bytes(right_raw, right_encoding) {
        return Some(TwoWayFileCompare {
            left_path: left_path.to_path_buf(),
            right_path: right_path.to_path_buf(),
            diff: FileDiff {
                hunks: Vec::new(),
                binary: true,
            },
            left_highlights: Arc::new(Vec::new()),
            right_highlights: Arc::new(Vec::new()),
            left_text: Arc::from(""),
            right_text: Arc::from(""),
            left_encoding,
            right_encoding,
            gaps: Arc::from(Vec::new()),
            image: None,
            too_large: false,
        });
    }

    let left_text: Arc<str> = Arc::from(left_decoded.into_owned());
    let right_text: Arc<str> = Arc::from(right_decoded.into_owned());

    if is_cancelled(cancel) {
        return None;
    }

    let file_diff = diff::diff_text(left_text.as_bytes(), right_text.as_bytes(), options);
    let gap_runs = Arc::from(diff::gaps(
        &file_diff,
        diff::line_count(&left_text),
        diff::line_count(&right_text),
    ));
    let image = image_kind.map(|kind| {
        image::from_text_inputs(
            kind,
            left_present,
            right_present,
            left_raw,
            right_raw,
            image::MAX_IMAGE_BYTES,
        )
    });

    if is_cancelled(cancel) {
        return None;
    }

    let mut highlighter = Highlighter::new();
    let left_highlights = if file_diff.binary {
        Vec::new()
    } else {
        highlight_text(&mut highlighter, left_path, &left_text)
    };
    if is_cancelled(cancel) {
        return None;
    }
    let right_highlights = if file_diff.binary {
        Vec::new()
    } else {
        highlight_text(&mut highlighter, right_path, &right_text)
    };

    Some(TwoWayFileCompare {
        left_path: left_path.to_path_buf(),
        right_path: right_path.to_path_buf(),
        diff: file_diff,
        left_highlights: Arc::new(left_highlights),
        right_highlights: Arc::new(right_highlights),
        left_text,
        right_text,
        left_encoding,
        right_encoding,
        gaps: gap_runs,
        image,
        too_large: false,
    })
}

// ---------------------------------------------------------------------------
// Three-way file compare
// ---------------------------------------------------------------------------

/// Classification of one aligned region in a three-way file comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreeWayDiffKind {
    /// Left, middle and right all have the same text.
    Unchanged,
    /// Only left differs from middle.
    LeftChanged,
    /// Only right differs from middle.
    RightChanged,
    /// Both left and right differ from middle, and agree (`left == right`).
    BothAgree,
    /// Both left and right differ from middle, and disagree (`left != right`).
    BothDisagree,
}

/// One aligned stretch of lines across left, middle and right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreeWayRegion {
    pub kind: ThreeWayDiffKind,
    /// 0-based line range in the left file.
    pub left: Range<usize>,
    /// 0-based line range in the middle (base) file.
    pub middle: Range<usize>,
    /// 0-based line range in the right file.
    pub right: Range<usize>,
    /// 0-based row range in [`ThreeWayFileCompare::rows`].
    pub row_range: Range<usize>,
}

/// One visual row in a three-way synchronized comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreeWayRow {
    pub kind: ThreeWayDiffKind,
    /// 1-based line number in each side, or `None` for padding rows.
    pub left: Option<u32>,
    pub middle: Option<u32>,
    pub right: Option<u32>,
}

/// Summary counts of changed regions in a three-way comparison.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreeWayCounts {
    pub left_changed: usize,
    pub right_changed: usize,
    pub both_agree: usize,
    pub both_disagree: usize,
}

impl ThreeWayCounts {
    pub fn total_changes(&self) -> usize {
        self.left_changed + self.right_changed + self.both_agree + self.both_disagree
    }
}

/// Three sides of a picture for a three-way image comparison.
#[derive(Debug, Clone)]
pub struct ThreeWayImageCompare {
    pub kind: ImageKind,
    pub left: Option<ImageSide>,
    pub middle: Option<ImageSide>,
    pub right: Option<ImageSide>,
}

/// Result of a three-way file comparison.
#[derive(Debug, Clone)]
pub struct ThreeWayFileCompare {
    /// The name each pane header shows. A filesystem compare uses the path; a
    /// comparison of repository revisions uses the branch and short id.
    pub left_name: String,
    pub middle_name: String,
    pub right_name: String,
    pub left_lines: Arc<Vec<String>>,
    pub middle_lines: Arc<Vec<String>>,
    pub right_lines: Arc<Vec<String>>,
    pub left_highlights: Arc<Vec<LineHighlights>>,
    pub middle_highlights: Arc<Vec<LineHighlights>>,
    pub right_highlights: Arc<Vec<LineHighlights>>,
    pub left_encoding: TextEncoding,
    pub middle_encoding: TextEncoding,
    pub right_encoding: TextEncoding,
    pub regions: Vec<ThreeWayRegion>,
    pub rows: Vec<ThreeWayRow>,
    pub binary: bool,
    pub too_large: bool,
    pub images: Option<ThreeWayImageCompare>,
}

impl ThreeWayFileCompare {
    /// Changed regions (excluding [`ThreeWayDiffKind::Unchanged`]).
    pub fn change_regions(&self) -> impl Iterator<Item = &ThreeWayRegion> {
        self.regions
            .iter()
            .filter(|region| region.kind != ThreeWayDiffKind::Unchanged)
    }

    /// Counts of changed regions by kind.
    pub fn counts(&self) -> ThreeWayCounts {
        let mut counts = ThreeWayCounts::default();
        for region in &self.regions {
            match region.kind {
                ThreeWayDiffKind::Unchanged => {}
                ThreeWayDiffKind::LeftChanged => counts.left_changed += 1,
                ThreeWayDiffKind::RightChanged => counts.right_changed += 1,
                ThreeWayDiffKind::BothAgree => counts.both_agree += 1,
                ThreeWayDiffKind::BothDisagree => counts.both_disagree += 1,
            }
        }
        counts
    }
}

/// Compares three files on disk (left, middle/base, right). Returns `Ok(None)`
/// if `cancel` is signalled.
pub fn compare_three_files(
    left: &Path,
    middle: &Path,
    right: &Path,
    cancel: &AtomicBool,
) -> Result<Option<ThreeWayFileCompare>> {
    compare_three_optional_files(Some(left), Some(middle), Some(right), cancel)
}

/// One side of an in-memory three-way file comparison.
#[derive(Debug, Clone)]
pub struct ThreeWaySide {
    /// Name shown for the side, such as a branch and short id.
    pub name: String,
    /// Path used for image detection and syntax highlighting, when `name` is
    /// only a label.
    pub path_hint: Option<String>,
    pub bytes: Option<Vec<u8>>,
}

impl ThreeWaySide {
    pub fn new(name: impl Into<String>, path_hint: Option<String>, bytes: Option<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            path_hint,
            bytes,
        }
    }

    fn detection_path(&self) -> Option<&Path> {
        self.path_hint.as_deref().map(Path::new)
    }
}

/// Compares three optional file paths on disk. Returns `Ok(None)` if `cancel`
/// is signalled.
pub fn compare_three_optional_files(
    left: Option<&Path>,
    middle: Option<&Path>,
    right: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<Option<ThreeWayFileCompare>> {
    if is_cancelled(cancel) {
        return Ok(None);
    }

    let image_kind = shared_image_kind(&[left, middle, right]);

    if image_kind == Some(ImageKind::Raster) {
        let left_side = read_image_side(left, ImageKind::Raster, cancel)?;
        if is_cancelled(cancel) {
            return Ok(None);
        }
        let middle_side = read_image_side(middle, ImageKind::Raster, cancel)?;
        if is_cancelled(cancel) {
            return Ok(None);
        }
        let right_side = read_image_side(right, ImageKind::Raster, cancel)?;
        if is_cancelled(cancel) {
            return Ok(None);
        }
        return Ok(Some(ThreeWayFileCompare {
            left_name: display_path(left),
            middle_name: display_path(middle),
            right_name: display_path(right),
            left_lines: Arc::new(Vec::new()),
            middle_lines: Arc::new(Vec::new()),
            right_lines: Arc::new(Vec::new()),
            left_highlights: Arc::new(Vec::new()),
            middle_highlights: Arc::new(Vec::new()),
            right_highlights: Arc::new(Vec::new()),
            left_encoding: TextEncoding::UTF_8,
            middle_encoding: TextEncoding::UTF_8,
            right_encoding: TextEncoding::UTF_8,
            regions: Vec::new(),
            rows: Vec::new(),
            binary: true,
            too_large: false,
            images: Some(ThreeWayImageCompare {
                kind: ImageKind::Raster,
                left: left_side,
                middle: middle_side,
                right: right_side,
            }),
        }));
    }

    let (left_bytes, left_too_large) = read_optional_text_bytes(left, cancel)?;
    if is_cancelled(cancel) {
        return Ok(None);
    }
    let (middle_bytes, middle_too_large) = read_optional_text_bytes(middle, cancel)?;
    if is_cancelled(cancel) {
        return Ok(None);
    }
    let (right_bytes, right_too_large) = read_optional_text_bytes(right, cancel)?;
    if is_cancelled(cancel) {
        return Ok(None);
    }

    if left_too_large || middle_too_large || right_too_large {
        return Ok(Some(ThreeWayFileCompare {
            left_name: display_path(left),
            middle_name: display_path(middle),
            right_name: display_path(right),
            left_lines: Arc::new(Vec::new()),
            middle_lines: Arc::new(Vec::new()),
            right_lines: Arc::new(Vec::new()),
            left_highlights: Arc::new(Vec::new()),
            middle_highlights: Arc::new(Vec::new()),
            right_highlights: Arc::new(Vec::new()),
            left_encoding: TextEncoding::UTF_8,
            middle_encoding: TextEncoding::UTF_8,
            right_encoding: TextEncoding::UTF_8,
            regions: Vec::new(),
            rows: Vec::new(),
            binary: false,
            too_large: true,
            images: None,
        }));
    }

    let side = |path: Option<&Path>, bytes: Option<Vec<u8>>| {
        let name = display_path(path);
        let hint = (!name.is_empty()).then(|| name.clone());
        ThreeWaySide::new(name, hint, bytes)
    };
    Ok(compare_three_sides(
        side(left, left_bytes),
        side(middle, middle_bytes),
        side(right, right_bytes),
        cancel,
    ))
}

/// Compares three in-memory sides. The names are what the panes show; the
/// optional path hints drive image detection and syntax highlighting.
///
/// Returns `None` if `cancel` is signalled.
pub fn compare_three_sides(
    left: ThreeWaySide,
    middle: ThreeWaySide,
    right: ThreeWaySide,
    cancel: &AtomicBool,
) -> Option<ThreeWayFileCompare> {
    if is_cancelled(cancel) {
        return None;
    }

    let hints = [
        left.detection_path(),
        middle.detection_path(),
        right.detection_path(),
    ];
    let image_kind = shared_image_kind(&hints);

    if image_kind == Some(ImageKind::Raster) {
        let image_side = |side: &ThreeWaySide| {
            side.bytes.as_ref().map(|bytes| {
                ImageSide::new(
                    ImageKind::Raster,
                    Some(bytes.clone()),
                    bytes.len() as u64,
                    image::MAX_IMAGE_BYTES,
                )
            })
        };
        let images = ThreeWayImageCompare {
            kind: ImageKind::Raster,
            left: image_side(&left),
            middle: image_side(&middle),
            right: image_side(&right),
        };
        return Some(ThreeWayFileCompare {
            left_name: left.name,
            middle_name: middle.name,
            right_name: right.name,
            left_lines: Arc::new(Vec::new()),
            middle_lines: Arc::new(Vec::new()),
            right_lines: Arc::new(Vec::new()),
            left_highlights: Arc::new(Vec::new()),
            middle_highlights: Arc::new(Vec::new()),
            right_highlights: Arc::new(Vec::new()),
            left_encoding: TextEncoding::UTF_8,
            middle_encoding: TextEncoding::UTF_8,
            right_encoding: TextEncoding::UTF_8,
            regions: Vec::new(),
            rows: Vec::new(),
            binary: true,
            too_large: false,
            images: Some(images),
        });
    }

    if [&left, &middle, &right]
        .into_iter()
        .filter_map(|side| side.bytes.as_ref())
        .any(|bytes| bytes.len() > MAX_TEXT_COMPARE_BYTES)
    {
        return Some(ThreeWayFileCompare {
            left_name: left.name,
            middle_name: middle.name,
            right_name: right.name,
            left_lines: Arc::new(Vec::new()),
            middle_lines: Arc::new(Vec::new()),
            right_lines: Arc::new(Vec::new()),
            left_highlights: Arc::new(Vec::new()),
            middle_highlights: Arc::new(Vec::new()),
            right_highlights: Arc::new(Vec::new()),
            left_encoding: TextEncoding::UTF_8,
            middle_encoding: TextEncoding::UTF_8,
            right_encoding: TextEncoding::UTF_8,
            regions: Vec::new(),
            rows: Vec::new(),
            binary: false,
            too_large: true,
            images: None,
        });
    }

    let left_raw = left.bytes.as_deref().unwrap_or_default();
    let middle_raw = middle.bytes.as_deref().unwrap_or_default();
    let right_raw = right.bytes.as_deref().unwrap_or_default();

    let left_file = TextFile::parse(left_raw);
    let middle_file = TextFile::parse(middle_raw);
    let right_file = TextFile::parse(right_raw);

    let binary = [
        (&left_file, left_raw),
        (&middle_file, middle_raw),
        (&right_file, right_raw),
    ]
    .into_iter()
    .any(|(file, raw)| is_binary_bytes(raw, file.encoding));

    if binary {
        return Some(ThreeWayFileCompare {
            left_name: left.name,
            middle_name: middle.name,
            right_name: right.name,
            left_lines: Arc::new(Vec::new()),
            middle_lines: Arc::new(Vec::new()),
            right_lines: Arc::new(Vec::new()),
            left_highlights: Arc::new(Vec::new()),
            middle_highlights: Arc::new(Vec::new()),
            right_highlights: Arc::new(Vec::new()),
            left_encoding: left_file.encoding,
            middle_encoding: middle_file.encoding,
            right_encoding: right_file.encoding,
            regions: Vec::new(),
            rows: Vec::new(),
            binary: true,
            too_large: false,
            images: None,
        });
    }

    if is_cancelled(cancel) {
        return None;
    }

    let aligned = align_three_way(&middle_file.lines, &left_file.lines, &right_file.lines);
    let mut regions = Vec::with_capacity(aligned.len());
    let mut rows = Vec::new();

    for segment in aligned {
        let kind = match segment.kind {
            ThreeWayRegionKind::Unchanged => ThreeWayDiffKind::Unchanged,
            ThreeWayRegionKind::LeftOnly => ThreeWayDiffKind::LeftChanged,
            ThreeWayRegionKind::RightOnly => ThreeWayDiffKind::RightChanged,
            ThreeWayRegionKind::BothSame => ThreeWayDiffKind::BothAgree,
            ThreeWayRegionKind::Conflict => ThreeWayDiffKind::BothDisagree,
        };
        let row_start = rows.len();
        let len = segment
            .left
            .len()
            .max(segment.base.len())
            .max(segment.right.len());
        for offset in 0..len {
            rows.push(ThreeWayRow {
                kind,
                left: (offset < segment.left.len())
                    .then(|| (segment.left.start + offset + 1) as u32),
                middle: (offset < segment.base.len())
                    .then(|| (segment.base.start + offset + 1) as u32),
                right: (offset < segment.right.len())
                    .then(|| (segment.right.start + offset + 1) as u32),
            });
        }
        let row_end = rows.len();
        regions.push(ThreeWayRegion {
            kind,
            left: segment.left,
            middle: segment.base,
            right: segment.right,
            row_range: row_start..row_end,
        });
    }

    if is_cancelled(cancel) {
        return None;
    }

    let left_hint = left.detection_path();
    let middle_hint = middle.detection_path();
    let right_hint = right.detection_path();
    let mut highlighter = Highlighter::new();
    let left_highlights = left_hint.map_or_else(Vec::new, |path| {
        highlight_lines(
            &mut highlighter,
            path,
            &left_file.lines,
            left_file.trailing_newline,
        )
    });
    if is_cancelled(cancel) {
        return None;
    }
    let middle_highlights = middle_hint.map_or_else(Vec::new, |path| {
        highlight_lines(
            &mut highlighter,
            path,
            &middle_file.lines,
            middle_file.trailing_newline,
        )
    });
    if is_cancelled(cancel) {
        return None;
    }
    let right_highlights = right_hint.map_or_else(Vec::new, |path| {
        highlight_lines(
            &mut highlighter,
            path,
            &right_file.lines,
            right_file.trailing_newline,
        )
    });

    let images = image_kind.map(|kind| {
        let svg_side = |side: &ThreeWaySide| {
            side.bytes.as_ref().map(|bytes| {
                ImageSide::new(
                    kind,
                    Some(bytes.clone()),
                    bytes.len() as u64,
                    image::MAX_IMAGE_BYTES,
                )
            })
        };
        ThreeWayImageCompare {
            kind,
            left: svg_side(&left),
            middle: svg_side(&middle),
            right: svg_side(&right),
        }
    });

    Some(ThreeWayFileCompare {
        left_name: left.name,
        middle_name: middle.name,
        right_name: right.name,
        left_lines: Arc::new(left_file.lines),
        middle_lines: Arc::new(middle_file.lines),
        right_lines: Arc::new(right_file.lines),
        left_highlights: Arc::new(left_highlights),
        middle_highlights: Arc::new(middle_highlights),
        right_highlights: Arc::new(right_highlights),
        left_encoding: left_file.encoding,
        middle_encoding: middle_file.encoding,
        right_encoding: right_file.encoding,
        regions,
        rows,
        binary: false,
        too_large: false,
        images,
    })
}

/// Compares three in-memory byte slices, named by the paths they would have on
/// disk. Returns `None` if `cancel` is signalled.
pub fn compare_three_bytes(
    left_path: &Path,
    left_bytes: Option<&[u8]>,
    middle_path: &Path,
    middle_bytes: Option<&[u8]>,
    right_path: &Path,
    right_bytes: Option<&[u8]>,
    cancel: &AtomicBool,
) -> Option<ThreeWayFileCompare> {
    let side = |path: &Path, bytes: Option<&[u8]>| {
        let name = path.display().to_string();
        ThreeWaySide::new(name.clone(), Some(name), bytes.map(<[u8]>::to_vec))
    };
    compare_three_sides(
        side(left_path, left_bytes),
        side(middle_path, middle_bytes),
        side(right_path, right_bytes),
        cancel,
    )
}

fn display_path(path: Option<&Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_default()
}
// ---------------------------------------------------------------------------
// Folder compare (2 or 3 directories)
// ---------------------------------------------------------------------------

/// Options controlling recursive directory comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderCompareOptions {
    /// When `true` (the default), `.gitignore` files inside each compared tree
    /// filter out matching paths. `.git` directories are always skipped.
    pub respect_gitignore: bool,
}

impl Default for FolderCompareOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
        }
    }
}

/// Filesystem entry type for one side of a folder comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FolderEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Comparison status of an entry across the compared directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FolderEntryStatus {
    Identical,
    Different,
    OnlyLeft,
    OnlyMiddle,
    OnlyRight,
    TypeMismatch,
}

/// Per-side metadata for one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSideInfo {
    pub kind: FolderEntryKind,
    pub size: Option<u64>,
}

/// One row of a recursive two-way or three-way folder comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderCompareEntry {
    /// Forward-slash separated path relative to the compared root directories.
    pub relative_path: String,
    /// Zero-based directory nesting depth (`0` for root children).
    pub depth: usize,
    /// Shared entry kind across sides, or the first present side's kind on
    /// [`FolderEntryStatus::TypeMismatch`].
    pub kind: FolderEntryKind,
    pub status: FolderEntryStatus,
    /// Metadata for `[left, right]` (2-way) or `[left, middle, right]` (3-way).
    pub sides: Vec<Option<FolderSideInfo>>,
}

/// Entry counts broken down by status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderCompareCounts {
    pub total: usize,
    pub identical: usize,
    pub different: usize,
    pub only_left: usize,
    pub only_middle: usize,
    pub only_right: usize,
    pub type_mismatch: usize,
}

impl FolderCompareCounts {
    pub(crate) fn record(&mut self, status: FolderEntryStatus) {
        self.total += 1;
        match status {
            FolderEntryStatus::Identical => self.identical += 1,
            FolderEntryStatus::Different => self.different += 1,
            FolderEntryStatus::OnlyLeft => self.only_left += 1,
            FolderEntryStatus::OnlyMiddle => self.only_middle += 1,
            FolderEntryStatus::OnlyRight => self.only_right += 1,
            FolderEntryStatus::TypeMismatch => self.type_mismatch += 1,
        }
    }
}

/// Output of comparing two or three directories.
#[derive(Debug, Clone)]
pub struct FolderCompareResult {
    pub roots: Vec<PathBuf>,
    pub entries: Vec<FolderCompareEntry>,
    pub counts: FolderCompareCounts,
}

// ---------------------------------------------------------------------------
// Repository revision three-way compare
// ---------------------------------------------------------------------------

/// How a path changed relative to the base tree when both sides are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreeWayTreeChange {
    /// Only the left tree differs from the base.
    LeftChanged,
    /// Only the right tree differs from the base.
    RightChanged,
    /// Both sides changed and ended up with the same content.
    BothAgree,
    /// Both sides changed and disagree, so the path is a candidate conflict.
    BothDisagree,
}

/// One file of a three-way comparison between repository trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreeWayTreeEntry {
    /// Forward-slash separated path relative to the repository root.
    pub relative_path: String,
    pub kind: FolderEntryKind,
    /// Presence and equality across `[left, base, right]`, with the folder
    /// compare's vocabulary.
    pub status: FolderEntryStatus,
    /// How the path changed relative to the base.
    pub change: ThreeWayTreeChange,
    /// `git merge-tree` reports a real merge conflict for this path.
    pub conflicting: bool,
    /// Metadata for `[left, base, right]`.
    pub sides: Vec<Option<FolderSideInfo>>,
}

/// Output of comparing three repository trees.
#[derive(Debug, Clone)]
pub struct ThreeWayTreeCompare {
    pub left: Oid,
    pub base: Oid,
    pub right: Oid,
    pub entries: Vec<ThreeWayTreeEntry>,
    pub counts: FolderCompareCounts,
    /// `git merge-tree` was available and reported conflicts. When it is too
    /// old the entries still carry the tree-diff classification, but no entry
    /// is marked [`ThreeWayTreeEntry::conflicting`].
    pub merge_tree_checked: bool,
}

/// Compares 2 or 3 directories recursively. Returns `Ok(None)` when cancelled.
pub fn compare_folders(
    roots: &[&Path],
    options: FolderCompareOptions,
    cancel: &AtomicBool,
) -> Result<Option<FolderCompareResult>> {
    if !(2..=3).contains(&roots.len()) {
        return Err(Error::other(format!(
            "folder compare requires 2 or 3 directories, got {}",
            roots.len()
        )));
    }
    for root in roots {
        if !root.is_dir() {
            return Err(Error::other(format!(
                "compare path is not a directory: {}",
                root.display()
            )));
        }
    }
    if is_cancelled(cancel) {
        return Ok(None);
    }

    let mut ignore_stacks: Vec<Vec<IgnoreScope>> = vec![Vec::new(); roots.len()];
    let mut entries = Vec::new();
    let dir_paths: Vec<Option<PathBuf>> =
        roots.iter().map(|path| Some(path.to_path_buf())).collect();

    let completed = walk_folder_level(
        &dir_paths,
        "",
        0,
        options,
        &mut ignore_stacks,
        cancel,
        &mut entries,
    )?;

    if !completed {
        return Ok(None);
    }

    let mut counts = FolderCompareCounts::default();
    for entry in &entries {
        counts.record(entry.status);
    }

    Ok(Some(FolderCompareResult {
        roots: roots.iter().map(|p| p.to_path_buf()).collect(),
        entries,
        counts,
    }))
}

#[derive(Clone)]
struct IgnoreScope {
    rel_prefix: String,
    search: Search,
}

fn load_gitignore(dir: &Path, rel_prefix: &str) -> Option<IgnoreScope> {
    let gitignore_path = dir.join(".gitignore");
    let bytes = fs::read(&gitignore_path).ok()?;
    let mut search = Search::default();
    search
        .add_patterns_buffer(
            &bytes,
            &gitignore_path,
            None,
            gix::ignore::search::Ignore::default(),
        )
        .ok()?;
    Some(IgnoreScope {
        rel_prefix: rel_prefix.to_owned(),
        search,
    })
}

fn is_gitignored(stack: &[IgnoreScope], rel_path: &str, is_dir: bool) -> bool {
    let mut ignored = false;
    let case = if cfg!(any(target_os = "macos", target_os = "windows")) {
        Case::Fold
    } else {
        Case::Sensitive
    };
    for scope in stack {
        let local_rel = if scope.rel_prefix.is_empty() {
            rel_path
        } else {
            let Some(rest) = rel_path
                .strip_prefix(&scope.rel_prefix)
                .and_then(|s| s.strip_prefix('/'))
            else {
                continue;
            };
            rest
        };
        let rel_bstr: &BStr = local_rel.as_bytes().into();
        if let Some(m) = scope
            .search
            .pattern_matching_relative_path(rel_bstr, Some(is_dir), case)
        {
            ignored = !m.pattern.is_negative();
        }
    }
    ignored
}

struct DiscoveredChild {
    full_path: PathBuf,
    kind: FolderEntryKind,
    size: Option<u64>,
}

/// Walks one directory level across 2 or 3 sides and appends sorted entries.
/// Returns `true` when finished without cancellation, or `false` if cancelled.
fn walk_folder_level(
    dirs: &[Option<PathBuf>],
    rel_prefix: &str,
    depth: usize,
    options: FolderCompareOptions,
    ignore_stacks: &mut [Vec<IgnoreScope>],
    cancel: &AtomicBool,
    out: &mut Vec<FolderCompareEntry>,
) -> Result<bool> {
    if is_cancelled(cancel) {
        return Ok(false);
    }

    let side_count = dirs.len();
    let mut pushed_ignore = vec![false; side_count];
    if options.respect_gitignore {
        for (side_idx, dir) in dirs.iter().enumerate() {
            if let Some(dir) = dir
                && let Some(scope) = load_gitignore(dir, rel_prefix)
            {
                ignore_stacks[side_idx].push(scope);
                pushed_ignore[side_idx] = true;
            }
        }
    }

    let result =
        walk_folder_level_inner(dirs, rel_prefix, depth, options, ignore_stacks, cancel, out);

    for (side_idx, pushed) in pushed_ignore.into_iter().enumerate() {
        if pushed {
            ignore_stacks[side_idx].pop();
        }
    }

    result
}

fn walk_folder_level_inner(
    dirs: &[Option<PathBuf>],
    rel_prefix: &str,
    depth: usize,
    options: FolderCompareOptions,
    ignore_stacks: &mut [Vec<IgnoreScope>],
    cancel: &AtomicBool,
    out: &mut Vec<FolderCompareEntry>,
) -> Result<bool> {
    let side_count = dirs.len();
    let mut per_side: Vec<BTreeMap<String, DiscoveredChild>> = Vec::with_capacity(side_count);

    for (side_idx, dir) in dirs.iter().enumerate() {
        let mut map = BTreeMap::new();
        if let Some(dir) = dir {
            for entry in fs::read_dir(dir)? {
                if is_cancelled(cancel) {
                    return Ok(false);
                }
                let entry = entry?;
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy().into_owned();
                if name == ".git" {
                    continue;
                }
                let full_path = entry.path();
                let meta = fs::symlink_metadata(&full_path)?;
                let kind = classify_metadata(&meta);
                let size = (kind == FolderEntryKind::File).then_some(meta.len());
                let rel_path = join_rel(rel_prefix, &name);
                if options.respect_gitignore
                    && is_gitignored(
                        &ignore_stacks[side_idx],
                        &rel_path,
                        kind == FolderEntryKind::Directory,
                    )
                {
                    continue;
                }
                map.insert(
                    name,
                    DiscoveredChild {
                        full_path,
                        kind,
                        size,
                    },
                );
            }
        }
        per_side.push(map);
    }

    let mut all_names: HashSet<String> = HashSet::new();
    for map in &per_side {
        all_names.extend(map.keys().cloned());
    }

    let mut sorted_names: Vec<(bool, String, String)> = all_names
        .into_iter()
        .map(|name| {
            let is_dir = is_directory_for_sort(&per_side, &name);
            let lower = name.to_lowercase();
            (is_dir, lower, name)
        })
        .collect();
    // Deterministic sort: directories first, then case-insensitive name, with
    // exact byte order as tie-breaker on case-sensitive filesystems.
    sorted_names.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    for (_, _, name) in sorted_names {
        if is_cancelled(cancel) {
            return Ok(false);
        }

        let rel_path = join_rel(rel_prefix, &name);
        let children: Vec<Option<&DiscoveredChild>> =
            per_side.iter().map(|map| map.get(&name)).collect();
        let sides: Vec<Option<FolderSideInfo>> = children
            .iter()
            .map(|child| {
                child.map(|c| FolderSideInfo {
                    kind: c.kind,
                    size: c.size,
                })
            })
            .collect();

        let present_kinds: Vec<FolderEntryKind> =
            children.iter().flatten().map(|c| c.kind).collect();
        let primary_kind = present_kinds
            .first()
            .copied()
            .unwrap_or(FolderEntryKind::Other);
        let types_match = present_kinds.iter().all(|k| *k == primary_kind);
        let present_count = children.iter().filter(|c| c.is_some()).count();

        if !types_match {
            out.push(FolderCompareEntry {
                relative_path: rel_path,
                depth,
                kind: primary_kind,
                status: FolderEntryStatus::TypeMismatch,
                sides,
            });
            continue;
        }

        if primary_kind == FolderEntryKind::Directory {
            let child_dirs: Vec<Option<PathBuf>> = children
                .iter()
                .map(|c| c.map(|child| child.full_path.clone()))
                .collect();
            let entry_index = out.len();
            let initial_status =
                single_side_status(&children).unwrap_or(FolderEntryStatus::Identical);
            out.push(FolderCompareEntry {
                relative_path: rel_path.clone(),
                depth,
                kind: FolderEntryKind::Directory,
                status: initial_status,
                sides,
            });

            if !walk_folder_level(
                &child_dirs,
                &rel_path,
                depth + 1,
                options,
                ignore_stacks,
                cancel,
                out,
            )? {
                return Ok(false);
            }

            let status = if let Some(only) = single_side_status(&children) {
                only
            } else if present_count < side_count {
                FolderEntryStatus::Different
            } else if out[(entry_index + 1)..]
                .iter()
                .all(|child| child.status == FolderEntryStatus::Identical)
            {
                FolderEntryStatus::Identical
            } else {
                FolderEntryStatus::Different
            };
            out[entry_index].status = status;
            continue;
        }

        let status = if let Some(only) = single_side_status(&children) {
            only
        } else if present_count < side_count {
            FolderEntryStatus::Different
        } else {
            let equal = match primary_kind {
                FolderEntryKind::File => files_equal(&children, cancel)?,
                FolderEntryKind::Symlink => symlinks_equal(&children)?,
                FolderEntryKind::Other => true,
                FolderEntryKind::Directory => unreachable!(),
            };
            if is_cancelled(cancel) {
                return Ok(false);
            }
            if equal {
                FolderEntryStatus::Identical
            } else {
                FolderEntryStatus::Different
            }
        };

        out.push(FolderCompareEntry {
            relative_path: rel_path,
            depth,
            kind: primary_kind,
            status,
            sides,
        });
    }

    Ok(true)
}

fn is_directory_for_sort(per_side: &[BTreeMap<String, DiscoveredChild>], name: &str) -> bool {
    let mut saw_any = false;
    for map in per_side {
        if let Some(child) = map.get(name) {
            saw_any = true;
            if child.kind != FolderEntryKind::Directory {
                return false;
            }
        }
    }
    saw_any
}

fn single_side_status(children: &[Option<&DiscoveredChild>]) -> Option<FolderEntryStatus> {
    let present: Vec<usize> = children
        .iter()
        .enumerate()
        .filter_map(|(idx, child)| child.map(|_| idx))
        .collect();
    if present.len() != 1 {
        return None;
    }
    let idx = present[0];
    if children.len() == 2 {
        match idx {
            0 => Some(FolderEntryStatus::OnlyLeft),
            1 => Some(FolderEntryStatus::OnlyRight),
            _ => None,
        }
    } else {
        match idx {
            0 => Some(FolderEntryStatus::OnlyLeft),
            1 => Some(FolderEntryStatus::OnlyMiddle),
            2 => Some(FolderEntryStatus::OnlyRight),
            _ => None,
        }
    }
}

fn files_equal(children: &[Option<&DiscoveredChild>], cancel: &AtomicBool) -> Result<bool> {
    let present: Vec<&DiscoveredChild> = children.iter().flatten().copied().collect();
    let Some(first) = present.first() else {
        return Ok(true);
    };
    let first_size = first.size;
    if present.iter().any(|child| child.size != first_size) {
        return Ok(false);
    }
    if first_size == Some(0) {
        return Ok(true);
    }

    let Some(first_hash) = stream_file_hash(&first.full_path, cancel)? else {
        return Ok(false);
    };
    for other in &present[1..] {
        let Some(other_hash) = stream_file_hash(&other.full_path, cancel)? else {
            return Ok(false);
        };
        if other_hash != first_hash {
            return Ok(false);
        }
    }
    Ok(true)
}

fn symlinks_equal(children: &[Option<&DiscoveredChild>]) -> Result<bool> {
    let present: Vec<&DiscoveredChild> = children.iter().flatten().copied().collect();
    let Some(first) = present.first() else {
        return Ok(true);
    };
    let first_target = fs::read_link(&first.full_path)?;
    for other in &present[1..] {
        if fs::read_link(&other.full_path)? != first_target {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Streaming 128-bit FNV-1a hash in 64 KiB chunks so large files do not load
/// into memory and check cancellation on every chunk.
fn stream_file_hash(path: &Path, cancel: &AtomicBool) -> Result<Option<u128>> {
    const OFFSET_128: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME_128: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    let mut file = File::open(path)?;
    let mut buf = [0u8; HASH_CHUNK_BYTES];
    let mut hash = OFFSET_128;
    loop {
        if is_cancelled(cancel) {
            return Ok(None);
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for &byte in &buf[..n] {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(PRIME_128);
        }
    }
    Ok(Some(hash))
}

fn classify_metadata(meta: &fs::Metadata) -> FolderEntryKind {
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        FolderEntryKind::Symlink
    } else if file_type.is_dir() {
        FolderEntryKind::Directory
    } else if file_type.is_file() {
        FolderEntryKind::File
    } else {
        FolderEntryKind::Other
    }
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

// ---------------------------------------------------------------------------
// Shared file helpers
// ---------------------------------------------------------------------------

fn is_cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

fn shared_image_kind(paths: &[Option<&Path>]) -> Option<ImageKind> {
    let mut detected: Option<ImageKind> = None;
    for path in paths.iter().flatten() {
        let kind = image::kind_for_path(&path.to_string_lossy())?;
        match detected {
            None => detected = Some(kind),
            Some(prev) if prev != kind => return None,
            Some(_) => {}
        }
    }
    detected
}

fn is_binary_bytes(bytes: &[u8], encoding: TextEncoding) -> bool {
    !bytes.is_empty() && diff::is_binary(bytes) && !encoding.is_utf16()
}

fn read_image_side(
    path: Option<&Path>,
    kind: ImageKind,
    cancel: &AtomicBool,
) -> Result<Option<ImageSide>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if is_cancelled(cancel) {
        return Ok(None);
    }
    let meta = fs::metadata(path)?;
    let size = meta.len();
    if size > image::MAX_IMAGE_BYTES as u64 {
        return Ok(Some(ImageSide::new(
            kind,
            None,
            size,
            image::MAX_IMAGE_BYTES,
        )));
    }
    let bytes = fs::read(path)?;
    Ok(Some(ImageSide::new(
        kind,
        Some(bytes),
        size,
        image::MAX_IMAGE_BYTES,
    )))
}

fn read_optional_text_bytes(
    path: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<(Option<Vec<u8>>, bool)> {
    let Some(path) = path else {
        return Ok((None, false));
    };
    if is_cancelled(cancel) {
        return Ok((None, false));
    }
    let meta = fs::metadata(path)?;
    if meta.len() > MAX_TEXT_COMPARE_BYTES as u64 {
        return Ok((None, true));
    }
    let bytes = fs::read(path)?;
    Ok((Some(bytes), false))
}

fn highlight_text(highlighter: &mut Highlighter, path: &Path, text: &str) -> Vec<LineHighlights> {
    if text.len() > MAX_HIGHLIGHT_BYTES {
        return Vec::new();
    }
    let Some(language) = language_for_path(&path.to_string_lossy()) else {
        return Vec::new();
    };
    highlighter.highlight(language, text)
}

fn highlight_lines(
    highlighter: &mut Highlighter,
    path: &Path,
    lines: &[String],
    trailing: bool,
) -> Vec<LineHighlights> {
    let Some(language) = language_for_path(&path.to_string_lossy()) else {
        return Vec::new();
    };
    let mut text = lines.join("\n");
    if trailing && !lines.is_empty() {
        text.push('\n');
    }
    if text.len() > MAX_HIGHLIGHT_BYTES {
        return Vec::new();
    }
    highlighter.highlight(language, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::LineKind;
    use tempfile::TempDir;

    fn write_file(dir: &Path, rel: &str, bytes: &[u8]) {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(full, bytes).expect("write file");
    }

    #[test]
    fn two_way_text_diff_produces_hunks_gaps_and_highlights() {
        let tmp = TempDir::new().expect("tempdir");
        let left = tmp.path().join("old.rs");
        let right = tmp.path().join("new.rs");
        fs::write(
            &left,
            "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\nfn f() {}\nfn g() {}\nfn h() {}\nfn i() {}\nfn j() {}\nfn k() {}\n",
        )
        .unwrap();
        fs::write(
            &right,
            "fn a() {}\nfn b_changed() {}\nfn c() {}\nfn d() {}\nfn e() {}\nfn f() {}\nfn g() {}\nfn h() {}\nfn i() {}\nfn j() {}\nfn k() {}\n",
        )
        .unwrap();

        let cancel = AtomicBool::new(false);
        let result = compare_two_files(&left, &right, &DiffOptions::default(), &cancel)
            .expect("compare ok")
            .expect("not cancelled");

        assert!(!result.diff.binary);
        assert!(!result.too_large);
        assert!(result.image.is_none());
        assert_eq!(result.diff.hunks.len(), 1);
        assert!(
            result.diff.hunks[0]
                .lines
                .iter()
                .any(|line| line.kind == LineKind::Removed && line.content.contains("fn b()"))
        );
        assert!(
            result.diff.hunks[0]
                .lines
                .iter()
                .any(|line| line.kind == LineKind::Added && line.content.contains("fn b_changed()"))
        );
        // Trailing context past hunk creates a gap at the end of the file.
        assert_eq!(result.gaps.len(), 2);
        assert!(result.gaps[1].len > 0);
        assert!(!result.left_highlights.is_empty());
        assert!(!result.right_highlights.is_empty());
    }

    #[test]
    fn two_way_detects_encoding_and_binary_and_utf16_as_text() {
        let tmp = TempDir::new().expect("tempdir");
        // UTF-16LE with BOM contains NUL bytes yet is valid text.
        let left_u16 = tmp.path().join("a.txt");
        let right_u16 = tmp.path().join("b.txt");
        let mut a = vec![0xFF, 0xFE];
        for ch in "hello\n".encode_utf16() {
            a.extend_from_slice(&ch.to_le_bytes());
        }
        let mut b = vec![0xFF, 0xFE];
        for ch in "world\n".encode_utf16() {
            b.extend_from_slice(&ch.to_le_bytes());
        }
        fs::write(&left_u16, &a).unwrap();
        fs::write(&right_u16, &b).unwrap();

        let cancel = AtomicBool::new(false);
        let u16_res = compare_two_files(&left_u16, &right_u16, &DiffOptions::default(), &cancel)
            .unwrap()
            .unwrap();
        assert!(!u16_res.diff.binary);
        assert!(u16_res.left_encoding.is_utf16());
        assert_eq!(u16_res.left_text.as_ref(), "hello\n");
        assert_eq!(u16_res.right_text.as_ref(), "world\n");

        // True binary file with NUL bytes.
        let bin_a = tmp.path().join("a.bin");
        let bin_b = tmp.path().join("b.bin");
        fs::write(&bin_a, [0x00, 0x01, 0x02, 0x03]).unwrap();
        fs::write(&bin_b, [0x00, 0x01, 0x04, 0x05]).unwrap();
        let bin_res = compare_two_files(&bin_a, &bin_b, &DiffOptions::default(), &cancel)
            .unwrap()
            .unwrap();
        assert!(bin_res.diff.binary);
        assert!(bin_res.diff.hunks.is_empty());
    }

    #[test]
    fn two_way_flags_raster_and_svg_images() {
        let tmp = TempDir::new().expect("tempdir");
        let png_a = tmp.path().join("a.png");
        let png_b = tmp.path().join("b.png");
        // Minimal PNG header (signature + IHDR 2x3).
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&2u32.to_be_bytes());
        png.extend_from_slice(&3u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        fs::write(&png_a, &png).unwrap();
        fs::write(&png_b, &png).unwrap();

        let cancel = AtomicBool::new(false);
        let png_res = compare_two_files(&png_a, &png_b, &DiffOptions::default(), &cancel)
            .unwrap()
            .unwrap();
        let img = png_res.image.expect("image diff present");
        assert_eq!(img.kind, ImageKind::Raster);
        assert_eq!(img.old.as_ref().and_then(|s| s.width), Some(2));
        assert_eq!(img.old.as_ref().and_then(|s| s.height), Some(3));

        let svg_a = tmp.path().join("a.svg");
        let svg_b = tmp.path().join("b.svg");
        fs::write(&svg_a, "<svg width=\"10\" height=\"10\"></svg>\n").unwrap();
        fs::write(&svg_b, "<svg width=\"20\" height=\"10\"></svg>\n").unwrap();
        let svg_res = compare_two_files(&svg_a, &svg_b, &DiffOptions::default(), &cancel)
            .unwrap()
            .unwrap();
        assert!(!svg_res.diff.binary);
        assert!(!svg_res.diff.hunks.is_empty());
        assert_eq!(svg_res.image.as_ref().map(|i| i.kind), Some(ImageKind::Svg));
    }

    #[test]
    fn three_way_marks_left_right_both_agree_and_both_disagree() {
        let cancel = AtomicBool::new(false);
        let middle = b"line1\nline2\nline3\nsep\nline4\nline5\nline6\nline7\n";
        let left = b"line1-L\nline2\nline3\nsep\nline4-BOTH\nline5\nline6-CONFLICT-L\nline7\n";
        let right = b"line1\nline2\nline3-R\nsep\nline4-BOTH\nline5\nline6-CONFLICT-R\nline7\n";

        let res = compare_three_bytes(
            Path::new("left.txt"),
            Some(left),
            Path::new("mid.txt"),
            Some(middle),
            Path::new("right.txt"),
            Some(right),
            &cancel,
        )
        .expect("not cancelled");

        let kinds: Vec<ThreeWayDiffKind> = res.regions.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ThreeWayDiffKind::LeftChanged,
                ThreeWayDiffKind::Unchanged,
                ThreeWayDiffKind::RightChanged,
                ThreeWayDiffKind::Unchanged,
                ThreeWayDiffKind::BothAgree,
                ThreeWayDiffKind::Unchanged,
                ThreeWayDiffKind::BothDisagree,
                ThreeWayDiffKind::Unchanged,
            ]
        );
        let counts = res.counts();
        assert_eq!(counts.left_changed, 1);
        assert_eq!(counts.right_changed, 1);
        assert_eq!(counts.both_agree, 1);
        assert_eq!(counts.both_disagree, 1);
        assert_eq!(res.rows.len(), 8);
    }

    #[test]
    fn folder_compare_two_way_statuses_sort_git_skip_and_gitignore() {
        let tmp = TempDir::new().expect("tempdir");
        let left = tmp.path().join("left");
        let right = tmp.path().join("right");
        fs::create_dir_all(&left).unwrap();
        fs::create_dir_all(&right).unwrap();

        // Always skipped `.git` directories with differing contents.
        write_file(&left, ".git/HEAD", b"ref: refs/heads/a\n");
        write_file(&right, ".git/HEAD", b"ref: refs/heads/b\n");

        // `.gitignore` rules
        write_file(&left, ".gitignore", b"*.log\nignored_dir/\n");
        write_file(&right, ".gitignore", b"*.log\nignored_dir/\n");
        write_file(&left, "app.log", b"left log");
        write_file(&right, "app.log", b"right log");
        write_file(&left, "ignored_dir/secret.txt", b"a");
        write_file(&right, "ignored_dir/secret.txt", b"b");

        // Regular files and nested folders
        write_file(&left, "Zebra.txt", b"same");
        write_file(&right, "Zebra.txt", b"same");
        write_file(&left, "alpha.txt", b"old");
        write_file(&right, "alpha.txt", b"new");
        write_file(&left, "only_left.txt", b"l");
        write_file(&right, "only_right.txt", b"r");
        write_file(&left, "sub/inner.txt", b"same");
        write_file(&right, "sub/inner.txt", b"same");
        write_file(&left, "sub/.gitignore", b"/skip.txt\n");
        write_file(&right, "sub/.gitignore", b"/skip.txt\n");
        write_file(&left, "sub/skip.txt", b"ignored inside sub");
        write_file(&right, "sub/skip.txt", b"ignored inside sub");
        write_file(&left, "mismatch/item", b"file");
        write_file(&right, "mismatch/item/nested.txt", b"dir");

        let cancel = AtomicBool::new(false);
        let res = compare_folders(
            &[&left, &right],
            FolderCompareOptions {
                respect_gitignore: true,
            },
            &cancel,
        )
        .unwrap()
        .unwrap();

        let paths_and_statuses: Vec<(&str, FolderEntryStatus)> = res
            .entries
            .iter()
            .map(|e| (e.relative_path.as_str(), e.status))
            .collect();

        // Directories first (`mismatch`, `sub`), case-insensitive file names after.
        assert_eq!(
            paths_and_statuses,
            vec![
                ("mismatch", FolderEntryStatus::Different),
                ("mismatch/item", FolderEntryStatus::TypeMismatch),
                ("sub", FolderEntryStatus::Identical),
                ("sub/.gitignore", FolderEntryStatus::Identical),
                ("sub/inner.txt", FolderEntryStatus::Identical),
                (".gitignore", FolderEntryStatus::Identical),
                ("alpha.txt", FolderEntryStatus::Different),
                ("only_left.txt", FolderEntryStatus::OnlyLeft),
                ("only_right.txt", FolderEntryStatus::OnlyRight),
                ("Zebra.txt", FolderEntryStatus::Identical),
            ]
        );

        // When `respect_gitignore` is off, `.log`, nested ignored files, and `ignored_dir` show up (`.git` still skipped).
        let unignored = compare_folders(
            &[&left, &right],
            FolderCompareOptions {
                respect_gitignore: false,
            },
            &cancel,
        )
        .unwrap()
        .unwrap();
        assert!(
            unignored
                .entries
                .iter()
                .any(|e| e.relative_path == "app.log")
        );
        assert!(
            unignored
                .entries
                .iter()
                .any(|e| e.relative_path == "sub/skip.txt")
        );
        assert!(
            unignored
                .entries
                .iter()
                .any(|e| e.relative_path == "ignored_dir/secret.txt")
        );
        assert!(
            !unignored
                .entries
                .iter()
                .any(|e| e.relative_path.starts_with(".git/"))
                && !unignored.entries.iter().any(|e| e.relative_path == ".git")
        );
    }

    #[test]
    fn folder_compare_three_way_and_cancellation() {
        let tmp = TempDir::new().expect("tempdir");
        let left = tmp.path().join("left");
        let middle = tmp.path().join("middle");
        let right = tmp.path().join("right");
        fs::create_dir_all(&left).unwrap();
        fs::create_dir_all(&middle).unwrap();
        fs::create_dir_all(&right).unwrap();

        write_file(&left, "same.txt", b"v1");
        write_file(&middle, "same.txt", b"v1");
        write_file(&right, "same.txt", b"v1");

        write_file(&left, "changed.txt", b"v2-left");
        write_file(&middle, "changed.txt", b"v1");
        write_file(&right, "changed.txt", b"v1");

        write_file(&left, "l.txt", b"1");
        write_file(&middle, "m.txt", b"1");
        write_file(&right, "r.txt", b"1");

        let cancel = AtomicBool::new(false);
        let res = compare_folders(
            &[&left, &middle, &right],
            FolderCompareOptions::default(),
            &cancel,
        )
        .unwrap()
        .unwrap();
        assert_eq!(res.counts.identical, 1);
        assert_eq!(res.counts.different, 1);
        assert_eq!(res.counts.only_left, 1);
        assert_eq!(res.counts.only_middle, 1);
        assert_eq!(res.counts.only_right, 1);

        let cancelled = AtomicBool::new(true);
        assert!(
            compare_folders(
                &[&left, &middle, &right],
                FolderCompareOptions::default(),
                &cancelled
            )
            .unwrap()
            .is_none()
        );
        assert!(
            compare_two_files(
                &left.join("same.txt"),
                &right.join("same.txt"),
                &DiffOptions::default(),
                &cancelled
            )
            .unwrap()
            .is_none()
        );
        assert!(
            compare_three_files(
                &left.join("same.txt"),
                &middle.join("same.txt"),
                &right.join("same.txt"),
                &cancelled
            )
            .unwrap()
            .is_none()
        );
    }
}
