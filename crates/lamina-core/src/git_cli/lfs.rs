//! Git LFS commands: locking, tracking and object download.
//!
//! Everything here runs through the `git` executable so the repository's LFS
//! configuration, endpoint and credential helper apply exactly as in a shell.
//! `git-lfs` may not be installed; [`Git::lfs_available`] lets the UI warn
//! instead of showing a failed command for every LFS file.

use crate::Result;
use crate::error::Error;
use crate::git_cli::Git;
use crate::lfs::{LfsLock, parse_locks_json};

/// What `git lfs install` writes for the filter. Kept here so a git-lfs too
/// old for `--skip-repo` still gets the filter that makes a tracked file
/// store a pointer instead of its bytes.
const LFS_FILTER_CONFIG: [(&str, &str); 4] = [
    ("filter.lfs.clean", "git-lfs clean -- %f"),
    ("filter.lfs.smudge", "git-lfs smudge -- %f"),
    ("filter.lfs.process", "git-lfs filter-process"),
    ("filter.lfs.required", "true"),
];

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
    ///
    /// Returns true when the filter was missing and this call configured it.
    /// Without that filter, tracking only edits `.gitattributes` and the next
    /// commit stores the raw bytes, because `filter.lfs.clean` never converted
    /// them.
    pub fn lfs_track(&self, pattern: &str) -> Result<bool> {
        let installed = !self.lfs_filter_configured()?;
        if installed {
            self.configure_lfs_filter()?;
        }
        self.run(&["lfs", "track", "--", pattern], None)
            .map(|_| ())?;
        Ok(installed)
    }

    /// Writes the filter through `git lfs install`, but never its hooks.
    ///
    /// A plain `install` also adds a pre-push hook nobody asked for, and exits
    /// non-zero when one already exists — after it wrote the filter — which
    /// used to abandon the track and leave `.gitattributes` untouched.
    /// `--skip-repo` configures only the filter; the direct config writes cover
    /// a git-lfs too old for that flag.
    fn configure_lfs_filter(&self) -> Result<()> {
        match self.run(&["lfs", "install", "--local", "--skip-repo"], None) {
            Ok(_) if self.lfs_filter_configured()? => return Ok(()),
            // Without git-lfs the fallback would leave a required filter whose
            // command does not exist, and every later checkout or status of a
            // tracked path would fail.
            Err(error) if !self.lfs_available() => return Err(error),
            _ => {}
        }
        for (key, value) in LFS_FILTER_CONFIG {
            self.run(&["config", key, value], None)?;
        }
        if self.lfs_filter_configured()? {
            Ok(())
        } else {
            Err(Error::other("git did not configure the LFS filter"))
        }
    }

    /// Whether both sides of the LFS clean filter are configured.
    ///
    /// `git lfs install --local` writes these; a repository that only added
    /// `filter=lfs` to `.gitattributes` has neither.
    fn lfs_filter_configured(&self) -> Result<bool> {
        let process = self
            .run_internal(&["config", "--get", "filter.lfs.process"], None)
            .is_ok();
        let clean = self
            .run_internal(&["config", "--get", "filter.lfs.clean"], None)
            .is_ok();
        Ok(process && clean)
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

    /// Skips instead of failing when `git-lfs` is not installed, so a CI image
    /// without it still passes; the reason is printed for anyone debugging a
    /// skipped run.
    fn git_lfs_available(test: &TestRepo) -> bool {
        test.git_output(&["lfs", "version"]).status.success()
    }

    #[test]
    fn tracking_installs_the_filter_when_it_is_not_configured() {
        let test = TestRepo::new();
        if !git_lfs_available(&test) {
            eprintln!("skipping LFS filter test: git-lfs is not installed");
            return;
        }
        // A fresh repository has `filter=lfs` nowhere. Install runs before
        // tracking so the next commit stores a pointer instead of raw bytes.
        assert!(
            !test
                .git_output(&["config", "--get", "filter.lfs.process"])
                .status
                .success(),
            "the fixture must start without the filter"
        );

        let git = test.git_handle();
        assert!(
            git.lfs_track("*.bin").expect("track with a missing filter"),
            "tracking had to run `git lfs install --local`"
        );
        for key in ["filter.lfs.process", "filter.lfs.clean"] {
            let output = test.git_output(&["config", "--get", key]);
            assert!(
                output.status.success(),
                "{key} was not configured: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            !git.lfs_track("*.dat").expect("track again"),
            "a configured filter must not be installed twice"
        );
    }

    #[test]
    fn track_leaves_an_existing_pre_push_hook_alone() {
        let test = TestRepo::new();
        if !git_lfs_available(&test) {
            eprintln!("skipping LFS hook test: git-lfs is not installed");
            return;
        }
        // `git lfs install` exits 2 when this hook is already there, after it
        // wrote the filter, and the track used to be dropped with it.
        let hooks = test.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks).expect("create hooks directory");
        let hook = hooks.join("pre-push");
        let script = "#!/bin/sh\nexit 0\n";
        std::fs::write(&hook, script).expect("write pre-push hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
                .expect("chmod the hook");
        }

        let git = test.git_handle();
        assert!(
            git.lfs_track("*.bin").expect("track with an existing hook"),
            "tracking had to configure the missing filter"
        );
        assert_eq!(
            std::fs::read_to_string(&hook).expect("read the hook"),
            script,
            "tracking must not replace the pre-push hook"
        );
        let attributes =
            std::fs::read_to_string(test.path().join(".gitattributes")).expect("read attributes");
        assert!(attributes.contains("*.bin filter=lfs"), "{attributes}");
    }

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
