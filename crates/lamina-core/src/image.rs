//! Raw bytes for the image diff view.
//!
//! The diff worker hands both sides over as content rather than as a text
//! diff, because a raster image has no lines to compare. Dimensions are read
//! from the file header here: the diff pane labels every picture with its size
//! and the 1:1 mode needs the pixel size even before the UI decodes anything.
//!
//! This module never decodes pixels. The app renders the bytes with the image
//! support it already has, and only the header bytes are inspected here.

use std::path::Path;
use std::sync::Arc;

use crate::lfs::{self, LfsDiffSide, LfsDiffSummary, LfsPointer};

/// Larger images are offered by size only. Handing a multi-megabyte picture to
/// a view that has to decode and upload it costs more than the preview is
/// worth, and the user still sees what changed.
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// How the diff pane renders a file whose extension names a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// A bitmap: never shows a text diff.
    Raster,
    /// XML that is also text: the diff stays available beside the rendering.
    Svg,
}

/// A picture format this module recognises from the content's magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
    Ico,
    Svg,
}

/// The formats [`kind_for_path`] routes to the image view.
pub fn kind_for_path(path: &str) -> Option<ImageKind> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" => Some(ImageKind::Raster),
        "svg" => Some(ImageKind::Svg),
        _ => None,
    }
}

