//! Plain data types shared between core modules and the UI.

pub use gix::ObjectId as Oid;

/// One commit as shown in the log. Owned and cheap to clone into the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: Oid,
    pub parents: Vec<Oid>,
    /// First line of the message.
    pub summary: String,
    pub author_name: String,
    pub author_email: String,
    /// Seconds since the Unix epoch.
    pub author_time: i64,
    pub committer_time: i64,
    /// This commit is a shallow clone's boundary: its parents are outside the
    /// fetched history, so a missing parent edge means "not fetched" rather
    /// than "root commit".
    pub shallow_boundary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefInfo {
    /// Full name, e.g. `refs/heads/main`.
    pub full_name: String,
    /// Short name, e.g. `main` or `origin/main`.
    pub short_name: String,
    pub kind: RefKind,
    /// Peeled commit id (annotated tags are peeled to their commit).
    pub target: Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadInfo {
    /// HEAD points to a branch. `target` is `None` for an unborn branch.
    Branch {
        short_name: String,
        target: Option<Oid>,
    },
    Detached {
        target: Oid,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

/// A file changed between two trees (commit view) or in index/worktree (status view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Repository-relative path using `/` separators.
    pub path: String,
    /// Previous path for renames/copies.
    pub old_path: Option<String>,
    pub kind: ChangeKind,
}

/// What a tree entry is, as the file list of a commit needs to draw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TreeEntryKind {
    File,
    Directory,
    /// A symbolic link; its blob content is the link target.
    Symlink,
    /// A gitlink to another repository.
    Submodule,
}

/// One entry of a directory at a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The entry's name inside its directory.
    pub name: String,
    /// Repository-relative path using `/` separators.
    pub path: String,
    pub kind: TreeEntryKind,
    /// The raw git mode, e.g. `0o100644` or `0o120000`.
    pub mode: u16,
    /// Blob size in bytes for files and symlinks. `None` for directories and
    /// submodules, and when the object header cannot be read cheaply.
    pub size: Option<u64>,
}

/// Working tree status split the way the staging UI shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeStatus {
    /// HEAD vs index.
    pub staged: Vec<FileChange>,
    /// Index vs working tree, including untracked files.
    pub unstaged: Vec<FileChange>,
}

/// One entry of the stash list. `index` is the `N` in `stash@{N}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashInfo {
    pub index: usize,
    pub id: Oid,
    pub message: String,
    /// When the stash was created, in seconds since the Unix epoch.
    pub created_time: i64,
    /// The branch the stash was made on, parsed from the stash message.
    /// `None` when HEAD was detached ("WIP on (no branch)") or the message
    /// does not name a branch.
    pub branch: Option<String>,
}

/// What a reflog message says the action was, as far as the messages lamina
/// recognises go. Anything else is [`ReflogKind::Other`] and is shown as the
/// raw message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReflogKind {
    Commit,
    /// `git commit --amend`.
    Amend,
    /// A checkout or switch, including one that creates a branch.
    Checkout,
    Reset,
    /// A message-only rewrite of history (`reword: ...`).
    Reword,
    /// A rebase entry other than its finish (start, pick, squash, abort).
    RebaseStart,
    /// The entry that records the branch the rebase left behind.
    RebaseFinish,
    Merge,
    CherryPick,
    Pull,
    /// `git branch` creating a branch (or `git switch -c`).
    BranchCreate,
    /// `git branch -m`.
    BranchRename,
    Other,
}

impl ReflogKind {
    /// Classifies one reflog message. The prefixes are git's own wording, which
    /// is why they are matched literally instead of trying to infer the action
    /// from the text after the colon.
    pub fn parse(message: &str) -> Self {
        let message = message.trim_start();
        // The amend form shares the `commit` prefix, so it is checked first.
        if message.starts_with("commit (amend)") {
            Self::Amend
        } else if message.starts_with("commit") {
            Self::Commit
        } else if message.starts_with("checkout: moving from ")
            || message.starts_with("switch: moving from ")
        {
            Self::Checkout
        } else if message.starts_with("reset: ") {
            Self::Reset
        } else if message.starts_with("reword: ") {
            Self::Reword
        } else if message.starts_with("rebase (finish)") {
            Self::RebaseFinish
        } else if message.starts_with("rebase (") {
            Self::RebaseStart
        } else if message.starts_with("merge ") {
            Self::Merge
        } else if message.starts_with("cherry-pick") {
            Self::CherryPick
        } else if message.starts_with("pull") {
            Self::Pull
        } else if message.starts_with("branch: Created") {
            Self::BranchCreate
        } else if message.starts_with("Branch: renamed") {
            Self::BranchRename
        } else {
            Self::Other
        }
    }

