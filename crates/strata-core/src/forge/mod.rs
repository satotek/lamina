//! Optional forge integration: pull requests and web links on the repository's
//! own hosting service.
//!
//! [`remote`] turns a clone URL into a host-neutral [`ForgeRemote`], which
//! answers both "which service is this" and "where is this commit, file,
//! branch or pull request on its web pages". Pull-request questions go through
//! [`ForgeCli`], so a repository view asks for "the pull requests of this
//! checkout" without knowing which program answers: `gh` on GitHub and `az`
//! with the `azure-devops` extension on Azure DevOps. Bitbucket Cloud is links
//! only for now.
//!
//! Every call is made only when the app has already found the program and
//! confirmed it is usable for the repository's host. Nothing reads a token:
//! each CLI owns its credentials.

pub mod az;
pub(crate) mod cli;
pub mod gh;
pub mod remote;

use std::path::Path;

use crate::Result;

pub use az::Az;
pub use gh::Gh;
pub use remote::{AzureDevOpsRemote, BitbucketRemote, ForgeKind, ForgeRemote};

/// Where a pull request is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PullRequestState {
    Open,
    /// An open pull request its author marked as not ready for review.
    Draft,
    Merged,
    Closed,
}

impl PullRequestState {
    /// Whether the pull request is still active, which includes a draft.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Open | Self::Draft)
    }
}

/// The review verdict on an open pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    ReviewRequired,
    None,
}

/// What the checks attached to a pull request look like as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChecksState {
    Passing,
    Failing,
    Pending,
    /// No checks are attached, or the host reports none for this branch.
    #[default]
    None,
}

/// The checks attached to a pull request, counted by outcome.
///
/// Git counts a neutral or skipped check as not failing: the summary separates
/// "passed", "failed" and "still running" so a tooltip can say `3/4 passing`
/// even while one check waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PullRequestChecks {
    pub state: ChecksState,
    pub passing: u32,
    pub failing: u32,
    pub pending: u32,
}

impl PullRequestChecks {
    /// Every check that counts towards the summary.
    pub fn total(self) -> u32 {
        self.passing + self.failing + self.pending
    }
}

/// One pull request as the branch list needs it: no body, comments or files,
/// so a hundred of them fit in one `gh` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSummary {
    pub number: u64,
    pub title: String,
    pub state: PullRequestState,
    /// The branch the pull request comes from, without its remote.
    pub head_branch: String,
    /// The owner of the fork the head branch lives on. `None` when the host
    /// does not report it.
    pub head_owner: Option<String>,
    /// The branch the pull request would merge into.
    pub base: String,
    pub url: String,
    pub review: ReviewState,
    pub checks: PullRequestChecks,
    /// ISO 8601, as the host reports it. Compared as text when several pull
    /// requests share a branch.
    pub updated_at: String,
}

/// The two operations the app needs from a forge's command-line program.
///
/// `Send` lets the app move the chosen implementation into a background task
/// without knowing which program it is.
pub trait ForgeCli: Send {
    /// The pull requests whose head branch is one of the repository's
    /// branches, newest first as the host lists them.
    fn pull_requests_for_branches(&self, repo_dir: &Path) -> Result<Vec<PullRequestSummary>>;

    /// Checks out the pull request's branch in `repo_dir`.
    fn checkout_pull_request(&self, repo_dir: &Path, number: u64) -> Result<()>;
}

/// Every pull request of `repo_dir`'s repository, using the `gh` on `PATH`.
pub fn pull_requests_for_branches(repo_dir: &Path) -> Result<Vec<PullRequestSummary>> {
    Gh::detect()?.pull_requests_for_branches(repo_dir)
}

/// Checks out pull request `number` in `repo_dir`, using the `gh` on `PATH`.
pub fn checkout_pull_request(repo_dir: &Path, number: u64) -> Result<()> {
    Gh::detect()?.checkout_pull_request(repo_dir, number)
}

/// The same, with a `gh` the app already found.
pub fn pull_requests_for_branches_with(
    binary: &Path,
    repo_dir: &Path,
) -> Result<Vec<PullRequestSummary>> {
    Gh::new(binary).pull_requests_for_branches(repo_dir)
}

/// The same, with a `gh` the app already found.
pub fn checkout_pull_request_with(binary: &Path, repo_dir: &Path, number: u64) -> Result<()> {
    Gh::new(binary).checkout_pull_request(repo_dir, number)
}
