//! The `az` implementation of [`ForgeCli`](super::ForgeCli).
//!
//! The Azure CLI reaches pull requests through the `azure-devops` extension.
//! One `az repos pr list` call answers for a page of pull requests, and the
//! mapping is a pure function of its JSON text, so the parser can be tested
//! without a network, a login or a repository. Commands run without a shell,
//! never prompt, and give up after [`TIMEOUT`].
//!
//! Checkout does not use `az repos pr checkout`: that command can report
//! success while its git step has failed, which leaves the branch unmoved.
//! Reading the pull request's source ref and fetching it explicitly keeps the
//! fetch and the switch visible to the same error handling as the rest of the
//! app, and an existing local branch is only switched to, never rewritten.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use serde_json::Value;

use super::cli;
use super::remote::AzureDevOpsRemote;
use super::{
    ChecksState, ForgeCli, PullRequestChecks, PullRequestState, PullRequestSummary, ReviewState,
};
use crate::{Error, Result};

/// How long one `az` metadata call may take before it is killed.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// How long a fetch of the pulled request's branch may take. Network transfers
/// are the one call here whose length is not bounded by the CLI's own work.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// How many pull requests one list page asks for.
const PAGE_SIZE: usize = 100;

/// How many pages a refresh follows before it reports what it has. Five pages
/// cover the active pull requests of any repository this app is likely to open,
/// while keeping one refresh to a handful of calls.
const MAX_PAGES: usize = 5;

/// The `az` program and the repository it is asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Az {
    binary: PathBuf,
    remote: AzureDevOpsRemote,
}

impl Az {
    pub fn new(binary: &Path, remote: AzureDevOpsRemote) -> Self {
        Self {
            binary: binary.to_path_buf(),
            remote,
        }
    }

    /// Uses the `az` on `PATH`.
    pub fn detect(remote: AzureDevOpsRemote) -> Result<Self> {
        crate::tools::find_az()
            .map(|binary| Self::new(&binary, remote))
            .ok_or_else(|| Error::other("az is not installed"))
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    pub fn remote(&self) -> &AzureDevOpsRemote {
        &self.remote
    }

    fn az(&self, args: &[&str], workdir: &Path, timeout: Duration) -> Result<Output> {
        cli::run(
            &self.binary,
            args,
            workdir,
            timeout,
            "az",
            &[
                // The CLI otherwise prints progress and telemetry notices,
                // which belong neither in a parsed body nor in a status line.
                ("AZURE_CORE_ONLY_SHOW_ERRORS", "true"),
                ("AZURE_CORE_COLLECT_TELEMETRY", "no"),
                ("AZURE_CORE_NO_COLOR", "true"),
                ("GIT_TERMINAL_PROMPT", "0"),
            ],
        )
    }

    fn git(&self, repo_dir: &Path, args: &[String], timeout: Duration) -> Result<Output> {
        let binary =
            crate::tools::find_binary("git").ok_or_else(|| Error::other("git is not installed"))?;
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        cli::run(
            &binary,
            &refs,
            repo_dir,
            timeout,
            "git",
            &[("GIT_TERMINAL_PROMPT", "0")],
        )
    }
}

/// The arguments of one list page.
pub fn list_arguments(remote: &AzureDevOpsRemote, skip: usize) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "repos".into(),
        "pr".into(),
        "list".into(),
        "--organization".into(),
        remote.organization_url(),
        "--project".into(),
        remote.project.clone(),
        "--repository".into(),
        remote.repo.clone(),
        "--status".into(),
        "active".into(),
        "--top".into(),
        PAGE_SIZE.to_string(),
        "--output".into(),
        "json".into(),
    ];
    if skip > 0 {
        args.push("--skip".into());
        args.push(skip.to_string());
    }
    args
}

/// The arguments of the one read that names a pull request's source branch.
pub fn show_arguments(remote: &AzureDevOpsRemote, number: u64) -> Vec<String> {
    vec![
        "repos".into(),
        "pr".into(),
        "show".into(),
        "--id".into(),
        number.to_string(),
        "--organization".into(),
        remote.organization_url(),
        "--project".into(),
        remote.project.clone(),
        "--output".into(),
        "json".into(),
    ]
}

/// The fetch that brings the source branch's tip into the remote-tracking ref.
///
/// The explicit refspec is what makes this work in a single-branch clone,
/// where the branch has no remote-tracking ref yet.
pub fn fetch_arguments(remote: &str, branch: &str) -> Vec<String> {
    vec![
        "fetch".to_owned(),
        "--no-tags".to_owned(),
        remote.to_owned(),
        format!("refs/heads/{branch}:refs/remotes/{remote}/{branch}"),
    ]
}

