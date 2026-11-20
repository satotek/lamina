//! Git LFS pointer parsing and object-store access.
//!
//! LFS keeps a small pointer blob in history and the real content under
//! `.git/lfs/objects`. A diff or a file viewer that renders the pointer as if
//! it were the file shows twelve lines of metadata instead of the content, so
//! every read path checks [`LfsPointer::parse`] first.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Result};

/// The first line every LFS pointer starts with.
pub const POINTER_HEADER: &str = "version https://git-lfs.github.com/spec/v1";

/// Pointer blobs are tiny; anything larger is file content that happens to
/// mention LFS, not a pointer.
pub const MAX_POINTER_BYTES: usize = 1024;

/// A parsed `version`/`oid sha256:`/`size` pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsPointer {
    /// The full object id, including its `sha256:` prefix.
    pub oid: String,
    /// Size of the real object in bytes.
    pub size: u64,
}

impl LfsPointer {
    /// Parse pointer bytes, returning `None` for malformed or ordinary files.
    ///
    /// Extra keys are accepted: the pointer format is a list of `key value`
    /// lines, and future versions may add keys before `oid` or `size`. A CRLF
    /// checkout is still the same pointer, so a trailing `\r` is trimmed.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() > MAX_POINTER_BYTES {
            return None;
        }
        let text = std::str::from_utf8(data).ok()?;
        let mut lines = text
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line));
        if lines.next()? != POINTER_HEADER {
            return None;
        }

        let mut oid = None;
        let mut size = None;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once(' ')?;
            match key {
                "oid" => {
                    if oid.is_some() {
                        return None;
                    }
                    let hex = value.strip_prefix("sha256:")?;
                    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        return None;
                    }
                    oid = Some(value.to_owned());
                }
                "size" => {
                    if size.is_some()
                        || value.is_empty()
                        || !value.bytes().all(|b| b.is_ascii_digit())
                    {
                        return None;
                    }
                    size = Some(value.parse().ok()?);
                }
                // Extra keys carry no meaning for the content itself.
                _ => {}
            }
        }
        Some(Self {
            oid: oid?,
            size: size?,
        })
    }

    /// The hex part of the object id, without the `sha256:` prefix.
    pub fn hex(&self) -> &str {
        self.oid
            .strip_prefix("sha256:")
            .unwrap_or(self.oid.as_str())
    }

    /// Whether the object exists at `common_dir/lfs/objects/aa/bb/<oid>`.
    pub fn present(&self, common_dir: &Path) -> bool {
        object_path(common_dir, self)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }
}

/// Pointer metadata as the UI needs it: the object's size and whether it can be
/// read without downloading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsInfo {
    /// `sha256:…` for a pointer; empty for a real worktree file whose LFS
    /// attribute marks it without a pointer to read.
    pub oid: String,
    pub size: u64,
    pub present: bool,
}

impl LfsInfo {
    pub fn from_pointer(pointer: &LfsPointer, common_dir: &Path) -> Self {
        Self {
            oid: pointer.oid.clone(),
            size: pointer.size,
            present: pointer.present(common_dir),
        }
    }
}

/// One side of an LFS diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsDiffSide {
    pub oid: String,
    pub size: u64,
    pub present: bool,
}

/// What the diff pane shows instead of pointer text: both sides' sizes and
/// object ids, and a one-line summary the status area can also use verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsDiffSummary {
    pub old: Option<LfsDiffSide>,
    pub new: Option<LfsDiffSide>,
    pub text: String,
}

/// How two blobs resolve when either side may be an LFS pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LfsDiff {
    /// Neither side is a pointer: diff the bytes normally.
    NotLfs,
    /// Both sides resolved to real content: diff these bytes, but still show
    /// the LFS card above the diff.
    Real {
        old: Vec<u8>,
        new: Vec<u8>,
        summary: LfsDiffSummary,
    },
    /// At least one object is missing or too large: show the summary only.
    Summary(LfsDiffSummary),
}

