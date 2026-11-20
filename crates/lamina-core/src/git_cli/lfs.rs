//! Git LFS commands: locking, tracking and object download.
//!
//! Everything here runs through the `git` executable so the repository's LFS
//! configuration, endpoint and credential helper apply exactly as in a shell.
//! `git-lfs` may not be installed; [`Git::lfs_available`] lets the UI warn
//! instead of showing a failed command for every LFS file.

use crate::Result;
use crate::git_cli::Git;
use crate::lfs::{LfsLock, parse_locks_json};

impl Git {
    /// Whether `git lfs` is installed and runnable.
    pub fn lfs_available(&self) -> bool {
        self.run_internal(&["lfs", "version"], None).is_ok()
    }

    /// The `git lfs version` line, for the notice that says what to install.
    pub fn lfs_version(&self) -> Option<String> {
        self.run_internal(&["lfs", "version"], None)
            .ok()
            .and_then(|output| {
                let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                (!text.is_empty()).then_some(text)
            })
    }

    /// Locks the remote reports, from `git lfs locks --json`.
    pub fn lfs_locks(&self) -> Result<Vec<LfsLock>> {
        let output = self.run(&["lfs", "locks", "--json"], None)?;
        parse_locks_json(&String::from_utf8_lossy(&output.stdout))
    }

    /// `git lfs lock <path>`.
    pub fn lfs_lock(&self, path: &str) -> Result<()> {
        self.run(&["lfs", "lock", "--", path], None).map(|_| ())
    }

    /// `git lfs unlock <path>`.
    pub fn lfs_unlock(&self, path: &str) -> Result<()> {
        self.run(&["lfs", "unlock", "--", path], None).map(|_| ())
    }

    /// `git lfs track <pattern>`: adds `filter=lfs` to `.gitattributes`.
    pub fn lfs_track(&self, pattern: &str) -> Result<()> {
        self.run(&["lfs", "track", "--", pattern], None).map(|_| ())
    }

    /// `git lfs pull --include=<path>`, which needs the remote.
    pub fn lfs_pull(&self, path: &str) -> Result<()> {
        let include = format!("--include={path}");
        self.run(&["lfs", "pull", &include], None).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use crate::lfs::POINTER_HEADER;
    use crate::test_support::TestRepo;

    fn git_lfs_available(test: &TestRepo) -> bool {
        test.git_output(&["lfs", "version"]).status.success()
    }

    /// Skips instead of failing when `git-lfs` is not installed, so a CI image
    /// without it still passes; the reason is printed for anyone debugging a
    /// skipped run.
    #[test]
    fn track_commit_and_read_the_pointer_from_history() {
        let test = TestRepo::new();
        if !git_lfs_available(&test) {
            eprintln!("skipping LFS end-to-end test: git-lfs is not installed");
            return;
        }
        // Never install globally: this repository's config and hooks only.
        test.git(&["lfs", "install", "--local"]);
        test.git(&["lfs", "track", "*.bin"]);
        test.write("data.bin", "hello lfs\n");
        let commit = test.commit("add an lfs file");

        let repo = test.open();
        let pointer = repo
            .lfs_info_at(commit, "data.bin")
            .expect("read the blob")
            .expect("the committed blob is a pointer");
        assert_eq!(pointer.size, 10);
        assert!(
            pointer.present,
            "git-lfs stores the object locally before the commit"
        );

        // History keeps the pointer, not the content.
        let blob = repo
            .file_at_commit(commit, "data.bin")
            .expect("blob read")
            .expect("blob");
        assert!(
            String::from_utf8_lossy(&blob).starts_with(POINTER_HEADER),
            "the commit stores the LFS pointer"
        );
        let object = crate::lfs::read_object(
            repo.common_dir(),
            &crate::lfs::LfsPointer {
                oid: pointer.oid.clone(),
                size: pointer.size,
            },
            1024,
        )
        .expect("object read")
        .expect("object present");
        assert_eq!(object, b"hello lfs\n");
    }
}
