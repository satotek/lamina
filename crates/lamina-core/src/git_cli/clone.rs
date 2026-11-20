//! The two commands that create a repository: clone and init.
//!
//! Neither goes through a [`Session`](crate::session::Session): there is no
//! repository to open yet, so the caller has nothing but a destination folder.
//! Both take a [`Git`] handle whose working directory is the folder the new
//! repository is created in, which keeps the same environment (and test
//! overrides) as every other CLI call.
//!
//! Cloning streams git's `--progress` stderr through the same parser fetch
//! uses and stops on the caller's cancel flag. Cancelling kills the child and
//! removes the directory, but only when this call created it: an existing
//! empty folder belongs to the user.

use std::path::{Path, PathBuf};

use super::{Git, checked_ref, progress::progress_percent};
use crate::{Error, Result};

/// What a clone should take from the remote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloneOptions {
    /// Clone this branch instead of the remote's default.
    pub branch: Option<String>,
    /// `--depth`: only the newest N commits. `None` is a full clone.
    pub depth: Option<u32>,
    /// `--recurse-submodules`: check out submodules as well.
    pub recurse_submodules: bool,
}

/// How a clone ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneOutcome {
    /// The repository is complete at the requested path.
    Cloned,
    /// The caller cancelled; a directory this call created has been removed.
    Cancelled,
}

/// Clone `url` into `target`, reporting git's progress lines as they arrive.
///
/// `runner` must have been created for the directory `target` is created in,
/// so a relative environment and the command log line up with the clone.
/// Progress is `(line, percentage)`; lines without a counter (such as
/// `Cloning into 'repo'...`) carry `None`.
pub fn clone_repository(
    runner: &Git,
    url: &str,
    target: &Path,
    options: &CloneOptions,
    cancelled: &(impl Fn() -> bool + Send + Sync),
    mut on_progress: impl FnMut(&str, Option<u8>),
) -> Result<CloneOutcome> {
    let url = url.trim();
    let target = absolute(target);
    validate_clone(url, &target, options)?;
    if cancelled() {
        return Ok(CloneOutcome::Cancelled);
    }

    let existed = target.exists();
    let mut args = vec!["clone".to_owned(), "--progress".to_owned()];
    if let Some(branch) = &options.branch {
        args.push("--branch".to_owned());
        args.push(branch.trim().to_owned());
    }
    if let Some(depth) = options.depth {
        args.push("--depth".to_owned());
        args.push(depth.to_string());
    }
    if options.recurse_submodules {
        args.push("--recurse-submodules".to_owned());
    }
    // A URL can contain almost anything, including a leading `-`; `--` makes
    // sure git never reads it as an option.
    args.push("--".to_owned());
    args.push(url.to_owned());
    args.push(target.to_string_lossy().into_owned());

    let finished = runner.run_stderr_cancellable(&args, cancelled, |text| {
        let text = text.trim();
        if !text.is_empty() {
            on_progress(text, progress_percent(text));
        }
    })?;
    if finished {
        return Ok(CloneOutcome::Cloned);
    }

    if !existed {
        // Only the directory this call created: `git clone` into an existing
        // empty folder leaves that folder behind when it is interrupted. A
        // cancel before git created it has nothing to remove.
        match std::fs::remove_dir_all(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::other(format!(
                    "the clone was cancelled, but {} could not be removed: {error}",
                    target.display()
                )));
            }
        }
    }
    Ok(CloneOutcome::Cancelled)
}

/// Create a repository at `target`, optionally on an initial branch.
///
/// An existing folder is valid (`git init` is also how a folder becomes a
/// repository), so only the parent of a missing folder is checked.
pub fn init_repository(runner: &Git, target: &Path, branch: Option<&str>) -> Result<()> {
    let branch = branch.map(str::trim).filter(|branch| !branch.is_empty());
    if let Some(branch) = branch {
        checked_ref(branch, "branch")?;
    }
    let target = absolute(target);
    if target.file_name().is_none() {
        return Err(Error::other("the repository needs a folder name"));
    }
    if !target.exists()
        && let Some(parent) = target.parent()
    {
        check_parent(parent)?;
    }

    let mut args = vec!["init".to_owned()];
    if let Some(branch) = branch {
        args.push("-b".to_owned());
        args.push(branch.to_owned());
    }
    args.push("--".to_owned());
    args.push(target.to_string_lossy().into_owned());
    runner.run(&args, None).map(|_| ())
}