    /// Whether the state before an entry of this kind can be restored with a
    /// branch move (see [`ReflogKind::moves_branch`]) or a switch.
    ///
    /// `RebaseStart`, branch creation and rename have no state of their own to
    /// go back to: a rebase start is undone with its finish or abort entry, and
    /// a created or renamed branch is not something a reset restores.
    pub fn can_restore(self) -> bool {
        matches!(
            self,
            Self::Commit
                | Self::Amend
                | Self::Checkout
                | Self::Reset
                | Self::Reword
                | Self::RebaseFinish
                | Self::Merge
                | Self::CherryPick
                | Self::Pull
        )
    }

    /// Whether restoring this entry moves the branch rather than switching it.
    pub fn moves_branch(self) -> bool {
        self.can_restore() && self != Self::Checkout
    }
}

/// One reflog entry, as the reflog view and undo read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogEntry {
    /// `HEAD`, or the local branch whose reflog holds the entry.
    pub reference: String,
    /// The ref's target before the action.
    pub old_id: Oid,
    /// The ref's target after the action.
    pub new_id: Oid,
    pub committer_name: String,
    pub committer_email: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
    /// git's message for the action, e.g. `commit: fix the parser`.
    pub message: String,
    pub kind: ReflogKind,
}

/// Line counts of one file changed by a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    /// Repository-relative path using `/` separators.
    pub path: String,
    /// Previous path for a rename or copy; `None` otherwise.
    pub old_path: Option<String>,
    pub added: u64,
    pub removed: u64,
    /// A binary file has no line counts; `added` and `removed` are both 0.
    pub binary: bool,
}

/// Line counts of a commit against its first parent, or against the empty tree
/// for a root commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitStat {
    /// Sorted by `path`.
    pub files: Vec<FileStat>,
    /// Totals over `files`; binary files add nothing.
    pub added: u64,
    pub removed: u64,
}

/// Tracking state of a local branch against its upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamInfo {
    /// Short name of the upstream, e.g. `origin/main`.
    pub name: String,
    /// Commits on the local branch that the upstream lacks.
    ///
    /// `None` when the count cannot be computed reliably, for example when the
    /// merge base is outside a shallow clone's fetched history. A missing
    /// upstream ref also leaves this unknown.
    pub ahead: Option<u32>,
    /// Commits on the upstream that the local branch lacks, with the same
    /// uncertainty as [`ahead`](Self::ahead).
    pub behind: Option<u32>,
    /// The configured upstream ref no longer exists (e.g. deleted on the remote and pruned).
    pub gone: bool,
}

/// A configured remote.
///
/// A remote is listed even while it has no remote-tracking branches: fetching
/// and pushing only need its URLs, and the UI offers every configured remote
/// before the first fetch created any refs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    pub name: String,
    /// The first `remote.<name>.url`, as git resolves it.
    pub fetch_url: Option<String>,
    /// The push URL, but only when pushes do not already use `fetch_url`.
    pub push_url: Option<String>,
}

/// An operation git left half-done, as recorded in the git dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RepoState {
    #[default]
    Normal,
    Merging,
    Rebasing,
    CherryPicking,
    Reverting,
    Bisecting,
    /// `git am` / applying a mailbox.
    ApplyingPatches,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictKind {
    BothModified,
    BothAdded,
    BothDeleted,
    AddedByUs,
    AddedByThem,
    DeletedByUs,
    DeletedByThem,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictEntry {
    pub path: String,
    pub kind: ConflictKind,
}

/// Index stages of a conflicted path: 1 = common ancestor, 2 = ours, 3 = theirs.
/// A side is `None` when that stage is absent (e.g. deleted on that side).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictSides {
    pub base: Option<Vec<u8>>,
    pub ours: Option<Vec<u8>>,
    pub theirs: Option<Vec<u8>>,
}
