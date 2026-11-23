//! A read-only local copy of a remote file, so the editor can open it.
//!
//! The configured editor is a program on this machine. It cannot open a path on
//! the host, and handing it a remote path would silently open whatever the same
//! path means here. The file is therefore copied out of the session into a
//! per-tab temporary directory and the editor is given that local path.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// One tab's directory of copies. Dropping it removes the copies with the tab.
pub(crate) struct RemoteCopyDir {
    dir: PathBuf,
    /// Owns the directory and deletes it on drop, including copies an editor
    /// still has open.
    _temp: tempfile::TempDir,
}

impl RemoteCopyDir {
    /// Creates the directory for `tab`.
    ///
    /// The name is random and the directory is private. A predictable name in
    /// the shared temporary directory could be pre-created as a symlink, and
    /// every copy would then be written wherever it points. The random suffix
    /// and the 0700 mode keep another local account from arranging that.
    ///
    /// The prefix says read-only copy, so the editor's window title and
    /// recent-files list show what the user has.
    pub(crate) fn create(tab: u64) -> Result<Self, String> {
        let parent = std::env::temp_dir();
        let temp = tempfile::Builder::new()
            .prefix(&format!("lamina-readonly-copy-{tab}-"))
            .tempdir_in(&parent)
            .map_err(|error| write_failed(&parent, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))
                .map_err(|error| write_failed(temp.path(), error))?;
        }
        Ok(Self {
            dir: temp.path().to_path_buf(),
            _temp: temp,
        })
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

/// Where the copy of `repo_path` belongs under `dir`.
///
/// Only ordinary components are joined, so a path that came from the host
/// cannot name anything outside that directory. Control characters are
/// refused too: they would reach the editor as part of a path it shows and
/// passes on to other programs.
pub(crate) fn copy_path(dir: &Path, repo_path: &str) -> Option<PathBuf> {
    let mut path = dir.to_path_buf();
    let mut named = false;
    for component in Path::new(repo_path).components() {
        match component {
            Component::Normal(part) => {
                if part
                    .to_str()
                    .is_some_and(|name| name.chars().any(char::is_control))
                {
                    return None;
                }
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
    // Reopening a file asks for the same copy again, and the private directory
    // has no other writer, so removing the previous copy is safe and keeps
    // `create_new` from turning a reopen into an error. Removing a planted
    // symlink deletes the link, not its target.
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(write_failed(&path, error)),
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| write_failed(&path, error))?;
    file.write_all(bytes)
        .map_err(|error| write_failed(&path, error))?;
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
    fn control_characters_cannot_reach_the_editor() {
        let copy = RemoteCopyDir::create(6).expect("directory");
        assert!(copy.path_for("src/\u{1}main.rs").is_none());
        assert!(copy.write("src/\u{7}", b"x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_copy_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| path.metadata().expect("metadata").permissions().mode() & 0o777;
        let copy = RemoteCopyDir::create(5).expect("directory");
        assert_eq!(mode(copy.dir()), 0o700, "the directory is owner-only");
        let path = copy.write("src/main.rs", b"fn main() {}\n").expect("write");
        assert_eq!(mode(&path), 0o600, "the copy is owner-only");
    }

    #[test]
    fn writing_the_same_copy_again_replaces_it() {
        let copy = RemoteCopyDir::create(4).expect("directory");
        let first = copy.write("README.md", b"alpha\n").expect("first write");
        copy.write("README.md", b"beta\n").expect("second write");
        assert_eq!(
            std::fs::read_to_string(first).expect("read the copy"),
            "beta\n"
        );
    }

    #[test]
    fn the_copy_holds_the_blob_the_session_read() {
        let repo = lamina_core::test_support::TestRepo::new();
        let content = "alpha\nbeta\n";
        repo.write("docs/with space/file.txt", content);
        repo.commit("alpha");
        let reader = lamina_core::repo::Repo::open(repo.path()).expect("open the repository");
        let head = match reader.head().expect("head") {
            lamina_core::HeadInfo::Branch {
                target: Some(oid), ..
            } => oid,
            lamina_core::HeadInfo::Detached { target } => target,
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