/// Resolve two blob byte slices for diffing.
///
/// A side that parses as a pointer is replaced by its object when the object is
/// present and no larger than `max_bytes`. The worktree side of a staged or
/// unstaged diff is real content, not a pointer, and is used as-is.
pub fn resolve_diff(
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    common_dir: &Path,
    max_bytes: usize,
) -> LfsDiff {
    let old_pointer = old.and_then(LfsPointer::parse);
    let new_pointer = new.and_then(LfsPointer::parse);
    if old_pointer.is_none() && new_pointer.is_none() {
        return LfsDiff::NotLfs;
    }

    // A real worktree file on one side still has a size to show, but no object
    // id: it is checked-out content, not something LFS named.
    let old_side = pointer_side(old_pointer.as_ref(), common_dir).or_else(|| old.map(real_side));
    let new_side = pointer_side(new_pointer.as_ref(), common_dir).or_else(|| new.map(real_side));
    let summary = summarize(old_side, new_side);

    let old_real = match &old_pointer {
        Some(pointer) => read_object(common_dir, pointer, max_bytes).ok().flatten(),
        None => old.map(<[u8]>::to_vec),
    };
    let new_real = match &new_pointer {
        Some(pointer) => read_object(common_dir, pointer, max_bytes).ok().flatten(),
        None => new.map(<[u8]>::to_vec),
    };

    match (old_real, new_real) {
        (Some(old), Some(new)) => LfsDiff::Real { old, new, summary },
        _ => LfsDiff::Summary(summary),
    }
}

fn pointer_side(pointer: Option<&LfsPointer>, common_dir: &Path) -> Option<LfsDiffSide> {
    pointer.map(|pointer| LfsDiffSide {
        oid: pointer.oid.clone(),
        size: pointer.size,
        present: pointer.present(common_dir),
    })
}

/// Both sides' metadata plus the one-line summary a card shows.
///
/// The image diff resolves each side on its own (an added image has no old
/// side at all), so it builds the summary from the same pieces the text diff
/// uses instead of going through [`resolve_diff`].
pub fn summarize(old: Option<LfsDiffSide>, new: Option<LfsDiffSide>) -> LfsDiffSummary {
    LfsDiffSummary {
        text: describe(&old, &new),
        old,
        new,
    }
}

fn real_side(bytes: &[u8]) -> LfsDiffSide {
    LfsDiffSide {
        oid: String::new(),
        size: bytes.len() as u64,
        present: true,
    }
}

fn describe(old: &Option<LfsDiffSide>, new: &Option<LfsDiffSide>) -> String {
    let ids = |old: &LfsDiffSide, new: &LfsDiffSide| {
        if old.oid.is_empty() || new.oid.is_empty() {
            String::new()
        } else {
            format!(" ({} → {})", short_label(&old.oid), short_label(&new.oid))
        }
    };
    match (old, new) {
        (Some(old), Some(new)) => format!(
            "LFS object changed: {} → {}{}",
            format_size(old.size),
            format_size(new.size),
            ids(old, new),
        ),
        (None, Some(new)) if !new.oid.is_empty() => format!(
            "LFS object added: {} ({})",
            format_size(new.size),
            short_label(&new.oid)
        ),
        (Some(old), None) if !old.oid.is_empty() => format!(
            "LFS object removed: {} ({})",
            format_size(old.size),
            short_label(&old.oid)
        ),
        (None, Some(new)) => format!("LFS object added: {}", format_size(new.size)),
        (Some(old), None) => format!("LFS object removed: {}", format_size(old.size)),
        (None, None) => "LFS object changed".to_owned(),
    }
}

/// `sha256:0123456` for a card, full ids stay in the fields.
pub fn short_label(oid: &str) -> String {
    let hex = oid.strip_prefix("sha256:").unwrap_or(oid);
    let short: String = hex.chars().take(7).collect();
    if oid.starts_with("sha256:") {
        format!("sha256:{short}")
    } else {
        short
    }
}

