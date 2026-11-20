//! LFS pointer reads and `.gitattributes` `filter=lfs` detection.

use std::path::Path;

use gix::attrs::StateRef;
use gix::worktree::stack::state::attributes::Source;

use super::Repo;
use crate::lfs::{LfsInfo, LfsPointer};
use crate::{Error, Oid, Result};

impl Repo {
    /// The directory holding `lfs/objects`, which is the common git directory
    /// for a linked worktree and the ordinary git directory otherwise.
    pub fn common_dir(&self) -> &Path {
        self.inner
            .common_dir
            .as_deref()
            .unwrap_or_else(|| self.inner.git_dir())
    }

    /// LFS metadata for `path` at `commit`, when the blob there is a pointer.
    pub fn lfs_info_at(&self, commit: Oid, path: &str) -> Result<Option<LfsInfo>> {
        let Some(bytes) = self.file_at_commit(commit, path)? else {
            return Ok(None);
        };
        Ok(LfsPointer::parse(&bytes)
            .map(|pointer| LfsInfo::from_pointer(&pointer, self.common_dir())))
    }

    /// LFS metadata for the index blob at `path`, when it is a pointer.
    pub fn lfs_info_in_index(&self, path: &str) -> Result<Option<LfsInfo>> {
        let Some(bytes) = self.file_in_index(path)? else {
            return Ok(None);
        };
        Ok(LfsPointer::parse(&bytes)
            .map(|pointer| LfsInfo::from_pointer(&pointer, self.common_dir())))
    }

    /// LFS metadata for the worktree file at `path`.
    ///
    /// A checked-out LFS file holds the real content, so there is no pointer to
    /// read. The attribute decides whether it belongs to LFS; the size is the
    /// file's own size and the object counts as present because it is on disk.
    pub fn lfs_info_in_worktree(&self, path: &str) -> Result<Option<LfsInfo>> {
        let Some(bytes) = self.file_in_worktree(path)? else {
            return Ok(None);
        };
        if let Some(pointer) = LfsPointer::parse(&bytes) {
            return Ok(Some(LfsInfo::from_pointer(&pointer, self.common_dir())));
        }
        if self.is_lfs_tracked(path)? {
            return Ok(Some(LfsInfo {
                oid: String::new(),
                size: bytes.len() as u64,
                present: true,
            }));
        }
        Ok(None)
    }

    /// Whether `.gitattributes` gives `path` `filter=lfs`.
    ///
    /// gix resolves the full stack (worktree `.gitattributes`, the index copy
    /// and `info/attributes`), which is what `git check-attr` would report.
    pub fn is_lfs_tracked(&self, path: &str) -> Result<bool> {
        let repo = self.local();
        let index = repo.index_or_empty().map_err(Error::gix)?;
        let mut attributes = repo
            .attributes_only(&index, Source::WorktreeThenIdMapping)
            .map_err(Error::gix)?
            .detach();
        let mut outcome = gix::attrs::search::Outcome::default();
        outcome.initialize_with_selection(&Default::default(), ["filter"]);
        let platform = attributes
            .at_path(Path::new(path), None, &repo.objects)
            .map_err(Error::gix)?;
        platform.matching_attributes(&mut outcome);
        Ok(outcome.iter_selected().any(|matched| {
            matched.assignment.name.as_str() == "filter"
                && matches!(matched.assignment.state, StateRef::Value(value) if value.as_bstr() == b"lfs")
        }))
    }

    /// Every `filter=lfs` pattern in the worktree and common
    /// `info/attributes`, for marking tracked patterns in the UI.
    ///
    /// The stack is read directly rather than through the attribute matching
    /// API because this reports the patterns themselves, not the result for one
    /// path.
    pub fn lfs_tracked_patterns(&self) -> Result<Vec<String>> {
        let mut patterns = Vec::new();
        if let Some(workdir) = self.workdir() {
            collect_patterns(workdir, 0, &mut patterns);
        }
        let info = self.common_dir().join("info").join("attributes");
        if let Ok(text) = std::fs::read_to_string(info) {
            collect_text_patterns(&text, &mut patterns);
        }
        patterns.sort();
        patterns.dedup();
        Ok(patterns)
    }

    /// Whether the repository configures any LFS filter, which is what decides
    /// whether a missing `git-lfs` is worth telling the user about.
    pub fn uses_lfs(&self) -> bool {
        self.lfs_tracked_patterns()
            .is_ok_and(|patterns| !patterns.is_empty())
    }
}

fn collect_patterns(dir: &Path, depth: usize, out: &mut Vec<String>) {
    // `.gitattributes` files are normally shallow; the cap only stops a
    // pathological tree from turning a notice check into a full walk.
    if depth > 16 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if entry.file_name() != ".git" {
                collect_patterns(&path, depth + 1, out);
            }
        } else if entry.file_name() == ".gitattributes"
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            collect_text_patterns(&text, out);
        }
    }
}

fn collect_text_patterns(text: &str, out: &mut Vec<String>) {
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(pattern) = tokens.next() else {
            continue;
        };
        if tokens.any(|token| token == "filter=lfs") {
            out.push(pattern.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::test_support::TestRepo;

    #[test]
    fn reads_filter_lfs_from_attributes() {
        let test = TestRepo::new();
        test.write(
            ".gitattributes",
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        );
        let repo = test.open();
        assert!(
            repo.is_lfs_tracked("assets/model.bin")
                .expect("attr lookup")
        );
        assert!(!repo.is_lfs_tracked("src/main.rs").expect("attr lookup"));
        assert_eq!(
            repo.lfs_tracked_patterns().expect("patterns"),
            vec!["*.bin".to_owned()]
        );
        assert!(repo.uses_lfs());
    }

    #[test]
    fn detects_a_pointer_at_a_commit_and_its_object() {
        let test = TestRepo::new();
        let common = test.path().join(".git");
        let oid = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 5\n");
        test.write("asset.bin", &pointer);
        let head = test.commit("add lfs pointer");
        let repo = test.open();

        let info = repo
            .lfs_info_at(head, "asset.bin")
            .expect("pointer read")
            .expect("pointer");
        assert_eq!(info.size, 5);
        assert!(!info.present);

        let object: PathBuf = common
            .join("lfs")
            .join("objects")
            .join(&oid[0..2])
            .join(&oid[2..4])
            .join(oid);
        std::fs::create_dir_all(object.parent().expect("parent")).expect("object dir");
        std::fs::write(&object, b"hello").expect("object");
        let info = repo
            .lfs_info_at(head, "asset.bin")
            .expect("pointer read")
            .expect("pointer");
        assert!(info.present);
    }

    #[test]
    fn a_real_worktree_file_is_lfs_when_its_pattern_matches() {
        let test = TestRepo::new();
        test.write(".gitattributes", "*.bin filter=lfs\n");
        test.write("data.bin", "real content");
        let repo = test.open();
        let info = repo
            .lfs_info_in_worktree("data.bin")
            .expect("worktree read")
            .expect("lfs");
        assert_eq!(info.size, 12);
        assert!(info.present);
        assert!(info.oid.is_empty());
    }
}