/// The switch that leaves the checked-out branch at the fetched tip.
///
/// An existing branch is only switched to: moving it here could drop commits
/// the user has not pushed, and the app can advance it afterwards.
pub fn switch_arguments(branch: &str, existing: bool, remote: &str) -> Vec<String> {
    if existing {
        vec!["switch".to_owned(), branch.to_owned()]
    } else {
        vec![
            "switch".to_owned(),
            "-c".to_owned(),
            branch.to_owned(),
            "--track".to_owned(),
            format!("{remote}/{branch}"),
        ]
    }
}

impl ForgeCli for Az {
    fn pull_requests_for_branches(&self, repo_dir: &Path) -> Result<Vec<PullRequestSummary>> {
        let mut items = Vec::new();
        for page in 0..MAX_PAGES {
            let args = list_arguments(&self.remote, page * PAGE_SIZE);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let output = self.az(&refs, repo_dir, TIMEOUT)?;
            let mut page_items =
                parse_pull_requests(&String::from_utf8_lossy(&output.stdout), &self.remote)?;
            let count = page_items.len();
            items.append(&mut page_items);
            if count < PAGE_SIZE {
                break;
            }
        }
        Ok(items)
    }

    fn checkout_pull_request(&self, repo_dir: &Path, number: u64) -> Result<()> {
        let args = show_arguments(&self.remote, number);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.az(&refs, repo_dir, TIMEOUT)?;
        let source = parse_source_ref(&String::from_utf8_lossy(&output.stdout))?;
        let branch = branch_from_ref(&source)?;
        if !valid_branch_name(branch) {
            return Err(Error::other(format!(
                "pull request {number} names an unusable branch: {branch}"
            )));
        }

        let remote = self.remote_name(repo_dir)?;
        let existing_args = [
            "rev-parse".to_owned(),
            "--verify".to_owned(),
            "--quiet".to_owned(),
            format!("refs/heads/{branch}"),
        ];
        let existing = self.local_branch_exists(repo_dir, &existing_args)?;

        let fetch = fetch_arguments(&remote, branch);
        self.git(repo_dir, &fetch, FETCH_TIMEOUT)?;
        let switch = switch_arguments(branch, existing, &remote);
        self.git(repo_dir, &switch, TIMEOUT)?;
        Ok(())
    }
}

impl Az {
    fn local_branch_exists(&self, repo_dir: &Path, args: &[String]) -> Result<bool> {
        let binary =
            crate::tools::find_binary("git").ok_or_else(|| Error::other("git is not installed"))?;
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        cli::succeeded(&binary, &refs, repo_dir, TIMEOUT, "git")
    }

    /// The remote a checkout fetches from: `origin` when it is configured,
    /// otherwise the first configured remote.
    fn remote_name(&self, repo_dir: &Path) -> Result<String> {
        let args = ["remote"];
        let output = self.git(repo_dir, &args.map(str::to_owned), TIMEOUT)?;
        let names: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if names.iter().any(|name| name == "origin") {
            return Ok("origin".to_owned());
        }
        names
            .into_iter()
            .next()
            .ok_or_else(|| Error::other("the repository has no remote to fetch from"))
    }
}

/// Reads `az repos pr list --output json` into summaries.
///
/// Fields the call does not send — a missing draft flag, a pull request with no
/// title — leave a usable row rather than failing the whole refresh. Azure's
/// list reports no review verdict or checks; those need per-pull-request calls
/// this stage does not make, so they stay empty.
pub fn parse_pull_requests(
    json: &str,
    remote: &AzureDevOpsRemote,
) -> Result<Vec<PullRequestSummary>> {
    let value: Value = serde_json::from_str(json)
        .map_err(|error| Error::other(format!("az returned unexpected JSON: {error}")))?;
    let rows = value
        .as_array()
        .ok_or_else(|| Error::other("az returned no list of pull requests"))?;

    rows.iter()
        .map(|row| {
            let number = row
                .get("pullRequestId")
                .and_then(Value::as_u64)
                .ok_or_else(|| Error::other("a pull request has no pullRequestId"))?;
            let draft = row.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
            Ok(PullRequestSummary {
                number,
                title: text(row, "title"),
                state: state_of(row.get("status"), draft),
                head_branch: branch_of(row.get("sourceRefName")),
                // Azure reports no head owner, and every branch of the
                // repository belongs to its organization, which is what the
                // branch map compares against.
                head_owner: Some(remote.organization.clone()),
                base: branch_of(row.get("targetRefName")),
                url: format!("{}/pullrequest/{number}", remote.web_base()),
                review: ReviewState::None,
                checks: PullRequestChecks {
                    state: ChecksState::None,
                    ..PullRequestChecks::default()
                },
                updated_at: text(row, "creationDate"),
            })
        })
        .collect()
}