/// Reject what would make git fail with a message about its own arguments, so
/// the dialog can say what the user has to change instead.
fn validate_clone(url: &str, target: &Path, options: &CloneOptions) -> Result<()> {
    if url.is_empty() {
        return Err(Error::other("the repository URL is empty"));
    }
    if let Some(branch) = &options.branch {
        checked_ref(branch.trim(), "branch")?;
    }
    if options.depth == Some(0) {
        return Err(Error::other("clone depth must be at least 1"));
    }
    if target.file_name().is_none() {
        return Err(Error::other("the clone needs a folder name"));
    }
    if target.exists() {
        if !target.is_dir() {
            return Err(Error::other(format!(
                "cannot clone into {}: it is a file",
                target.display()
            )));
        }
        let mut entries = std::fs::read_dir(target)?;
        if entries.next().is_some() {
            return Err(Error::other(format!(
                "cannot clone into {}: the folder is not empty",
                target.display()
            )));
        }
    }
    match target.parent() {
        Some(parent) => check_parent(parent),
        None => Err(Error::other("the clone needs a destination folder")),
    }
}

/// A parent that does not exist or has no write bit would make git fail after
/// the network round trip; refuse before touching the remote.
fn check_parent(parent: &Path) -> Result<()> {
    let metadata = match std::fs::metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::other(format!(
                "cannot create a repository in {}: the folder does not exist",
                parent.display()
            )));
        }
        Err(error) => {
            return Err(Error::other(format!(
                "cannot use {} as a destination: {error}",
                parent.display()
            )));
        }
    };
    if !metadata.is_dir() {
        return Err(Error::other(format!(
            "cannot use {} as a destination: it is not a folder",
            parent.display()
        )));
    }
    if !writable(&metadata) {
        return Err(Error::other(format!(
            "cannot write to {}: the folder is read-only",
            parent.display()
        )));
    }
    Ok(())
}

/// The write bits are the cross-platform answer; ACLs are left to git, whose
/// failure is then reported verbatim.
fn writable(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o222 != 0
    }
    #[cfg(not(unix))]
    {
        !metadata.permissions().readonly()
    }
}

