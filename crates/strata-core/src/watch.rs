//! Debounced filesystem watching for one repository.
//!
//! [`RepoWatcher`] turns a noisy stream of native filesystem notifications into
//! [`Changes`], a coarse answer to "which views does the UI have to refresh?".
//!
//! Ignore handling: worktree paths are filtered through gix's excludes stack
//! ([`gix::Repository::excludes`]), which reads `.gitignore` files from the
//! worktree plus `info/exclude` and the global excludes file. `target/` or
//! `node_modules/` churn therefore never reaches the UI. The stack is built
//! once per debounce batch (it borrows a parsed index), which keeps a build's
//! worth of events cheap without holding a stale ignore cache between batches.
//!
//! Threading: notify's handler only forwards events into an unbounded channel.
//! A worker thread owns the debounce timer, the ignore filtering and the
//! callback, so the notify thread never waits on the UI.
//!
//! The callback receives at most one [`Changes`] per quiet `debounce` window,
//! and at least one every second while events keep streaming in.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::event::{CreateKind, RemoveKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{Error, Result};

/// How many distinct worktree paths a single [`Changes`] keeps.
const MAX_PATHS: usize = 1000;

/// Upper bound on how long an uninterrupted event stream can delay a callback.
const MAX_LATENCY: Duration = Duration::from_secs(1);

/// What changed since the last callback.
///
/// Everything is a conservative over-approximation: a flag means "refresh this
/// view", not "exactly this changed". Worktree paths are absolute and capped at
/// 1000 paths; git-dir changes only set flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    pub worktree: bool,
    pub index: bool,
    pub refs: bool,
    pub head: bool,
    /// `.git/config` changed, so remotes and upstreams may differ. History and
    /// status do not depend on it and are left alone.
    pub config: bool,
    /// Worktree paths that changed, deduplicated, in event order.
    pub paths: Vec<PathBuf>,
}

impl Changes {
    fn is_idle(&self) -> bool {
        !(self.worktree || self.index || self.refs || self.head || self.config)
    }
}

/// Watches a worktree and its git dir, reporting coalesced [`Changes`].
///
/// Dropping it stops watching and waits for the worker thread, unless the drop
/// happens from inside the callback itself.
pub struct RepoWatcher {
    watcher: Option<RecommendedWatcher>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RepoWatcher {
    /// Start watching `workdir` (recursively) and `git_dir`.
    ///
    /// `git_dir` must be the resolved git directory. For a linked worktree or a
    /// submodule that is the path the `.git` file points at, which may live
    /// outside `workdir`. A `None` workdir only watches the git dir (bare repo).
    ///
    /// Fails if the platform watcher cannot be created or a watch cannot be
    /// installed (for example when the inotify instance limit is exhausted);
    /// it never panics.
    pub fn new(
        workdir: Option<&Path>,
        git_dir: &Path,
        debounce: Duration,
        on_change: impl Fn(Changes) + Send + 'static,
    ) -> Result<Self> {
        let workdir = workdir.map(Root::new);
        let git_dir = Root::new(git_dir);
        let nested_git_dir = workdir
            .as_ref()
            .is_some_and(|workdir| workdir.contains(&git_dir));

        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            // Unbounded and non-blocking from notify's point of view; the worker
            // is the only place that may take its time.
            let _ = tx.send(event);
        })
        .map_err(|err| Error::other(format!("failed to create filesystem watcher: {err}")))?;

        // A recursive watch of the worktree already covers a git dir inside it.
        // Watching both would only duplicate every git-dir event.
        let mut roots = Vec::with_capacity(2);
        if let Some(workdir) = &workdir {
            roots.push(workdir.given.clone());
        }
        if !nested_git_dir {
            roots.push(git_dir.given.clone());
        }
        for root in roots {
            watcher
                .watch(&root, RecursiveMode::Recursive)
                .map_err(|err| {
                    Error::other(format!("failed to watch `{}`: {err}", root.display()))
                })?;
        }