fn text(row: &Value, field: &str) -> String {
    row.get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn state_of(status: Option<&Value>, draft: bool) -> PullRequestState {
    match status.and_then(Value::as_str) {
        Some(status) if status.eq_ignore_ascii_case("completed") => PullRequestState::Merged,
        Some(status) if status.eq_ignore_ascii_case("abandoned") => PullRequestState::Closed,
        _ if draft => PullRequestState::Draft,
        _ => PullRequestState::Open,
    }
}

/// `refs/heads/feature` -> `feature`; anything else keeps its ref spelling.
fn branch_of(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name).to_owned())
        .unwrap_or_default()
}

/// Reads the source ref from `az repos pr show --output json`.
pub fn parse_source_ref(json: &str) -> Result<String> {
    let value: Value = serde_json::from_str(json)
        .map_err(|error| Error::other(format!("az returned unexpected JSON: {error}")))?;
    value
        .get("sourceRefName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::other("the pull request names no source branch"))
}

/// The branch part of a full ref, accepting only branch refs.
pub fn branch_from_ref(reference: &str) -> Result<&str> {
    reference
        .strip_prefix("refs/heads/")
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| Error::other(format!("unexpected source ref: {reference}")))
}

/// Whether `name` can be passed to `git switch` as a branch.
///
/// The name comes from the host, so it must not be able to read as an option
/// or contain a sequence git itself rejects in a ref.
pub fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains(['~', '^', ':', '?', '*', '[', '\\'])
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote() -> AzureDevOpsRemote {
        AzureDevOpsRemote {
            host: "dev.azure.com".into(),
            organization: "myorg".into(),
            project: "My Project".into(),
            repo: "my repo".into(),
        }
    }

    const FIXTURE: &str = r#"[
      {
        "pullRequestId": 12,
        "title": "Add the forge layer",
        "status": "active",
        "isDraft": false,
        "sourceRefName": "refs/heads/feat/forge",
        "targetRefName": "refs/heads/main",
        "creationDate": "2026-01-04T10:00:00.000Z",
        "createdBy": {"displayName": "Me", "id": "guid", "descriptor": "aad.Me"},
        "repository": {"id": "repo-guid", "name": "my repo"},
        "lastMergeCommit": {"commitId": "abc"},
        "_links": {"web": {"href": "https://dev.azure.com/myorg/My%20Project/_git/my%20repo/pullrequest/12"}}
      },
      {
        "pullRequestId": 7,
        "title": "Work in progress",
        "status": "active",
        "isDraft": true,
        "sourceRefName": "refs/heads/wip",
        "targetRefName": "refs/heads/main",
        "creationDate": "2026-01-03T10:00:00.000Z"
      },
      {
        "pullRequestId": 5,
        "title": "Already merged",
        "status": "completed",
        "isDraft": false,
        "sourceRefName": "refs/heads/old",
        "targetRefName": "refs/heads/main",
        "creationDate": "2026-01-02T10:00:00.000Z"
      },
      {
        "pullRequestId": 3,
        "title": "Abandoned",
        "status": "abandoned",
        "sourceRefName": "refs/heads/closed-branch",
        "targetRefName": "refs/heads/main"
      },
      {
        "pullRequestId": 9,
        "title": "Sparse"
      }
    ]"#;

    #[test]
    fn the_fixture_covers_every_state() {
        let prs = parse_pull_requests(FIXTURE, &remote()).expect("the fixture parses");
        assert_eq!(prs.len(), 5);

        let open = &prs[0];
        assert_eq!(open.number, 12);
        assert_eq!(open.title, "Add the forge layer");
        assert_eq!(open.state, PullRequestState::Open);
        assert_eq!(open.head_branch, "feat/forge");
        assert_eq!(open.head_owner.as_deref(), Some("myorg"));
        assert_eq!(open.base, "main");
        assert_eq!(
            open.url,
            "https://dev.azure.com/myorg/My%20Project/_git/my%20repo/pullrequest/12"
        );
        assert_eq!(open.review, ReviewState::None);
        assert_eq!(open.checks.state, ChecksState::None);
        assert_eq!(open.updated_at, "2026-01-04T10:00:00.000Z");

        assert_eq!(prs[1].state, PullRequestState::Draft);
        assert_eq!(prs[2].state, PullRequestState::Merged);
        assert_eq!(prs[3].state, PullRequestState::Closed);
        assert_eq!(prs[3].updated_at, "", "no creation date is not an error");

        let sparse = &prs[4];
        assert_eq!(sparse.state, PullRequestState::Open);
        assert_eq!(sparse.head_branch, "");
        assert_eq!(sparse.base, "");
        assert_eq!(sparse.title, "Sparse");
    }

    #[test]
    fn malformed_json_is_a_short_error_instead_of_a_panic() {
        assert!(parse_pull_requests("not json", &remote()).is_err());
        assert!(parse_pull_requests(r#"{"pullRequestId": 1}"#, &remote()).is_err());
        assert!(parse_pull_requests(r#"[{"title": "no id"}]"#, &remote()).is_err());
    }

    #[test]
    fn the_list_call_names_the_organization_project_and_repository() {
        let args = list_arguments(&remote(), 0);
        assert_eq!(
            args,
            [
                "repos",
                "pr",
                "list",
                "--organization",
                "https://dev.azure.com/myorg",
                "--project",
                "My Project",
                "--repository",
                "my repo",
                "--status",
                "active",
                "--top",
                "100",
                "--output",
                "json",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--skip"));

        let second = list_arguments(&remote(), 100);
        assert_eq!(&second[second.len() - 2..], ["--skip", "100"]);
    }

    #[test]
    fn a_legacy_organization_is_addressed_by_its_own_host() {
        let legacy = AzureDevOpsRemote {
            host: "myorg.visualstudio.com".into(),
            organization: "myorg".into(),
            ..remote()
        };
        let args = list_arguments(&legacy, 0);
        let index = args
            .iter()
            .position(|arg| arg == "--organization")
            .expect("the call names an organization");
        assert_eq!(args[index + 1], "https://myorg.visualstudio.com");
    }

    #[test]
    fn the_show_call_names_the_number_and_the_repository() {
        let args = show_arguments(&remote(), 42);
        assert_eq!(
            args,
            [
                "repos",
                "pr",
                "show",
                "--id",
                "42",
                "--organization",
                "https://dev.azure.com/myorg",
                "--project",
                "My Project",
                "--output",
                "json",
            ]
        );
    }

    #[test]
    fn the_source_ref_is_read_from_the_show_output() {
        let json = r#"{"pullRequestId": 12, "sourceRefName": "refs/heads/feat/forge"}"#;
        assert_eq!(parse_source_ref(json).unwrap(), "refs/heads/feat/forge");
        assert_eq!(
            branch_from_ref("refs/heads/feat/forge").unwrap(),
            "feat/forge"
        );
        assert!(parse_source_ref(r#"{"pullRequestId": 12}"#).is_err());
        assert!(branch_from_ref("refs/tags/v1").is_err());
        assert!(branch_from_ref("refs/heads/").is_err());
    }

    #[test]
    fn checkout_fetches_the_source_ref_and_switches_to_its_branch() {
        assert_eq!(
            fetch_arguments("origin", "feat/forge"),
            [
                "fetch",
                "--no-tags",
                "origin",
                "refs/heads/feat/forge:refs/remotes/origin/feat/forge",
            ]
        );
        assert_eq!(
            switch_arguments("feat/forge", false, "origin"),
            ["switch", "-c", "feat/forge", "--track", "origin/feat/forge"]
        );
        assert_eq!(
            switch_arguments("feat/forge", true, "origin"),
            ["switch", "feat/forge"],
            "an existing branch is only switched to"
        );
    }

    #[test]
    fn branch_names_that_could_read_as_options_are_rejected() {
        for name in ["feat/ui", "release-1.2", "user/topic", "fix_1"] {
            assert!(valid_branch_name(name), "{name}");
        }
        for name in [
            "",
            "-b",
            "--force",
            "feat/",
            "topic..next",
            "a b",
            "x:y",
            "x^y",
            "x~y",
            "x?y",
            "x*y",
            "x[y",
            "x\\y",
            "x@{1}",
            "line\nbreak",
        ] {
            assert!(!valid_branch_name(name), "{name:?}");
        }
    }

    /// Read-only smoke test against a real clone and a signed-in `az`:
    ///
    /// `STRATA_AZ_PR_SMOKE_REPO=/tmp/az-pr-smoke cargo test -p strata-core \
    ///   --lib smoke_azure_pull_requests -- --ignored --nocapture`
    ///
    /// It is ignored by default because it needs the network and credentials,
    /// and because the organization, project and repository are environment
    /// specific.
    #[test]
    #[ignore = "needs a signed-in az and an Azure DevOps clone in STRATA_AZ_PR_SMOKE_REPO"]
    fn smoke_azure_pull_requests_from_a_clone() {
        let path = std::env::var_os("STRATA_AZ_PR_SMOKE_REPO")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/az-pr-smoke"));
        assert!(path.is_dir(), "{} is not a clone", path.display());
        let remote = remote();
        let az = Az::detect(remote).expect("az must be installed");
        let prs = az
            .pull_requests_for_branches(&path)
            .expect("az repos pr list must answer");
        eprintln!("smoke: parsed {} pull requests", prs.len());
    }
}