/// Resolve against the process working directory now, so the command's own
/// working directory cannot change what a relative target means.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::{CloneOptions, CloneOutcome, clone_repository, init_repository};
    use crate::git_cli::Git;

    /// An empty config file, so neither the user's settings nor a repository's
    /// local config can change what the test runs. A real file rather than
    /// `/dev/null`, which does not exist on Windows.
    fn empty_config() -> &'static Path {
        static CONFIG: OnceLock<PathBuf> = OnceLock::new();
        CONFIG.get_or_init(|| {
            let path =
                std::env::temp_dir().join(format!("lamina-test-gitconfig-{}", std::process::id()));
            let _ = std::fs::write(&path, "");
            path
        })
    }

    /// Runs git with this machine's config and identity kept out of the test.
    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", empty_config())
            .env("GIT_CONFIG_SYSTEM", empty_config())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Lamina Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Lamina Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// A handle with the same isolation as [`git`], so clone/init children do
    /// not read the developer's config either.
    fn isolated(dir: &Path) -> Git {
        Git::new(dir.to_path_buf())
            .with_env("GIT_CONFIG_GLOBAL", empty_config())
            .with_env("GIT_CONFIG_SYSTEM", empty_config())
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_AUTHOR_NAME", "Lamina Test")
            .with_env("GIT_AUTHOR_EMAIL", "test@example.com")
            .with_env("GIT_COMMITTER_NAME", "Lamina Test")
            .with_env("GIT_COMMITTER_EMAIL", "test@example.com")
    }

    struct Fixture {
        _temp: TempDir,
        work: PathBuf,
        bare: PathBuf,
    }

    /// A repository with two commits on `main` and `feature` at the first.
    fn fixture() -> Fixture {
        let temp = TempDir::new().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(work.join("first.txt"), "one").unwrap();
        git(&work, &["add", "first.txt"]);
        git(&work, &["commit", "-q", "-m", "first"]);
        std::fs::write(work.join("second.txt"), "two").unwrap();
        git(&work, &["add", "second.txt"]);
        git(&work, &["commit", "-q", "-m", "second"]);
        git(&work, &["branch", "feature", "HEAD~1"]);

        let bare = temp.path().join("origin.git");
        let bare_arg = bare.to_string_lossy().into_owned();
        let work_arg = work.to_string_lossy().into_owned();
        git(
            temp.path(),
            &["clone", "-q", "--bare", &work_arg, &bare_arg],
        );
        Fixture {
            _temp: temp,
            work,
            bare,
        }
    }

    fn file_url(path: &Path) -> String {
        let path = path.canonicalize().unwrap();
        let text = path.to_string_lossy();
        // Windows canonical paths start with `\\?\`; a URL does not take it.
        let text = text.strip_prefix(r"\\?\").unwrap_or(text.as_ref());
        let text = text.replace('\\', "/");
        if text.starts_with('/') {
            format!("file://{text}")
        } else {
            format!("file:///{text}")
        }
    }

    fn output_text(output: std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn clones_a_branch_and_only_the_requested_depth() {
        let fixture = fixture();
        let temp = TempDir::new().unwrap();
        let url = file_url(&fixture.bare);

        let shallow = temp.path().join("shallow");
        let runner = isolated(temp.path());
        let mut lines = Vec::new();
        let outcome = clone_repository(
            &runner,
            &url,
            &shallow,
            &CloneOptions {
                depth: Some(1),
                ..CloneOptions::default()
            },
            &|| false,
            |line, percent| lines.push((line.to_owned(), percent)),
        )
        .unwrap();
        assert_eq!(outcome, CloneOutcome::Cloned);
        assert_eq!(
            output_text(git(&shallow, &["rev-list", "--count", "HEAD"])),
            "1"
        );
        assert!(
            lines.iter().any(|(_, percent)| percent.is_some()),
            "progress lines with a counter must reach the callback: {lines:?}"
        );

        let branch = temp.path().join("branch");
        let outcome = clone_repository(
            &runner,
            &url,
            &branch,
            &CloneOptions {
                branch: Some("feature".to_owned()),
                ..CloneOptions::default()
            },
            &|| false,
            |_, _| {},
        )
        .unwrap();
        assert_eq!(outcome, CloneOutcome::Cloned);
        assert_eq!(
            output_text(git(&branch, &["rev-parse", "--abbrev-ref", "HEAD"])),
            "feature"
        );
        assert_eq!(
            output_text(git(&branch, &["rev-list", "--count", "HEAD"])),
            "1",
            "the feature branch points at the first commit"
        );
        // The source work tree was not changed by cloning.
        assert_eq!(
            output_text(git(&fixture.work, &["rev-parse", "--abbrev-ref", "HEAD"])),
            "main"
        );
    }

    #[test]
    fn cancelling_a_clone_removes_the_directory_it_created() {
        let fixture = fixture();
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("cancelled");
        let url = file_url(&fixture.bare);
        let runner = isolated(temp.path());

        let flag = Arc::new(AtomicBool::new(false));
        let cancel_flag = Arc::clone(&flag);
        let progress_flag = Arc::clone(&flag);
        let worker_target = target.clone();
        let probe = target.clone();
        let handle = thread::spawn(move || {
            clone_repository(
                &runner,
                &url,
                &worker_target,
                &CloneOptions::default(),
                &move || cancel_flag.load(Ordering::SeqCst),
                // Cancel only once git has created the directory, so the
                // cleanup is what the assertion observes.
                |_, _| {
                    if probe.exists() {
                        progress_flag.store(true, Ordering::SeqCst);
                    }
                },
            )
        });

        let deadline = Instant::now() + Duration::from_secs(20);
        while !target.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        flag.store(true, Ordering::SeqCst);
        let outcome = handle.join().unwrap().unwrap();
        assert_eq!(outcome, CloneOutcome::Cancelled);
        assert!(!target.exists(), "a cancelled clone leaves no directory");
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_a_clone_into_an_existing_folder_keeps_the_folder() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TempDir::new().unwrap();
        let target = temp.path().join("already-there");
        std::fs::create_dir(&target).unwrap();

        // A slow stand-in keeps the child alive until the flag flips, and
        // `exec` makes the sleep the process the watcher kills.
        let binary = temp.path().join("slow-git");
        std::fs::write(
            &binary,
            "#!/bin/sh\nprintf 'Cloning into...\\n' >&2\nexec sleep 10\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&binary, permissions).unwrap();

        let flag = Arc::new(AtomicBool::new(false));
        let worker_flag = Arc::clone(&flag);
        let worker_target = target.clone();
        let runner = Git::new(temp.path().to_path_buf()).with_git_binary(&binary);
        let handle = thread::spawn(move || {
            clone_repository(
                &runner,
                "https://example.com/repo.git",
                &worker_target,
                &CloneOptions::default(),
                &move || worker_flag.load(Ordering::SeqCst),
                |_, _| {},
            )
        });
        // The fake git does not create the folder; it already exists, and the
        // clone is cancelled while it runs.
        thread::sleep(Duration::from_millis(100));
        flag.store(true, Ordering::SeqCst);

        let outcome = handle.join().unwrap().unwrap();
        assert_eq!(outcome, CloneOutcome::Cancelled);
        assert!(
            target.is_dir(),
            "a folder that existed before the clone must survive a cancel"
        );
    }

    #[test]
    fn init_creates_a_repository_on_the_requested_branch() {
        let temp = TempDir::new().unwrap();
        let runner = isolated(temp.path());
        let target = temp.path().join("new-repo");

        init_repository(&runner, &target, Some("trunk")).unwrap();

        assert_eq!(
            output_text(git(&target, &["symbolic-ref", "--short", "HEAD"])),
            "trunk"
        );
        assert!(
            target.join(".git").is_dir(),
            "init must create the repository"
        );
    }

    #[test]
    fn init_accepts_an_existing_empty_folder_and_no_branch() {
        let temp = TempDir::new().unwrap();
        let runner = isolated(temp.path());
        let target = temp.path().join("existing");
        std::fs::create_dir(&target).unwrap();

        init_repository(&runner, &target, None).unwrap();
        assert!(target.join(".git").is_dir());
    }

    #[test]
    fn clone_inputs_are_validated_before_git_runs() {
        let fixture = fixture();
        let temp = TempDir::new().unwrap();
        let runner = isolated(temp.path());
        let url = file_url(&fixture.bare);

        let empty_url = clone_repository(
            &runner,
            "   ",
            &temp.path().join("target"),
            &CloneOptions::default(),
            &|| false,
            |_, _| {},
        )
        .unwrap_err();
        assert!(
            empty_url.to_string().contains("URL is empty"),
            "{empty_url}"
        );

        let busy = temp.path().join("busy");
        std::fs::create_dir(&busy).unwrap();
        std::fs::write(busy.join("keep.txt"), "x").unwrap();
        let not_empty = clone_repository(
            &runner,
            &url,
            &busy,
            &CloneOptions::default(),
            &|| false,
            |_, _| {},
        )
        .unwrap_err();
        assert!(not_empty.to_string().contains("not empty"), "{not_empty}");

        let zero_depth = clone_repository(
            &runner,
            &url,
            &temp.path().join("zero"),
            &CloneOptions {
                depth: Some(0),
                ..CloneOptions::default()
            },
            &|| false,
            |_, _| {},
        )
        .unwrap_err();
        assert!(
            zero_depth.to_string().contains("at least 1"),
            "{zero_depth}"
        );

        let option_branch = clone_repository(
            &runner,
            &url,
            &temp.path().join("option"),
            &CloneOptions {
                branch: Some("-main".to_owned()),
                ..CloneOptions::default()
            },
            &|| false,
            |_, _| {},
        )
        .unwrap_err();
        assert!(
            option_branch
                .to_string()
                .contains("must not start with `-`"),
            "{option_branch}"
        );

        let missing_parent = clone_repository(
            &runner,
            &url,
            &temp.path().join("gone").join("target"),
            &CloneOptions::default(),
            &|| false,
            |_, _| {},
        )
        .unwrap_err();
        assert!(
            missing_parent.to_string().contains("does not exist"),
            "{missing_parent}"
        );

        // Nothing was created by the rejected calls.
        assert!(!temp.path().join("target").exists());
        assert!(!temp.path().join("zero").exists());
        assert!(!temp.path().join("option").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_parent_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = fixture();
        let temp = TempDir::new().unwrap();
        let parent = temp.path().join("read-only");
        std::fs::create_dir(&parent).unwrap();
        let mut permissions = std::fs::metadata(&parent).unwrap().permissions();
        permissions.set_mode(0o555);
        std::fs::set_permissions(&parent, permissions).unwrap();

        let runner = isolated(&parent);
        let result = clone_repository(
            &runner,
            &file_url(&fixture.bare),
            &parent.join("target"),
            &CloneOptions::default(),
            &|| false,
            |_, _| {},
        );

        // Restore before the tempdir is dropped, so cleanup cannot fail.
        let mut permissions = std::fs::metadata(&parent).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&parent, permissions).unwrap();

        let error = result.unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error}");
    }
}