/// Human size in the same units the rest of the app uses.
pub fn format_size(size: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let size_f = size as f64;
    if size_f < KB {
        format!("{size} B")
    } else if size_f < MB {
        format!("{:.1} KB", size_f / KB)
    } else if size_f < GB {
        format!("{:.1} MB", size_f / MB)
    } else {
        format!("{:.1} GB", size_f / GB)
    }
}

/// `common_dir/lfs/objects/aa/bb/<hex>`.
pub fn object_path(common_dir: &Path, pointer: &LfsPointer) -> Option<PathBuf> {
    let hex = pointer.hex();
    if hex.len() < 4 {
        return None;
    }
    Some(
        common_dir
            .join("lfs")
            .join("objects")
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(hex),
    )
}

/// Read the real object for `pointer`, or `None` when it is absent or larger
/// than `max_bytes`.
pub fn read_object(
    common_dir: &Path,
    pointer: &LfsPointer,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let Some(path) = object_path(common_dir, pointer) else {
        return Ok(None);
    };
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= max_bytes as u64 => {
            Ok(Some(std::fs::read(path)?))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Io(error)),
    }
}

/// A lock as `git lfs locks --json` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsLock {
    pub id: String,
    pub path: String,
    pub owner: Option<String>,
    pub locked_at: Option<String>,
}

