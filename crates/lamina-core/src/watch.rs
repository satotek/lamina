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
//! The callback receives the burst's first [`Changes`] a few milliseconds after
//! its first event, then at most one per quiet `debounce` window, and at least
//! one every second while events keep streaming in.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::event::{CreateKind, RemoveKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{Error, Result};

/// How many distinct worktree paths a single [`Changes`] keeps.
const MAX_PATHS: usize = 1000;

/// Upper bound on how long an uninterrupted event stream can delay a callback.
const MAX_LATENCY: Duration = Duration::from_secs(1);

/// How long the first event of a burst waits before the first callback.
///
/// A single save lands as a handful of events (create, modify, close) within a
/// few milliseconds, so this settles that burst. It is deliberately much
/// shorter than the quiet window: the rest of the burst is coalesced *after*
/// the first refresh rather than in front of it.
const LEADING_SETTLE: Duration = Duration::from_millis(30);

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
    /// Watcher overflowed, reported a rescan/root event, dropped paths past
    /// `MAX_PATHS`, or saw an ignore/attribute file change, so status must fall
    /// back to a full scan.
    pub rescan: bool,
    /// Worktree paths that changed, deduplicated, in event order.
    pub paths: Vec<PathBuf>,
}

impl Changes {
    fn is_idle(&self) -> bool {
        !(self.worktree || self.index || self.refs || self.head || self.config || self.rescan)
    }
}

/// Watches a worktree and its git dir, reporting coalesced [`Changes`].
///
/// Dropping it stops watching and waits for the worker thread, unless the drop
/// happens from inside the callback itself. The platform backend is stopped
/// afterwards on its own thread, so a drop never waits for it: `notify`'s
/// FSEvents backend busy-waits for its run loop and purges queued device
/// events, which can take seconds on a loaded machine, and no change can reach
/// the caller once the worker is gone.
pub struct RepoWatcher {
    watcher: Option<RecommendedWatcher>,
    /// Cloned into the backend's callback; kept here so a drop can wake the
    /// worker without waiting for the backend to shut down.
    wake: Option<Sender<Wake>>,
    worker: Option<JoinHandle<()>>,
}

