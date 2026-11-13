//! Submodules recorded in `.gitmodules`.

use gix::bstr::ByteSlice;

use super::Repo;
use crate::{Error, Oid, Result};

/// How a submodule's checkout compares with the commit its superproject records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubmoduleStatus {
    /// The submodule repository or its checkout is missing, so there is nothing
    /// to compare yet. `git submodule update --init` fills it in.
    Uninitialised,
    /// The checked-out commit is the recorded one and the worktree is clean.
    UpToDate,
    /// The submodule is checked out at a commit other than the one recorded in
    /// the superproject's HEAD.
    CommitDiffers,
    /// The recorded commit is checked out, but the submodule has local changes.
    Modified,
}

/// One entry of `.gitmodules` with the state of its checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleInfo {
    /// The name from `.gitmodules`, e.g. `libs/parser`.
    pub name: String,
    /// Path relative to the superproject's worktree, using `/` separators.
    pub path: String,
    /// The configured URL, unresolved: a relative URL such as `../lib.git` is
    /// reported as written. `None` when the entry has no URL or it cannot be
    /// parsed.
    pub url: Option<String>,
    /// The gitlink recorded in HEAD's tree. `None` on an unborn branch, or for
    /// a submodule that only exists in the index so far.
    pub recorded_commit: Option<Oid>,
    /// The submodule's checked-out HEAD. `None` while it is uninitialised.
    pub checked_out_commit: Option<Oid>,
    pub status: SubmoduleStatus,
}

impl Repo {
    /// Every submodule from `.gitmodules`, sorted by path.
    ///
    /// A malformed `.gitmodules` is an error, like any other failed read; a
    /// single uninitialised entry is not. The status costs one metadata walk of
    /// each initialised submodule's worktree (gix stops at the first change),
    /// which is the same work `git submodule status` does and stays cheap for
    /// the sidebar's refresh cadence.
    pub fn submodules(&self) -> Result<Vec<SubmoduleInfo>> {
        let mut repo = self.local();
        // `submodule.<name>.*` overrides live in the configuration, which changes
        // while the session stays open (`git submodule add`, `git submodule init`),
        // so the cached snapshot has to be refreshed first.
        let _ = repo.reload();
        let Some(submodules) = repo.submodules().map_err(Error::gix)? else {
            return Ok(Vec::new());
        };
        // `head_id()` reads `HEAD^{tree}` and fails on an unborn branch, where
        // no commit is recorded for any submodule.
        let unborn = repo.head().map_err(Error::gix)?.is_unborn();

        let mut infos = Vec::new();
        for submodule in submodules {
            let path = submodule.path().map_err(Error::gix)?;
            let recorded_commit = if unborn {
                None
            } else {
                submodule.head_id().map_err(Error::gix)?
            };
            let url = submodule.url().ok().map(|url| url.to_bstring().to_string());
            let status = submodule
                .status(gix::submodule::config::Ignore::None, true)
                .map_err(Error::gix)?;

            // A cloned repository without a checkout (an old-form or deinitialised
            // submodule) has nothing to show, so it counts as uninitialised here.
            let initialised = status.state.repository_exists && status.state.worktree_checkout;
            let checked_out_commit = initialised.then_some(status.checked_out_head_id).flatten();
            let state = match checked_out_commit {
                None => SubmoduleStatus::Uninitialised,
                Some(checked_out)
                    if recorded_commit.is_some_and(|recorded| recorded != checked_out) =>
                {
                    SubmoduleStatus::CommitDiffers
                }
                Some(_) if status.is_dirty() == Some(true) => SubmoduleStatus::Modified,
                Some(_) => SubmoduleStatus::UpToDate,
            };

            infos.push(SubmoduleInfo {
                name: submodule.name().to_string(),
                path: path.to_str_lossy().into_owned(),
                url,
                recorded_commit,
                checked_out_commit,
                status: state,
            });
        }

        // The `.gitmodules` order is not meaningful, and the sidebar groups by path.
        infos.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.name.cmp(&b.name)));
        Ok(infos)
    }
}