/// One side of an image diff.
#[derive(Clone)]
pub struct ImageSide {
    /// The content, or `None` when it is over the cap or an LFS object is not
    /// local. `size` still describes the file.
    pub bytes: Option<Arc<[u8]>>,
    /// Size of the content in bytes.
    pub size: u64,
    /// Recognised from the content, `None` when the bytes are absent or are
    /// not a format the app can render.
    pub format: Option<ImageFormat>,
    /// Pixel dimensions, from the header. `None` when they are not known.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl std::fmt::Debug for ImageSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The content can be megabytes; a debug line names its length instead.
        f.debug_struct("ImageSide")
            .field("bytes", &self.bytes.as_ref().map(|bytes| bytes.len()))
            .field("size", &self.size)
            .field("format", &self.format)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl ImageSide {
    /// A side from content that may or may not be available.
    ///
    /// `bytes` is `None` when the content must not be read (over the cap, or
    /// an LFS object that is not local); `size` is then the size the pointer
    /// or the worktree reports.
    pub fn new(kind: ImageKind, bytes: Option<Vec<u8>>, size: u64, max_bytes: usize) -> Self {
        let Some(bytes) = bytes else {
            return Self {
                bytes: None,
                size,
                format: None,
                width: None,
                height: None,
            };
        };
        let size = bytes.len() as u64;
        if bytes.len() > max_bytes {
            return Self {
                bytes: None,
                size,
                format: None,
                width: None,
                height: None,
            };
        }
        let (format, dimensions) = sniff(kind, &bytes);
        let (width, height) = match dimensions {
            Some((width, height)) => (Some(width), Some(height)),
            None => (None, None),
        };
        Self {
            bytes: Some(Arc::from(bytes)),
            size,
            format,
            width,
            height,
        }
    }
}

/// Both sides of an image diff and the LFS card that goes with them.
#[derive(Debug, Clone)]
pub struct ImageDiff {
    pub kind: ImageKind,
    pub old: Option<ImageSide>,
    pub new: Option<ImageSide>,
}

/// An [`ImageDiff`] plus the LFS metadata the pane shows above it.
#[derive(Debug, Clone)]
pub struct ResolvedImage {
    pub diff: ImageDiff,
    pub lfs: Option<LfsDiffSummary>,
}

/// Resolve both sides as the text diff would, one side at a time.
///
/// [`lfs::resolve_diff`] insists on real content on both sides because a text
/// diff needs two strings; an added or deleted image has only one side, and
/// the view draws the side it has.
pub fn resolve(
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    kind: ImageKind,
    common_dir: &Path,
    max_bytes: usize,
) -> ResolvedImage {
    let (old, old_lfs) = resolve_side(old, kind, common_dir, max_bytes);
    let (new, new_lfs) = resolve_side(new, kind, common_dir, max_bytes);
    let lfs = (old_lfs.is_some() || new_lfs.is_some()).then(|| lfs::summarize(old_lfs, new_lfs));
    ResolvedImage {
        diff: ImageDiff { kind, old, new },
        lfs,
    }
}

/// The image sides for bytes that were already resolved for the text diff.
///
/// SVG keeps its text diff, so the worker hands over the LFS-resolved content
/// it diffed rather than reading the objects a second time. `old_present` and
/// `new_present` come from the side read, not from the content length, because
/// a missing LFS object leaves a present side with no bytes.
pub fn from_text_inputs(
    kind: ImageKind,
    old_present: bool,
    new_present: bool,
    old: &[u8],
    new: &[u8],
    max_bytes: usize,
) -> ImageDiff {
    let side = |present: bool, bytes: &[u8]| {
        present.then(|| {
            let content = (!bytes.is_empty()).then(|| bytes.to_vec());
            ImageSide::new(kind, content, bytes.len() as u64, max_bytes)
        })
    };
    ImageDiff {
        kind,
        old: side(old_present, old),
        new: side(new_present, new),
    }
}

fn resolve_side(
    bytes: Option<&[u8]>,
    kind: ImageKind,
    common_dir: &Path,
    max_bytes: usize,
) -> (Option<ImageSide>, Option<LfsDiffSide>) {
    let Some(bytes) = bytes else {
        return (None, None);
    };
    let Some(pointer) = LfsPointer::parse(bytes) else {
        return (
            Some(ImageSide::new(
                kind,
                Some(bytes.to_vec()),
                bytes.len() as u64,
                max_bytes,
            )),
            None,
        );
    };

    let side = LfsDiffSide {
        oid: pointer.oid.clone(),
        size: pointer.size,
        present: pointer.present(common_dir),
    };
    let object = lfs::read_object(common_dir, &pointer, max_bytes)
        .ok()
        .flatten();
    (
        Some(ImageSide::new(kind, object, pointer.size, max_bytes)),
        Some(side),
    )
}

/// The header's pixel dimensions, when the format is one this module knows.
pub fn probe_dimensions(kind: ImageKind, data: &[u8]) -> Option<(u32, u32)> {
    sniff(kind, data).1
}

fn sniff(kind: ImageKind, data: &[u8]) -> (Option<ImageFormat>, Option<(u32, u32)>) {
    match kind {
        ImageKind::Svg => (
            Some(ImageFormat::Svg),
            svg_dimensions(data).filter(|(width, height)| *width > 0 && *height > 0),
        ),
        ImageKind::Raster => {
            if data.starts_with(b"\x89PNG\r\n\x1a\n") {
                return (Some(ImageFormat::Png), png_dimensions(data));
            }
            if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
                return (Some(ImageFormat::Gif), gif_dimensions(data));
            }
            if data.starts_with(&[0xFF, 0xD8]) {
                return (Some(ImageFormat::Jpeg), jpeg_dimensions(data));
            }
            if data.starts_with(b"BM") {
                return (Some(ImageFormat::Bmp), bmp_dimensions(data));
            }
            if data.len() >= 4 && data[0..4] == [0, 0, 1, 0] {
                return (Some(ImageFormat::Ico), ico_dimensions(data));
            }
            if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
                return (Some(ImageFormat::Webp), webp_dimensions(data));
            }
            (None, None)
        }
    }
}

fn be_u32(data: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(0..4)?.try_into().ok()?))
}

fn le_u16(data: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(0..2)?.try_into().ok()?))
}

fn le_u32(data: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(0..4)?.try_into().ok()?))
}

fn be_u16(data: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(0..2)?.try_into().ok()?))
}

fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // The first chunk of a valid PNG is IHDR, so its fields sit at fixed
    // offsets after the 8-byte signature and the 8-byte length/type header.
    if data.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be_u32(data.get(16..20)?)?, be_u32(data.get(20..24)?)?))
}

