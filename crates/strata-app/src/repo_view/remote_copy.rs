//! A read-only local copy of a remote file, so the editor can open it.
//!
//! The configured editor is a program on this machine. It cannot open a path on
//! the host, and handing it a remote path would silently open whatever the same
//! path means here. The file is therefore copied out of the session into a
//! per-tab temporary directory and the editor is given that local path.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes directories that would otherwise share a name, such as two
/// tabs opened in the same second.
static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

/// One tab's directory of copies. Dropping it removes the copies with the tab.
pub(crate) struct RemoteCopyDir {
    dir: PathBuf,
}

impl RemoteCopyDir {
    /// Creates the directory for `tab`. The name says read-only copy, so the
    /// editor's window title and recent-files list show what the user has.
    pub(crate) fn create(tab: u64) -> Result<Self, String> {
        let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let name = format!("strata-readonly-copy-{tab}-{serial}");
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).map_err(|error| write_failed(&dir, error))?;
        Ok(Self { dir })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the copy of `repo_path` belongs inside this directory.
    #[cfg(test)]
    pub(crate) fn path_for(&self, repo_path: &str) -> Option<PathBuf> {
        copy_path(&self.dir, repo_path)
    }

    /// Writes `bytes` as the copy of `repo_path` and returns the local path.
    #[cfg(test)]
    pub(crate) fn write(&self, repo_path: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        write_copy(&self.dir, repo_path, bytes)
    }
}

impl Drop for RemoteCopyDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Where the copy of `repo_path` belongs under `dir`.
///
/// Only ordinary components are joined, so a path that came from the host
/// cannot name anything outside that directory.
pub(crate) fn copy_path(dir: &Path, repo_path: &str) -> Option<PathBuf> {
    let mut path = dir.to_path_buf();
    let mut named = false;
    for component in Path::new(repo_path).components() {
        match component {
            Component::Normal(part) => {
                path.push(part);
                named = true;
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    named.then_some(path)
}

/// Writes `bytes` as the copy of `repo_path` under `dir`, creating the folders
/// the path names.
pub(crate) fn write_copy(dir: &Path, repo_path: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = copy_path(dir, repo_path)
        .ok_or_else(|| t!("editor.remote_copy_unsafe_path", path = repo_path).to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| write_failed(parent, error))?;
    }
    std::fs::write(&path, bytes).map_err(|error| write_failed(&path, error))?;
    Ok(path)
}

fn write_failed(path: &Path, error: std::io::Error) -> String {
    t!(
        "editor.remote_copy_failed",
        path = path.display(),
        error = error
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_directory_is_local_and_unique_per_tab() {
        let one = RemoteCopyDir::create(7).expect("first directory");
        let two = RemoteCopyDir::create(7).expect("second directory");
        let other = RemoteCopyDir::create(8).expect("another tab");
        assert!(one.dir().starts_with(std::env::temp_dir()));
        assert_ne!(one.dir(), two.dir(), "a reopened tab starts empty");
        assert_ne!(one.dir(), other.dir());
        assert!(
            one.dir()
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("readonly-copy"))
        );
    }

    #[test]
    fn a_copy_directory_goes_away_with_the_tab() {
        let dir = {
            let copy = RemoteCopyDir::create(1).expect("directory");
            copy.write("README.md", b"alpha\n").expect("write");
            copy.dir().to_path_buf()
        };
        assert!(!dir.exists(), "the copies must not outlive the tab");
    }

    #[test]
    fn a_path_from_the_host_cannot_leave_the_copy_directory() {
        let copy = RemoteCopyDir::create(2).expect("directory");
        let inside = copy.path_for("src/main.rs").expect("a relative path");
        assert!(inside.starts_with(copy.dir()));
        assert!(copy.path_for("./src/main.rs").is_some());
        assert!(copy.path_for("../secrets").is_none());
        assert!(copy.path_for("src/../../secrets").is_none());
        assert!(copy.path_for("/etc/passwd").is_none());
        assert!(copy.path_for("").is_none());
    }

    #[test]
    fn the_copy_holds_the_blob_the_session_read() {
        let repo = strata_core::test_support::TestRepo::new();
        let content = "alpha\nbeta\n";
        repo.write("docs/with space/file.txt", content);
        repo.commit("alpha");
        let reader = strata_core::repo::Repo::open(repo.path()).expect("open the repository");
        let head = match reader.head().expect("head") {
            strata_core::HeadInfo::Branch {
                target: Some(oid), ..
            } => oid,
            strata_core::HeadInfo::Detached { target } => target,
            other => panic!("unexpected head {other:?}"),
        };
        let blob = reader
            .blob_at(head, "docs/with space/file.txt")
            .expect("read the blob");

        let copy = RemoteCopyDir::create(3).expect("directory");
        let path = copy
            .write("docs/with space/file.txt", &blob)
            .expect("write the copy");
        assert!(path.starts_with(copy.dir()), "the editor gets a local path");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read the copy"),
            content
        );
    }
}
