//! Read-only repository access through gix.
//!
//! `log.rs` (commit walk, refs, HEAD), `status.rs` (status, commit changes,
//! blob/file content), `conflicts.rs` (repository state, conflicted paths),
//! `upstream.rs` (tracking branches), `history.rs` (path-limited history),
//! `message.rs` (commit and merge messages) and `remotes.rs` (configured
//! remotes) are owned by separate tasks. `stat.rs` counts the lines a commit
//! added and removed. `compare.rs` compares two arbitrary commits. `range.rs`
//! plans the comparison of a multi-selection. `submodules.rs` and
//! `worktrees.rs` list the checkouts a repository owns beyond its own working
//! tree. `blame.rs` attributes each line of a file to a commit.
//! `in_progress.rs` reads details of an unfinished merge, rebase, cherry-pick,
//! revert, bisect or `am`. `config.rs` reports every effective configuration
//! value with the file and line that set it, without writing anything.
//! `rewrite.rs` reads the tree, parents, author and message a commit rewrite
//! has to reproduce.

mod blame;
mod bloom;
mod commit_graph;
mod compare;
mod config;
mod conflicts;
mod content_search;
mod history;
mod in_progress;
mod lfs;
mod log;
mod message;
mod range;
mod reflog;
mod remotes;
mod renames;
mod rewrite;
mod signature;
mod stat;
mod status;
mod submodules;
mod three_way;
mod tree;
mod upstream;
mod worktrees;

pub use blame::{Blame, BlameCommit, BlameHunk};
pub use commit_graph::{
    CommitGraphAdvice, CommitGraphFacts, CommitGraphThresholds, DEFAULT_MIN_COMMITS,
    DEFAULT_MIN_WALK,
};
pub use compare::{CompareFile, CompareMode, Comparison};
pub use config::{
    ConfigEntry, ConfigFilePaths, ConfigIdentity, ConfigInclude, ConfigLayer, ConfigOrigin,
    ConfigSources, ConfigValue, GitConfig, IncludeConditionKind, ValueRule, WELL_KNOWN_KEYS,
    WellKnownKey, validate_key_name, validate_value, well_known_key,
};
pub use conflicts::MergeTool;
pub use content_search::{
    ContentSearchEntries, ContentSearchEntry, ContentSearchMode, ContentSearchProgress,
    ContentSearchQuery,
};
pub use history::{PathHistory, PathHistoryEntries, PathHistoryEntry};
pub use in_progress::{InProgressCommit, InProgressDetails, RebaseOnto};
pub use log::CommitWalk;
pub use range::{SelectionRange, SelectionRangeKind, SelectionRangeMode, plan_selection_range};
pub(crate) use rewrite::CommitSource;

pub use signature::{
    CommitSignature, SignatureConfigHints, SignatureFormat, SignatureValidity,
    classify_signature_armor,
};
pub use status::{MAX_INCREMENTAL_PATHS, QuickStatus, quick_status};
pub use submodules::{SubmoduleInfo, SubmoduleStatus};
pub use three_way::ThreeWayRoles;
pub use upstream::UpstreamTarget;
pub use worktrees::WorktreeInfo;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gix::bstr::ByteSlice as _;

use stat::CommitDiffCache;

use crate::{Error, Result};

/// Cheap to clone; safe to share across threads.
#[derive(Clone)]
pub struct Repo {
    inner: gix::ThreadSafeRepository,
    workdir: Option<PathBuf>,
    /// Clones share the cache, so the worker that counts a commit's lines and
    /// the one that lists its files share a single first-parent tree walk,
    /// whichever of the two runs first.
    pub(crate) commit_diffs: Arc<Mutex<CommitDiffCache>>,
}

impl Repo {
    /// Discover the repository containing `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let inner = gix::ThreadSafeRepository::discover(path)
            .map_err(|_| Error::NotARepository(path.to_path_buf()))?;
        let workdir = inner.work_dir().map(Path::to_path_buf);
        Ok(Self {
            inner,
            workdir,
            commit_diffs: Arc::new(Mutex::new(CommitDiffCache::default())),
        })
    }

    pub fn workdir(&self) -> Option<&Path> {
        self.workdir.as_deref()
    }

    pub fn git_dir(&self) -> &Path {
        self.inner.git_dir()
    }

    /// The `working-tree-encoding` attribute for `path`, when the repository
    /// records one.
    ///
    /// Reading attributes needs the index, which a bare repository may not
    /// have; an unreadable index simply means no attribute, and detection
    /// falls back to the bytes.
    pub fn working_tree_encoding(&self, path: &str) -> Option<String> {
        let repo = self.local();
        let index = repo.index_or_empty().ok()?;
        let mut stack = repo
            .attributes_only(
                &index,
                gix::worktree::stack::state::attributes::Source::IdMapping,
            )
            .ok()?;
        let mut outcome = stack.selected_attribute_matches(["working-tree-encoding"]);
        let platform = stack.at_path(path, None).ok()?;
        platform.matching_attributes(&mut outcome);
        outcome.iter().find_map(|matched| {
            if matched.assignment.name.as_str() != "working-tree-encoding" {
                return None;
            }
            match matched.assignment.state {
                gix::attrs::StateRef::Value(value) => {
                    Some(value.as_bstr().to_str_lossy().into_owned())
                }
                _ => None,
            }
        })
    }

    /// A thread-local handle for gix calls.
    pub(crate) fn local(&self) -> gix::Repository {
        self.inner.to_thread_local()
    }
}