/// One message for the watcher worker.
enum Wake {
    /// A raw event from the platform backend.
    Event(notify::Result<Event>),
    /// The owner is dropping the watcher.
    Stop,
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
        let wake = tx.clone();
        let mut watcher = notify::recommended_watcher(move |event| {
            // Unbounded and non-blocking from notify's point of view; the worker
            // is the only place that may take its time.
            let _ = tx.send(Wake::Event(event));
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
        let watch = Watch {
            workdir,
            git_dir,
            debounce,
            filter,
        };

        let worker = thread::Builder::new()
            .name("lamina-repo-watcher".into())
            .spawn(move || run(rx, watch, on_change))
            .map_err(|err| Error::other(format!("failed to start watcher thread: {err}")))?;

        Ok(Self {
            watcher: Some(watcher),
            wake: Some(wake),
            worker: Some(worker),
        })
    }
}

impl Drop for RepoWatcher {
    fn drop(&mut self) {
        // Wake and join the worker first: the callback and the senders it holds
        // live on that thread, so joining it is what guarantees no change after
        // the drop returns.
        if let Some(wake) = self.wake.take() {
            let _ = wake.send(Wake::Stop);
        }
        if let Some(worker) = self.worker.take() {
            // A callback that drops its own watcher must not join itself.
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
        // Stop the backend last, on its own thread: its teardown blocks for as
        // long as its run loop takes to park, and nothing waits on it anymore.
        // A failed spawn drops it here instead, which is only slower.
        if let Some(watcher) = self.watcher.take() {
            let _ = thread::Builder::new()
                .name("lamina-repo-watcher-stop".into())
                .spawn(move || drop(watcher));
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
            if event.need_rescan() {
                changes.worktree = true;
                changes.rescan = true;
            }
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
                        Scope::Rescan => {
                            changes.worktree = true;
                            changes.rescan = true;
                        }
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
                    changes.rescan = true;
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
        let index = match repo.index_or_empty() {
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
            if is_rules_file(&relative) {
                push_path(changes, workdir.given.join(relative));
                continue;
            }
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
    if is_rules_file(&path) {
        changes.rescan = true;
    }
    if changes.paths.len() < MAX_PATHS {
        changes.paths.push(path);
    } else {
        changes.rescan = true;
    }
}

fn is_rules_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(OsStr::to_str),
        Some(".gitignore" | ".gitattributes" | ".gitmodules")
    )
}

/// The debounce loop.
///
/// The burst's first callback is a leading edge: it only waits for
/// [`LEADING_SETTLE`], so one edit shows up immediately. Every callback after
/// that waits for the quiet window, and a stream that never goes quiet is
/// still flushed [`MAX_LATENCY`] after the previous callback, so a checkout of
/// thousands of files costs a few refreshes instead of one per file.
fn run(rx: Receiver<Wake>, watch: Watch, on_change: impl Fn(Changes)) {
    loop {
        let first = match rx.recv() {
            Ok(Wake::Event(event)) => event,
            Ok(Wake::Stop) | Err(_) => return,
        };

        let started = Instant::now();
        let mut last = started;
        let mut last_flush: Option<Instant> = None;
        let mut batch = vec![first];
        let mut disconnected = false;

        loop {
            let deadline = flush_deadline(started, last, last_flush, watch.debounce);
            let now = Instant::now();
            if now >= deadline {
                // Either the leading edge or the quiet window; an empty batch
                // here means the burst has gone quiet and the worker can block
                // again.
                let settled = batch.is_empty();
                flush(&watch, &mut batch, &on_change);
                last_flush = Some(now);
                if settled {
                    break;
                }
                continue;
            }
            match rx.recv_timeout(deadline - now) {
                Ok(Wake::Event(event)) => {
                    batch.push(event);
                    last = Instant::now();
                }
                // The owner is going away; its remaining events are noise.
                Ok(Wake::Stop) => return,
                Err(RecvTimeoutError::Timeout) => {
                    flush(&watch, &mut batch, &on_change);
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    flush(&watch, &mut batch, &on_change);
                    disconnected = true;
                    break;
                }
            }
        }

        if disconnected {
            return;
        }
    }
}

/// Delivers the events in hand, if any of them changed something.
fn flush(watch: &Watch, batch: &mut Vec<notify::Result<Event>>, on_change: &impl Fn(Changes)) {
    if batch.is_empty() {
        return;
    }
    let changes = watch.changes(batch);
    batch.clear();
    if !changes.is_idle() {
        on_change(changes);
    }
}

/// When the batch in hand is delivered.
///
/// `started` is the burst's first event, `last_event` the newest one folded
/// into the batch, and `last_flush` the previous delivery if this burst had
/// one. The leading edge is measured from `started`, so a sustained burst still
/// gets its first refresh promptly instead of resetting the short window on
/// every write.
fn flush_deadline(
    started: Instant,
    last_event: Instant,
    last_flush: Option<Instant>,
    debounce: Duration,
) -> Instant {
    match last_flush {
        None => started + LEADING_SETTLE,
        Some(flushed) => (last_event + debounce).min(flushed + MAX_LATENCY),
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
    /// `.git/info/exclude` or `.git/info/attributes` changed; requires a full status scan.
    Rescan,
    /// Git bookkeeping that cannot change what the UI shows.
    Ignored,
}

/// Classify a path relative to the git dir.
///
/// `objects/` churn (fetches, gc), reflogs and lock files dominate event volume
/// and never affect the views, so they are dropped before they can wake the UI.
fn classify_git_dir(relative: &Path) -> Scope {
    let mut components = relative.components();
    let Some(Component::Normal(first)) = components.next() else {
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
    if first == OsStr::new("info")
        && matches!(
            components.next(),
            Some(Component::Normal(name))
                if name == OsStr::new("exclude") || name == OsStr::new("attributes")
        )
    {
        return Scope::Rescan;
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
        assert_eq!(classify_git_dir(Path::new("info/exclude")), Scope::Rescan);
        assert_eq!(
            classify_git_dir(Path::new("info/attributes")),
            Scope::Rescan
        );
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
        assert!(
            !Changes {
                rescan: true,
                ..Changes::default()
            }
            .is_idle()
        );
    }

    #[test]
    fn worktree_paths_are_capped_and_mark_rescan() {
        let mut changes = Changes::default();
        for i in 0..MAX_PATHS {
            push_path(&mut changes, PathBuf::from(format!("/worktree/file{i}")));
        }
        assert!(changes.worktree);
        assert!(!changes.rescan);
        assert_eq!(changes.paths.len(), MAX_PATHS);

        push_path(&mut changes, PathBuf::from("/worktree/overflow"));
        assert!(changes.rescan);
        assert_eq!(changes.paths.len(), MAX_PATHS);
    }

    #[test]
    fn rescan_flag_and_rules_files_force_rescan() {
        // `Root::new` makes the root absolute, and an event path only matches
        // in that form; `/worktree` alone becomes `C:\worktree` on Windows.
        let worktree = std::path::absolute("/worktree").expect("absolute worktree");
        let git_dir = worktree.join(".git");
        let watch = Watch {
            workdir: Some(Root::new(&worktree)),
            git_dir: Root::new(&git_dir),
            debounce: Duration::from_millis(10),
            filter: None,
        };

        let overflow = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        let changes = watch.changes(&[Ok(overflow)]);
        assert!(changes.worktree);
        assert!(changes.rescan);
        assert!(!changes.is_idle());

        let gitignore = notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(worktree.join(".gitignore"));
        let changes = watch.changes(&[Ok(gitignore)]);
        assert!(changes.worktree);
        assert!(changes.rescan);

        let info_exclude = notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(git_dir.join("info").join("exclude"));
        let changes = watch.changes(&[Ok(info_exclude)]);
        assert!(changes.worktree);
        assert!(changes.rescan);
    }

    /// The debounce used by the session, so the rule is tested at its real
    /// proportions instead of a convenient one.
    const DEBOUNCE: Duration = Duration::from_millis(150);

    /// Runs the worker's flush loop over a stream of events and returns, in
    /// order, when a callback carrying events would fire. `events` are offsets
    /// from the first one, so nothing here sleeps.
    fn callbacks(events: &[Duration]) -> Vec<Duration> {
        let start = Instant::now();
        let mut out = Vec::new();
        let mut index = 0;
        // One pass per burst: the worker blocks for an event, coalesces, and
        // blocks again once a quiet window passes with nothing held.
        while index < events.len() {
            let started = start + events[index];
            index += 1;
            let mut last_event = started;
            let mut last_flush = None;
            let mut batch = 1;

            loop {
                let deadline = flush_deadline(started, last_event, last_flush, DEBOUNCE);
                while let Some(offset) = events.get(index) {
                    let at = start + *offset;
                    if at >= deadline {
                        break;
                    }
                    index += 1;
                    batch += 1;
                    last_event = at;
                }
                let deadline = flush_deadline(started, last_event, last_flush, DEBOUNCE);
                if events
                    .get(index)
                    .is_some_and(|offset| start + *offset < deadline)
                {
                    continue;
                }
                if batch == 0 {
                    break;
                }
                out.push(deadline.duration_since(start));
                batch = 0;
                last_flush = Some(deadline);
            }
        }
        out
    }

    #[test]
    fn one_edit_is_reported_after_the_leading_settle() {
        // A single save: create, modify, close, all within a millisecond.
        let reported = callbacks(&[Duration::ZERO, Duration::from_millis(1)]);
        assert_eq!(
            reported.len(),
            1,
            "one edit must be one refresh: {reported:?}"
        );
        assert_eq!(
            reported[0], LEADING_SETTLE,
            "the leading edge waits for the save's burst, not the quiet window"
        );
        assert!(
            reported[0] < DEBOUNCE,
            "the first refresh must beat the old quiet-window rule"
        );
    }

    #[test]
    fn the_rest_of_a_burst_waits_for_the_quiet_window() {
        let reported = callbacks(&[
            Duration::ZERO,
            Duration::from_millis(1),
            Duration::from_millis(100),
            Duration::from_millis(120),
        ]);
        assert_eq!(reported.len(), 2, "a late event earns one more refresh");
        assert_eq!(reported[0], LEADING_SETTLE);
        assert_eq!(
            reported[1],
            Duration::from_millis(120) + DEBOUNCE,
            "the events after the leading refresh coalesce on the quiet window"
        );
    }

    #[test]
    fn a_flood_of_writes_coalesces_into_a_few_refreshes() {
        // 200 writes half a millisecond apart, like a checkout of 200 files.
        let events: Vec<Duration> = (0..200)
            .map(|index| Duration::from_micros(index * 500))
            .collect();
        let reported = callbacks(&events);
        assert!(
            reported.len() <= 3,
            "200 writes produced {} refreshes: {reported:?}",
            reported.len()
        );
        assert_eq!(
            reported[0], LEADING_SETTLE,
            "the flood still shows its first refresh promptly"
        );
        assert!(
            reported[1] >= Duration::from_micros(199 * 500) + DEBOUNCE,
            "the second refresh waits for the writes to stop: {reported:?}"
        );
    }

    #[test]
    fn an_endless_stream_flushes_once_a_second() {
        let events: Vec<Duration> = (0..63)
            .map(|index| Duration::from_millis(index * 40))
            .collect();
        let reported = callbacks(&events);
        assert!(
            reported.len() <= 4,
            "a 2.5 s stream produced {} refreshes: {reported:?}",
            reported.len()
        );
        assert_eq!(reported[0], LEADING_SETTLE);
        for pair in reported.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap <= MAX_LATENCY,
                "a callback waited {gap:?}, more than {MAX_LATENCY:?}"
            );
        }
    }

    #[test]
    fn a_quiet_burst_after_a_flush_delivers_the_rest_once() {
        // The leading refresh has already happened and the writes stop; the
        // late event starts a fresh burst with its own leading edge.
        let reported = callbacks(&[
            Duration::ZERO,
            Duration::from_millis(200),
            Duration::from_millis(210),
            Duration::from_millis(220),
        ]);
        assert_eq!(reported.len(), 2);
        assert_eq!(
            reported[1],
            Duration::from_millis(200) + LEADING_SETTLE,
            "the late burst leads with one refresh and folds 210/220 into it"
        );
    }
}
