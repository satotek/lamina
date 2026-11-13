//! Record of every `git` invocation, shown in the UI's command log panel.

use std::collections::VecDeque;
use std::fmt;
use std::process::Output;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

/// Captured stdout/stderr is cut here. Progress output and large patches can
/// otherwise put megabytes of text into memory per invocation.
pub const MAX_CAPTURE_BYTES: usize = 64 * 1024;

/// Older entries are dropped beyond this, so a long session stays bounded.
pub const MAX_LOG_ENTRIES: usize = 500;

/// One completed (or failed-to-spawn) `git` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub args: Vec<String>,
    pub started_at: SystemTime,
    pub duration: Duration,
    /// `None` when the process was killed by a signal or never started.
    pub exit_code: Option<i32>,
    /// Captured output, truncated to [`MAX_CAPTURE_BYTES`] and lossily decoded.
    pub stdout: String,
    /// Captured output, truncated to [`MAX_CAPTURE_BYTES`] and lossily decoded.
    pub stderr: String,
}

impl CommandRecord {
    pub(crate) fn from_output(
        args: Vec<String>,
        started_at: SystemTime,
        duration: Duration,
        output: &Output,
    ) -> Self {
        Self::capture(
            args,
            started_at,
            duration,
            output.status.code(),
            &output.stdout,
            &output.stderr,
        )
    }

    pub(crate) fn capture(
        args: Vec<String>,
        started_at: SystemTime,
        duration: Duration,
        exit_code: Option<i32>,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Self {
        Self {
            args,
            started_at,
            duration,
            exit_code,
            stdout: truncated(stdout),
            stderr: truncated(stderr),
        }
    }
}

fn truncated(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_CAPTURE_BYTES {
        return text.into_owned();
    }
    let mut end = MAX_CAPTURE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// A line of progress output, delivered while a command is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamLine {
    pub text: String,
}

type Subscriber = Arc<dyn Fn(&CommandRecord) + Send + Sync>;

/// Shared, thread-safe command history.
///
/// Clones of this are cheap; pass the same instance to several [`super::Git`]
/// handles (for example one per open repository) to get a single log.
#[derive(Default)]
pub struct CommandLog {
    entries: Mutex<VecDeque<CommandRecord>>,
    subscriber: Mutex<Option<Subscriber>>,
}

impl CommandLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an entry and notify the subscriber, if one is installed.
    pub fn record(&self, record: CommandRecord) {
        let subscriber = lock(&self.subscriber).clone();
        let notification = subscriber.as_ref().map(|_| record.clone());
        {
            let mut entries = lock(&self.entries);
            while entries.len() >= MAX_LOG_ENTRIES {
                entries.pop_front();
            }
            entries.push_back(record);
        }
        // Call outside the lock: a subscriber that reads the log must not
        // deadlock, and it may be slow (UI thread hand-off).
        if let (Some(subscriber), Some(record)) = (subscriber, notification) {
            subscriber(&record);
        }
    }

    /// Oldest first, so the log reads top-to-bottom like a terminal.
    pub fn snapshot(&self) -> Vec<CommandRecord> {
        lock(&self.entries).iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        lock(&self.entries).len()
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.entries).is_empty()
    }

    pub fn clear(&self) {
        lock(&self.entries).clear();
    }

    /// Called after every invocation, including failures. Replaces any
    /// previously installed subscriber.
    pub fn set_subscriber(&self, subscriber: impl Fn(&CommandRecord) + Send + Sync + 'static) {
        *lock(&self.subscriber) = Some(Arc::new(subscriber));
    }

    pub fn clear_subscriber(&self) {
        *lock(&self.subscriber) = None;
    }
}

impl fmt::Debug for CommandLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandLog")
            .field("entries", &self.len())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking subscriber must not make the whole log unusable.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
