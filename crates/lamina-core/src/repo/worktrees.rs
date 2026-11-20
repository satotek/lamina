//! Linked worktrees registered under `$GIT_COMMON_DIR/worktrees`.

use std::path::{Path, PathBuf};

use super::Repo;
use crate::{Error, HeadInfo, Result};

/// One worktree of the repository, as `git worktree list` reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Absolute path of the checkout. A linked worktree whose checkout was
    /// deleted keeps the path git recorded for it, which is what makes
    /// `prunable` visible.
    pub path: PathBuf,
    /// Where the worktree's HEAD points. `None` when it cannot be read, e.g. a
    /// corrupt private git directory; one broken entry must not hide the rest.
    pub head: Option<HeadInfo>,
    /// True for the repository's main worktree. A bare repository has no main
    /// worktree, so it only contributes its linked ones.
    pub is_main: bool,
    /// A locked worktree cannot be pruned, moved or removed.
    pub locked: bool,
    /// The reason recorded with the lock; `None` when unlocked, and also when
    /// locked without one.
    pub lock_reason: Option<String>,
    /// The checkout directory (or the file it points at) is missing, so
    /// `git worktree prune` would remove the registration.
    pub prunable: bool,
}

impl Repo {
    /// The main worktree followed by every linked worktree.
    pub fn worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let repo = self.local();
        let mut worktrees = Vec::new();

        // `main_repo()` also answers when this handle belongs to a linked
        // worktree, which is how a session opened on one still sees the rest.
        let main = repo.main_repo().map_err(Error::gix)?;
        if let Some(path) = main.workdir() {
            worktrees.push(WorktreeInfo {
                path: path.to_path_buf(),
                head: head_info(&main).ok(),
                is_main: true,
                locked: false,
                lock_reason: None,
                prunable: false,
            });
        }

        // `worktrees()` returns proxies that read the lock and `gitdir` files
        // without opening the checkout, which a prunable entry no longer has.
        for proxy in repo.worktrees().map_err(Error::gix)? {
            let recorded_path = proxy.base().ok();
            let locked = proxy.is_locked();
            let lock_reason = proxy
                .lock_reason()
                .map(|reason| reason.to_string())
                .filter(|reason| !reason.is_empty());
            let prunable = proxy.is_prunable();

            let linked = proxy
                .into_repo_with_possibly_inaccessible_worktree()
                .map_err(Error::gix)?;
            let path = recorded_path
                .or_else(|| linked.workdir().map(Path::to_path_buf))
                .unwrap_or_else(|| linked.git_dir().to_path_buf());

            worktrees.push(WorktreeInfo {
                path,
                head: head_info(&linked).ok(),
                is_main: false,
                locked,
                lock_reason,
                prunable,
            });
        }

        Ok(worktrees)
    }
}

/// The same resolution [`Repo::head`] performs, for a worktree's own repository
/// handle rather than the session's.
fn head_info(repo: &gix::Repository) -> Result<HeadInfo> {
    let head = repo.head().map_err(Error::gix)?;

    if head.is_detached() {
        let target = head
            .id()
            .ok_or_else(|| Error::other("detached HEAD has no object id"))?;
        return Ok(HeadInfo::Detached {
            target: target.detach(),
        });
    }

    let name = head
        .referent_name()
        .ok_or_else(|| Error::other("HEAD is neither detached nor symbolic"))?;
    Ok(HeadInfo::Branch {
        short_name: name.shorten().to_string(),
        target: head.id().map(|id| id.detach()),
    })
}