fn gif_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let width = u32::from(le_u16(data.get(6..8)?)?);
    let height = u32::from(le_u16(data.get(8..10)?)?);
    Some((width, height))
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2;
    while index + 4 <= data.len() {
        if data[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = data[index + 1];
        // Standalone markers have no length field.
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            index += 2;
            continue;
        }
        // A start-of-scan means every frame header before it has been walked.
        if marker == 0xDA {
            return None;
        }
        let length = usize::from(be_u16(data.get(index + 2..index + 4)?)?);
        if length < 2 {
            return None;
        }
        let is_frame_header = matches!(
            marker,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        );
        if is_frame_header {
            let height = u32::from(be_u16(data.get(index + 5..index + 7)?)?);
            let width = u32::from(be_u16(data.get(index + 7..index + 9)?)?);
            return Some((width, height));
        }
        index += 2 + length;
    }
    None
}

fn bmp_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let header = le_u32(data.get(14..18)?)?;
    if header == 12 {
        let width = u32::from(le_u16(data.get(18..20)?)?);
        let height = u32::from(le_u16(data.get(20..22)?)?);
        return Some((width, height));
    }
    let width = le_u32(data.get(18..22)?)? as i32;
    let height = le_u32(data.get(22..26)?)? as i32;
    // A negative height means the rows are stored bottom-up.
    Some((width.unsigned_abs(), height.unsigned_abs()))
}

fn ico_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let width = u32::from(*data.get(6)?);
    let height = u32::from(*data.get(7)?);
    // Zero stands for 256 in the directory entry.
    Some((
        if width == 0 { 256 } else { width },
        if height == 0 { 256 } else { height },
    ))
}

fn webp_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    match data.get(12..16)? {
        b"VP8X" => {
            let width = 1
                + u32::from(*data.get(24)?)
                + (u32::from(*data.get(25)?) << 8)
                + (u32::from(*data.get(26)?) << 16);
            let height = 1
                + u32::from(*data.get(27)?)
                + (u32::from(*data.get(28)?) << 8)
                + (u32::from(*data.get(29)?) << 16);
            Some((width, height))
        }
        b"VP8L" => {
            let bits = data.get(21..25)?;
            let width = 1 + (u32::from(bits[0]) | (u32::from(bits[1] & 0x3F) << 8));
            let height = 1
                + ((u32::from(bits[1]) >> 6)
                    | (u32::from(bits[2]) << 2)
                    | (u32::from(bits[3] & 0x0F) << 10));
            Some((width, height))
        }
        b"VP8 " => {
            // The frame header's start code sits between the chunk header and
            // the dimensions, which are 14-bit values.
            if data.get(23..26)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            let width = u32::from(le_u16(data.get(26..28)?)? & 0x3FFF);
            let height = u32::from(le_u16(data.get(28..30)?)? & 0x3FFF);
            Some((width, height))
        }
        _ => None,
    }
}

fn svg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(data).ok()?;
    let start = text.find("<svg")?;
    let tag = &text[start..start + text[start..].find('>')?];

    let attribute = |name: &str| -> Option<&str> {
        let mut search = tag;
        loop {
            let index = search.find(name)?;
            let before = search[..index].chars().next_back();
            let after = search[index + name.len()..].trim_start();
            if before.is_some_and(char::is_whitespace) && after.starts_with('=') {
                let value = after[1..].trim_start();
                let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
                let value = &value[1..];
                return value.find(quote).map(|end| &value[..end]);
            }
            search = &search[index + name.len()..];
        }
    };

    if let (Some(width), Some(height)) = (attribute("width"), attribute("height"))
        && let (Some(width), Some(height)) = (svg_length(width), svg_length(height))
    {
        return Some((width, height));
    }

    let view_box = attribute("viewBox").or_else(|| attribute("viewbox"))?;
    let values: Vec<f32> = view_box
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|value| !value.is_empty())
        .filter_map(|value| value.parse().ok())
        .collect();
    match values.as_slice() {
        [_, _, width, height] => Some((round_px(*width), round_px(*height))),
        _ => None,
    }
}