/// Parse the JSON object `git lfs locks --json` prints.
pub fn parse_locks_json(json: &str) -> Result<Vec<LfsLock>> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        locks: Vec<Lock>,
    }

    #[derive(Deserialize)]
    struct Lock {
        #[serde(default)]
        id: String,
        path: String,
        #[serde(default)]
        locked_at: Option<String>,
        #[serde(default)]
        owner: Option<Owner>,
    }

    #[derive(Deserialize)]
    struct Owner {
        #[serde(default)]
        name: Option<String>,
    }

    let response: Response = serde_json::from_str(json).map_err(|error| {
        Error::other(format!("could not parse `git lfs locks --json`: {error}"))
    })?;
    Ok(response
        .locks
        .into_iter()
        .map(|lock| LfsLock {
            id: lock.id,
            path: lock.path,
            owner: lock.owner.and_then(|owner| owner.name),
            locked_at: lock.locked_at,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";

    fn pointer(oid: &str, size: u64) -> String {
        format!("{POINTER_HEADER}\noid sha256:{oid}\nsize {size}\n")
    }

    #[test]
    fn parses_a_valid_pointer() {
        let parsed = LfsPointer::parse(pointer(OID, 1234).as_bytes()).expect("valid pointer");
        assert_eq!(parsed.oid, format!("sha256:{OID}"));
        assert_eq!(parsed.size, 1234);
        assert_eq!(parsed.hex(), OID);
    }

    #[test]
    fn parses_a_crlf_pointer() {
        let crlf = format!("{POINTER_HEADER}\r\noid sha256:{OID}\r\nsize 42\r\n");
        let parsed = LfsPointer::parse(crlf.as_bytes()).expect("CRLF pointer");
        assert_eq!(parsed.size, 42);
    }

    #[test]
    fn parses_extra_keys_in_any_order() {
        let text = format!(
            "{POINTER_HEADER}\nsize 7\nx-custom value\ndownload https://example.invalid\noid sha256:{OID}\n"
        );
        let parsed = LfsPointer::parse(text.as_bytes()).expect("extra keys");
        assert_eq!(parsed.size, 7);
        assert_eq!(parsed.oid, format!("sha256:{OID}"));
    }

    #[test]
    fn rejects_malformed_pointers() {
        assert!(LfsPointer::parse(b"").is_none());
        assert!(
            LfsPointer::parse(
                b"version https://git-lfs.github.com/spec/v2\noid sha256:00\nsize 1\n"
            )
            .is_none()
        );
        assert!(LfsPointer::parse(pointer("abcd", 1).as_bytes()).is_none());
        assert!(
            LfsPointer::parse(format!("{POINTER_HEADER}\noid sha256:{OID}\n").as_bytes()).is_none()
        );
        assert!(
            LfsPointer::parse(format!("{POINTER_HEADER}\noid sha256:{OID}\nsize -1\n").as_bytes())
                .is_none()
        );
        assert!(
            LfsPointer::parse(
                format!("{POINTER_HEADER}\noid sha256:{OID}\nsize 1\nsize 2\n").as_bytes()
            )
            .is_none()
        );
        // A normal file that merely starts with the header line is not enough.
        assert!(
            LfsPointer::parse(format!("{POINTER_HEADER}\nthis is ordinary text\n").as_bytes())
                .is_none()
        );
        let too_large = vec![b'x'; MAX_POINTER_BYTES + 1];
        assert!(LfsPointer::parse(&too_large).is_none());
    }

    #[test]
    fn summaries_name_sizes_and_short_oids() {
        let old = LfsDiffSide {
            oid: format!("sha256:{OID}"),
            size: 1200,
            present: true,
        };
        let new = LfsDiffSide {
            oid: format!("sha256:{}", OID.replace('4', "5")),
            size: 1300,
            present: false,
        };
        let text = describe(&Some(old), &Some(new.clone()));
        assert!(text.starts_with("LFS object changed: 1.2 KB → 1.3 KB"));
        assert!(text.contains("sha256:4d7a214"));
        assert!(super::describe(&None, &Some(new)).starts_with("LFS object added: 1.3 KB"));
    }

    fn with_object(dir: &Path, contents: &[u8]) -> LfsPointer {
        let pointer = LfsPointer {
            oid: format!("sha256:{OID}"),
            size: contents.len() as u64,
        };
        let path = object_path(dir, &pointer).expect("object path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("object dir");
        std::fs::write(path, contents).expect("object");
        pointer
    }

    #[test]
    fn worktree_diff_with_a_real_file_uses_the_real_content() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let stored = with_object(temp.path(), b"real file\nsecond line\n");
        let pointer_bytes = pointer(stored.hex(), stored.size);
        let worktree = b"real file\nchanged line\n";

        let resolved = resolve_diff(
            Some(pointer_bytes.as_bytes()),
            Some(worktree),
            temp.path(),
            MAX_POINTER_BYTES,
        );
        match resolved {
            LfsDiff::Real { old, new, summary } => {
                assert_eq!(old, b"real file\nsecond line\n");
                assert_eq!(new, worktree);
                assert!(summary.text.starts_with("LFS object changed"));
            }
            other => panic!("expected real content, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_the_summary_when_the_object_is_missing() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let text = format!("{POINTER_HEADER}\noid sha256:{OID}\nsize 2048\n");
        let resolved = resolve_diff(
            Some(text.as_bytes()),
            Some(b"new content\n"),
            temp.path(),
            MAX_POINTER_BYTES,
        );
        match resolved {
            LfsDiff::Summary(summary) => {
                assert!(!summary.old.as_ref().expect("old side").present);
                assert!(summary.text.contains("2.0 KB"));
            }
            other => panic!("expected a summary, got {other:?}"),
        }
    }

    #[test]
    fn an_ordinary_file_is_not_an_lfs_diff() {
        assert_eq!(
            resolve_diff(
                Some(b"one\n"),
                Some(b"two\n"),
                Path::new("/nonexistent"),
                1024
            ),
            LfsDiff::NotLfs
        );
    }

    #[test]
    fn locks_json_parses_owner_and_missing_fields() {
        let locks = parse_locks_json(
            r#"{"locks":[{"id":"1","path":"a.bin","locked_at":"2024-01-01T00:00:00Z","owner":{"name":"alice"}},{"path":"b.bin"}]}"#,
        )
        .expect("valid json");
        assert_eq!(locks.len(), 2);
        assert_eq!(locks[0].owner.as_deref(), Some("alice"));
        assert_eq!(locks[0].locked_at.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(locks[1].id, "");
        assert_eq!(locks[1].owner, None);
        assert!(parse_locks_json("not json").is_err());
    }
}