        let filter = workdir.as_ref().and_then(WorktreeFilter::open);
        let stop = Arc::new(AtomicBool::new(false));
        let watch = Watch {
            workdir,
            git_dir,
            debounce,
            filter,
            stop: Arc::clone(&stop),
        };

        let worker = thread::Builder::new()
            .name("strata-repo-watcher".into())
            .spawn(move || run(rx, watch, on_change))
            .map_err(|err| Error::other(format!("failed to start watcher thread: {err}")))?;

        Ok(Self {
            watcher: Some(watcher),
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for RepoWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Dropping the watcher drops the event sender, which wakes the worker.
        drop(self.watcher.take());
        if let Some(worker) = self.worker.take() {
            // A callback that drops its own watcher must not join itself.
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}

/// Everything the worker thread needs; the repository handle stays here so the
/// notify callback never touches git.
struct Watch {
    workdir: Option<Root>,
    git_dir: Root,
    debounce: Duration,
    filter: Option<WorktreeFilter>,
    stop: Arc<AtomicBool>,
}

impl Watch {
    /// Fold a batch of raw events into one [`Changes`].
    fn changes(&self, events: &[notify::Result<Event>]) -> Changes {
        let mut changes = Changes::default();
        let mut candidates: Vec<(PathBuf, bool)> = Vec::new();
        let mut seen = HashSet::new();

        for event in events {
            // Runtime backend errors (access denied on a subtree, queue
            // overflow) are transient and repeating; surfacing them as
            // refreshes would cause storms, and the UI refreshes anyway when it
            // regains focus.
            let Ok(event) = event else { continue };
            // Access events fire for every open/read, including our own reads,
            // and never change content. `Other` is meta-information.
            if matches!(event.kind, EventKind::Access(_) | EventKind::Other) {
                continue;
            }
            let hint = directory_hint(&event.kind);

            for path in &event.paths {
                // Both roots are matched in the caller's form and in their
                // canonical form (macOS FSEvents). Only when neither matches do
                // we canonicalize the event path, which keeps the common case
                // free of extra syscalls and still works for deleted entries.
                let mut git_relative = self.git_dir.strip(path);
                let mut work_relative = self
                    .workdir
                    .as_ref()
                    .and_then(|workdir| workdir.strip(path));
                if git_relative.is_none()
                    && work_relative.is_none()
                    && let Ok(resolved) = path.canonicalize()
                {
                    git_relative = self.git_dir.strip(&resolved);
                    work_relative = self
                        .workdir
                        .as_ref()
                        .and_then(|workdir| workdir.strip(&resolved));
                }

                if let Some(relative) = git_relative {
                    match classify_git_dir(&relative) {
                        Scope::Index => changes.index = true,
                        Scope::Refs => changes.refs = true,
                        Scope::Head => changes.head = true,
                        Scope::Config => changes.config = true,
                        Scope::Ignored => {}
                    }
                    continue;
                }

                let (Some(workdir), Some(relative)) = (&self.workdir, work_relative) else {
                    continue;
                };
                if relative.as_os_str().is_empty() {
                    // The worktree root itself, e.g. an FSEvents "root changed".
                    changes.worktree = true;
                    continue;
                }

                let is_dir = hint.unwrap_or_else(|| workdir.given.join(&relative).is_dir());
                if seen.insert(relative.clone()) {
                    candidates.push((relative, is_dir));
                }
            }
        }

        if let (Some(workdir), false) = (&self.workdir, candidates.is_empty()) {
            self.record_worktree(candidates, workdir, &mut changes);
        }
        changes
    }

    fn record_worktree(
        &self,
        candidates: Vec<(PathBuf, bool)>,
        workdir: &Root,
        changes: &mut Changes,
    ) {
        let Some(filter) = &self.filter else {
            push_candidates(changes, workdir, candidates);
            return;
        };

        // The stack borrows the repository and index, so it lives only for this
        // batch. Failing to build it (missing index, unreadable config) must
        // not drop real changes on the floor, hence the fallbacks below.
        let repo = filter.repo.to_thread_local();
        let index = match repo.open_index() {
            Ok(index) => index,
            Err(_) => {
                push_candidates(changes, workdir, candidates);
                return;
            }
        };
        let mut stack = match repo.excludes(
            &index,
            None,
            gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
        ) {
            Ok(stack) => stack,
            Err(_) => {
                push_candidates(changes, workdir, candidates);
                return;
            }
        };

        for (relative, is_dir) in candidates {
            let mode = is_dir.then_some(gix::index::entry::Mode::DIR);
            let ignored = stack
                .at_path(&relative, mode)
                .is_ok_and(|platform| platform.is_excluded());
            if !ignored {
                push_path(changes, workdir.given.join(relative));
            }
        }
    }
}

fn push_candidates(
    changes: &mut Changes,
    workdir: &Root,
    candidates: impl IntoIterator<Item = (PathBuf, bool)>,
) {
    for (relative, _) in candidates {
        push_path(changes, workdir.given.join(relative));
    }
}

fn push_path(changes: &mut Changes, path: PathBuf) {
    changes.worktree = true;
    if changes.paths.len() < MAX_PATHS {
        changes.paths.push(path);
    }
}

/// The debounce loop.
fn run(rx: Receiver<notify::Result<Event>>, watch: Watch, on_change: impl Fn(Changes)) {
    while let Ok(first) = rx.recv() {
        if watch.stop.load(Ordering::SeqCst) {
            return;
        }

        let started = Instant::now();
        let mut last = started;
        let mut batch = vec![first];
        let mut disconnected = false;

        loop {
            // Quiet for `debounce`, but a never-ending stream still flushes
            // once a second so the UI cannot go stale.
            let deadline = (last + watch.debounce).min(started + MAX_LATENCY);
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match rx.recv_timeout(deadline - now) {
                Ok(event) => {
                    batch.push(event);
                    last = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if watch.stop.load(Ordering::SeqCst) {
            return;
        }
        let changes = watch.changes(&batch);
        if !changes.is_idle() {
            on_change(changes);
        }
        if disconnected {
            return;
        }
    }
}

/// A watched directory, kept in two forms.
///
/// Event paths use the form the OS and the watch call agreed on (the caller's
/// path on Windows and inotify), while macOS FSEvents reports symlink-resolved
/// paths such as `/private/var/...` for a `/var/...` watch. Keeping both lets a
/// missing path (a delete) still be classified without `canonicalize`, which
/// fails once the entry is gone.
struct Root {
    /// Absolute form of the path the caller gave us.
    given: PathBuf,
    /// Symlink-resolved form, when it differs from `given`.
    canonical: Option<PathBuf>,
}

impl Root {
    fn new(path: &Path) -> Self {
        Self {
            given: std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
            canonical: path.canonicalize().ok(),
        }
    }

    /// `path` relative to this root, matching either stored form.
    fn strip(&self, path: &Path) -> Option<PathBuf> {
        if let Ok(relative) = path.strip_prefix(&self.given) {
            return Some(relative.to_path_buf());
        }
        self.canonical
            .as_ref()
            .and_then(|canonical| path.strip_prefix(canonical).ok())
            .map(Path::to_path_buf)
    }

    /// Whether `other` is this directory or below it.
    fn contains(&self, other: &Self) -> bool {
        match (&self.canonical, &other.canonical) {
            (Some(parent), Some(child)) => child.starts_with(parent),
            _ => other.given.starts_with(&self.given),
        }
    }

    fn is_same_dir(&self, other: &Self) -> bool {
        match (&self.canonical, &other.canonical) {
            (Some(this), Some(that)) => this == that,
            _ => self.given == other.given,
        }
    }
}

/// Ignore filtering for one worktree.
struct WorktreeFilter {
    repo: gix::ThreadSafeRepository,
}

impl WorktreeFilter {
    fn open(workdir: &Root) -> Option<Self> {
        let repo = gix::ThreadSafeRepository::discover(&workdir.given).ok()?;
        // The excludes stack resolves `.gitignore` paths relative to the
        // repository's work dir, so only filter when it is the watched root.
        let root = Root::new(repo.work_dir()?);
        root.is_same_dir(workdir).then_some(Self { repo })
    }
}

/// Where a git-dir event happened, as far as [`Changes`] is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Index,
    Refs,
    Head,
    /// `.git/config`, which decides the configured remotes and upstreams.
    Config,
    /// Git bookkeeping that cannot change what the UI shows.
    Ignored,
}

/// Classify a path relative to the git dir.
///
/// `objects/` churn (fetches, gc), reflogs and lock files dominate event volume
/// and never affect the views, so they are dropped before they can wake the UI.
fn classify_git_dir(relative: &Path) -> Scope {
    let Some(Component::Normal(first)) = relative.components().next() else {
        // The git dir itself (metadata-only event).
        return Scope::Ignored;
    };

    // `index.lock`, `refs/heads/main.lock`, ...; the real update follows under
    // its final name.
    if relative
        .file_name()
        .is_some_and(|name| name.as_encoded_bytes().ends_with(b".lock"))
    {
        return Scope::Ignored;
    }

    if first == OsStr::new("refs") {
        return Scope::Refs;
    }
    if first == OsStr::new("index") {
        return Scope::Index;
    }
    if first == OsStr::new("HEAD") {
        return Scope::Head;
    }
    // `git remote add/remove/set-url` and `git branch --set-upstream-to` only
    // edit the configuration, so the file has to be visible to the session.
    if first == OsStr::new("config") {
        return Scope::Config;
    }
    // Pseudo-refs that move as a result of ref-changing operations; refreshing
    // refs is the closest match, and it is cheap.
    if [
        "packed-refs",
        "FETCH_HEAD",
        "ORIG_HEAD",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "REBASE_HEAD",
    ]
    .iter()
    .any(|name| first == OsStr::new(name))
    {
        return Scope::Refs;
    }

    // `objects/**`, `logs/**` (including `logs/HEAD`), `fsmonitor--daemon/**`,
    // `gc.pid`, `config`, hooks, `COMMIT_EDITMSG`, ...: nothing to refresh.
    Scope::Ignored
}

fn directory_hint(kind: &EventKind) -> Option<bool> {
    match kind {
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => Some(true),
        EventKind::Create(CreateKind::File) | EventKind::Remove(RemoveKind::File) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_dir_index_and_head() {
        assert_eq!(classify_git_dir(Path::new("index")), Scope::Index);
        assert_eq!(classify_git_dir(Path::new("HEAD")), Scope::Head);
        assert_eq!(classify_git_dir(Path::new("refs/heads/main")), Scope::Refs);
        assert_eq!(classify_git_dir(Path::new("packed-refs")), Scope::Refs);
        assert_eq!(classify_git_dir(Path::new("FETCH_HEAD")), Scope::Refs);
        assert_eq!(classify_git_dir(Path::new("ORIG_HEAD")), Scope::Refs);
        assert_eq!(classify_git_dir(Path::new("MERGE_HEAD")), Scope::Refs);
        assert_eq!(classify_git_dir(Path::new("config")), Scope::Config);
    }

    #[test]
    fn git_dir_noise_is_ignored() {
        for path in [
            "index.lock",
            "refs/heads/main.lock",
            "HEAD.lock",
            "objects/ab/cdef",
            "logs/HEAD",
            "gc.pid",
            "fsmonitor--daemon/foo",
            "",
        ] {
            assert_eq!(classify_git_dir(Path::new(path)), Scope::Ignored, "{path}");
        }
    }

    #[test]
    fn changes_start_idle() {
        assert!(Changes::default().is_idle());
        assert!(
            !Changes {
                worktree: true,
                ..Changes::default()
            }
            .is_idle()
        );
        assert!(
            !Changes {
                config: true,
                ..Changes::default()
            }
            .is_idle()
        );
    }

    #[test]
    fn worktree_paths_are_capped() {
        let mut changes = Changes::default();
        for i in 0..MAX_PATHS + 10 {
            push_path(&mut changes, PathBuf::from(format!("/worktree/file{i}")));
        }
        assert!(changes.worktree);
        assert_eq!(changes.paths.len(), MAX_PATHS);
    }
}