/// A CSS length without a unit, or in pixels. Other units are refused rather
/// than guessed at, and the caller falls back to the view box.
fn svg_length(value: &str) -> Option<u32> {
    let value = value.trim();
    let number = value.strip_suffix("px").unwrap_or(value);
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    number.parse::<f32>().ok().map(round_px)
}

fn round_px(value: f32) -> u32 {
    value.max(0.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn gif_header(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 3]);
        bytes
    }

    fn bmp_header(width: i32, height: i32) -> Vec<u8> {
        let mut bytes = b"BM".to_vec();
        bytes.extend_from_slice(&[0u8; 12]);
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        bytes
    }

    fn ico_header(width: u8, height: u8) -> Vec<u8> {
        vec![0, 0, 1, 0, 1, 0, width, height, 0, 0, 1, 0, 32, 0]
    }

    fn webp_vp8x_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(b"VP8X");
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(&[0u8; 4]);
        let width = width - 1;
        let height = height - 1;
        bytes.push(width as u8);
        bytes.push((width >> 8) as u8);
        bytes.push((width >> 16) as u8);
        bytes.push(height as u8);
        bytes.push((height >> 8) as u8);
        bytes.push((height >> 16) as u8);
        bytes
    }

    fn jpeg_header(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0];
        bytes.extend_from_slice(&17u16.to_be_bytes());
        bytes.push(8);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.push(3);
        bytes.extend_from_slice(&[0u8; 9]);
        bytes
    }

    #[test]
    fn only_image_extensions_take_the_image_route() {
        for path in [
            "a.png",
            "dir/photo.JPG",
            "b.jpeg",
            "c.gif",
            "d.webp",
            "e.bmp",
            "f.ico",
        ] {
            assert_eq!(
                kind_for_path(path),
                Some(ImageKind::Raster),
                "{path} is a raster image"
            );
        }
        assert_eq!(kind_for_path("logo.svg"), Some(ImageKind::Svg));
        assert_eq!(kind_for_path("LOGO.SVG"), Some(ImageKind::Svg));
        for path in ["a.rs", "b.txt", "archive.tar.gz", "no-extension", ".png"] {
            assert_eq!(kind_for_path(path), None, "{path} is not an image");
        }
    }

    #[test]
    fn raster_headers_give_their_dimensions() {
        assert_eq!(
            probe_dimensions(ImageKind::Raster, &png_header(640, 480)),
            Some((640, 480))
        );
        assert_eq!(
            probe_dimensions(ImageKind::Raster, &gif_header(320, 200)),
            Some((320, 200))
        );
        assert_eq!(
            probe_dimensions(ImageKind::Raster, &jpeg_header(1024, 768)),
            Some((1024, 768))
        );
        assert_eq!(
            probe_dimensions(ImageKind::Raster, &bmp_header(64, -32)),
            Some((64, 32)),
            "a negative height is a bottom-up bitmap, not a negative size"
        );
        assert_eq!(
            probe_dimensions(ImageKind::Raster, &ico_header(0, 48)),
            Some((256, 48)),
            "zero in an icon directory entry means 256"
        );
        assert_eq!(
            probe_dimensions(ImageKind::Raster, &webp_vp8x_header(300, 150)),
            Some((300, 150))
        );
    }

    #[test]
    fn svg_dimensions_come_from_attributes_or_the_view_box() {
        let sized = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="16"></svg>"#;
        assert_eq!(probe_dimensions(ImageKind::Svg, sized), Some((24, 16)));

        let boxed = br#"<svg viewBox="0 0 32 20" xmlns="http://www.w3.org/2000/svg"></svg>"#;
        assert_eq!(probe_dimensions(ImageKind::Svg, boxed), Some((32, 20)));

        let percent = br#"<svg width="100%" height="100%" viewBox="0,0,10,5"></svg>"#;
        assert_eq!(
            probe_dimensions(ImageKind::Svg, percent),
            Some((10, 5)),
            "a percentage has no pixel size, so the view box answers"
        );

        let unparseable = br"<svg></svg>";
        assert_eq!(probe_dimensions(ImageKind::Svg, unparseable), None);
    }

    #[test]
    fn content_over_the_cap_keeps_only_its_metadata() {
        let bytes = vec![0u8; 32];
        let side = ImageSide::new(ImageKind::Raster, Some(bytes), 32, 16);
        assert!(side.bytes.is_none());
        assert_eq!(side.size, 32);
        assert_eq!(side.format, None);
        assert_eq!((side.width, side.height), (None, None));

        let missing = ImageSide::new(ImageKind::Raster, None, 4096, 16);
        assert!(missing.bytes.is_none());
        assert_eq!(missing.size, 4096);
    }

    #[test]
    fn an_added_or_deleted_image_has_one_side() {
        let added = resolve(
            None,
            Some(&png_header(10, 20)),
            ImageKind::Raster,
            Path::new("/nonexistent"),
            MAX_IMAGE_BYTES,
        );
        assert!(added.diff.old.is_none());
        let new = added.diff.new.expect("new side");
        assert_eq!((new.width, new.height), (Some(10), Some(20)));
        assert_eq!(new.format, Some(ImageFormat::Png));
        assert!(added.lfs.is_none());

        let deleted = resolve(
            Some(&png_header(10, 20)),
            None,
            ImageKind::Raster,
            Path::new("/nonexistent"),
            MAX_IMAGE_BYTES,
        );
        assert!(deleted.diff.new.is_none());
        assert!(deleted.diff.old.is_some());
    }

    #[test]
    fn an_lfs_pointer_resolves_to_its_object() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let oid = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let content = png_header(800, 600);
        let pointer = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            content.len()
        );
        let object = temp
            .path()
            .join("lfs/objects")
            .join(&oid[0..2])
            .join(&oid[2..4])
            .join(oid);
        std::fs::create_dir_all(object.parent().expect("parent")).expect("object dir");
        std::fs::write(&object, &content).expect("object");

        let resolved = resolve(
            Some(pointer.as_bytes()),
            Some(&png_header(1, 1)),
            ImageKind::Raster,
            temp.path(),
            MAX_IMAGE_BYTES,
        );
        let old = resolved.diff.old.expect("old side");
        assert_eq!(
            old.bytes.as_deref(),
            Some(content.as_slice()),
            "the diff shows the object, not the pointer"
        );
        assert_eq!((old.width, old.height), (Some(800), Some(600)));
        let summary = resolved.lfs.expect("an LFS card");
        assert!(summary.old.expect("old lfs side").present);
        assert!(
            summary.text.starts_with("LFS object removed"),
            "one pointer side still gets a card: {}",
            summary.text
        );
    }

    #[test]
    fn a_missing_lfs_object_still_names_the_file() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let oid = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 2048\n");

        let resolved = resolve(
            Some(pointer.as_bytes()),
            None,
            ImageKind::Raster,
            temp.path(),
            MAX_IMAGE_BYTES,
        );
        let old = resolved.diff.old.expect("old side");
        assert!(old.bytes.is_none());
        assert_eq!(old.size, 2048);
        let summary = resolved.lfs.expect("an LFS card");
        assert!(!summary.old.expect("old lfs side").present);
        assert!(summary.text.starts_with("LFS object removed"));
    }

    #[test]
    fn an_svg_sided_from_text_inputs_keeps_its_rendering() {
        let old = br#"<svg width="8" height="8"></svg>"#;
        let new = br#"<svg width="16" height="12"></svg>"#;
        let diff = from_text_inputs(ImageKind::Svg, true, true, old, new, MAX_IMAGE_BYTES);
        let side = diff.new.expect("new side");
        assert_eq!(side.format, Some(ImageFormat::Svg));
        assert_eq!((side.width, side.height), (Some(16), Some(12)));

        let added = from_text_inputs(ImageKind::Svg, false, true, b"", new, MAX_IMAGE_BYTES);
        assert!(added.old.is_none());
        assert!(added.new.is_some());
    }
}
