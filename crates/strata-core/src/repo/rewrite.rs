//! The parts of a commit a rewrite has to reproduce.
//!
//! A message-only rewrite keeps every tree, so it does not need the changes a
//! commit introduced; it needs the raw material `git commit-tree` stores again:
//! the tree, the parents, the author and the message. Reading them here keeps
//! the write side on the CLI while the object parsing stays in `gix`.

use gix::bstr::{BStr, ByteSlice as _};

use super::Repo;
use crate::{Error, Oid, Result};

/// Everything `git commit-tree` needs to rebuild one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitSource {
    pub(crate) tree: Oid,
    /// First parent first, as stored. Empty for a root commit and for a
    /// shallow clone's boundary.
    pub(crate) parents: Vec<Oid>,
    pub(crate) author_name: String,
    pub(crate) author_email: String,
    /// Seconds since the Unix epoch.
    pub(crate) author_seconds: i64,
    /// Offset east of UTC, in seconds.
    pub(crate) author_offset: i32,
    /// The message bytes exactly as stored, so a rebuild does not re-encode a
    /// message git could not decode.
    pub(crate) message: Vec<u8>,
}

impl CommitSource {
    /// The author time in git's raw date format (`<seconds> <+-hhmm>`), which
    /// `commit-tree` reads from `GIT_AUTHOR_DATE`.
    pub(crate) fn author_date(&self) -> String {
        let sign = if self.author_offset < 0 { '-' } else { '+' };
        let offset = self.author_offset.abs();
        format!(
            "{} {}{:02}{:02}",
            self.author_seconds,
            sign,
            offset / 3600,
            offset % 3600 / 60
        )
    }
}

impl Repo {
    /// The tree, parents, author and message of `commit`.
    ///
    /// Parents come from [`Repo::parents`] so a shallow clone's boundary reads
    /// as a root, exactly as the log shows it.
    pub(crate) fn commit_source(&self, commit: Oid) -> Result<CommitSource> {
        let repo = self.local();
        let commit = repo.find_commit(commit).map_err(Error::gix)?;
        let tree = commit.tree_id().map_err(Error::gix)?.detach();
        let author = commit.author().map_err(Error::gix)?;
        let (author_seconds, author_offset) = author
            .time()
            .map_or((0, 0), |time| (time.seconds, time.offset));
        let message = commit.message_raw().map_err(Error::gix)?;
        Ok(CommitSource {
            tree,
            parents: self.parents(commit.id().detach())?,
            author_name: name(author.name),
            author_email: name(author.email),
            author_seconds,
            author_offset,
            message: message.to_vec(),
        })
    }
}

fn name(bytes: &BStr) -> String {
    bytes.to_str_lossy().into_owned()
}
