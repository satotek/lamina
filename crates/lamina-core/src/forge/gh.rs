//! The `gh` implementation of [`ForgeCli`](super::ForgeCli).
//!
//! One `gh pr list` call answers for every branch, and parsing is a pure
//! function of its JSON text so the mapping can be tested without a network,
//! a login or a repository. Commands run without a shell, never prompt, and
//! give up after [`TIMEOUT`] so a hung CLI cannot pin a background worker.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{
    ChecksState, ForgeCli, PullRequestChecks, PullRequestState, PullRequestSummary, ReviewState,
};
use crate::{Error, Result};

/// How long one `gh` invocation may take before it is killed.
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// The fields one list call asks for. Kept in one place so the argument list
/// and the parser cannot drift apart.
pub const LIST_FIELDS: &str = "number,title,state,isDraft,headRefName,headRepositoryOwner,\
baseRefName,url,reviewDecision,statusCheckRollup,updatedAt";

/// A limit high enough for a large repository's active branches, low enough
/// that one call stays quick.
const LIST_LIMIT: &str = "100";

/// The `gh` program the app found, bound to the repository it is asked about
/// through the methods of [`ForgeCli`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gh {
    binary: PathBuf,
    proxy: crate::proxy::ProxyPolicy,
}

impl Gh {
    pub fn new(binary: &Path) -> Self {
        Self {
            binary: binary.to_path_buf(),
            proxy: crate::proxy::ProxyPolicy::Inherit,
        }
    }

    /// Uses the `gh` on `PATH`.
    pub fn detect() -> Result<Self> {
        crate::tools::find_binary("gh")
            .map(|binary| Self::new(&binary))
            .ok_or_else(|| Error::other("gh is not installed"))
    }

    pub fn with_proxy(mut self, proxy: crate::proxy::ProxyPolicy) -> Self {
        self.proxy = proxy;
        self
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }
}

/// The arguments of the one list call. Public for the test that pins them.
pub fn list_arguments() -> [&'static str; 8] {
    [
        "pr",
        "list",
        "--state",
        "all",
        "--limit",
        LIST_LIMIT,
        "--json",
        LIST_FIELDS,
    ]
}

/// The arguments of a checkout. Numbered rather than free text, so a branch
/// name can never reach the CLI as an option.
pub fn checkout_arguments(number: u64) -> [String; 3] {
    ["pr".to_owned(), "checkout".to_owned(), number.to_string()]
}

impl ForgeCli for Gh {
    fn pull_requests_for_branches(&self, repo_dir: &Path) -> Result<Vec<PullRequestSummary>> {
        let args = list_arguments();
        let output = run(&self.binary, &args, repo_dir, &self.proxy)?;
        parse_pull_requests(&String::from_utf8_lossy(&output.stdout))
    }

    fn checkout_pull_request(&self, repo_dir: &Path, number: u64) -> Result<()> {
        let args = checkout_arguments(number);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&self.binary, &refs, repo_dir, &self.proxy).map(|_| ())
    }
}

/// Reads `gh pr list --json …` output into summaries.
///
/// Parsing is deliberately lenient about what gh omits: a missing rollup, a
/// deleted fork owner or an unknown check conclusion must leave the row
/// usable rather than fail the whole refresh.
pub fn parse_pull_requests(json: &str) -> Result<Vec<PullRequestSummary>> {
    let value: Value = serde_json::from_str(json)
        .map_err(|error| Error::other(format!("gh returned unexpected JSON: {error}")))?;
    let rows = value
        .as_array()
        .ok_or_else(|| Error::other("gh returned no list of pull requests"))?;

    rows.iter()
        .map(|row| {
            let number = row
                .get("number")
                .and_then(Value::as_u64)
                .ok_or_else(|| Error::other("a pull request has no number"))?;
            let draft = row.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
            Ok(PullRequestSummary {
                number,
                title: text(row, "title"),
                state: state_of(row, draft),
                head_branch: text(row, "headRefName"),
                head_owner: row
                    .get("headRepositoryOwner")
                    .and_then(|owner| owner.get("login"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                base: text(row, "baseRefName"),
                url: text(row, "url"),
                review: review_of(row.get("reviewDecision")),
                checks: checks_of(row.get("statusCheckRollup")),
                updated_at: text(row, "updatedAt"),
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

fn state_of(row: &Value, draft: bool) -> PullRequestState {
    match row.get("state").and_then(Value::as_str) {
        Some(state) if state.eq_ignore_ascii_case("MERGED") => PullRequestState::Merged,
        Some(state) if state.eq_ignore_ascii_case("CLOSED") => PullRequestState::Closed,
        _ if draft => PullRequestState::Draft,
        _ => PullRequestState::Open,
    }
}

fn review_of(decision: Option<&Value>) -> ReviewState {
    match decision.and_then(Value::as_str) {
        Some(decision) if decision.eq_ignore_ascii_case("APPROVED") => ReviewState::Approved,
        Some(decision) if decision.eq_ignore_ascii_case("CHANGES_REQUESTED") => {
            ReviewState::ChangesRequested
        }
        Some(decision) if decision.eq_ignore_ascii_case("REVIEW_REQUIRED") => {
            ReviewState::ReviewRequired
        }
        _ => ReviewState::None,
    }
}

/// One check's outcome, before the counts are summed.
#[derive(Clone, Copy)]
enum Outcome {
    Passing,
    Failing,
    Pending,
}

/// Sums every entry of `statusCheckRollup`.
///
/// A `statusCheckRollup` is a union: commit checks are `CheckRun` objects with
/// a `status` and a `conclusion`, while commit statuses are `StatusContext`
/// objects with a single `state`. The shape is read from `__typename` when the
/// host sends it and from the fields otherwise, so both old and new gh builds
/// map the same way.
fn checks_of(rollup: Option<&Value>) -> PullRequestChecks {
    let Some(checks) = rollup.and_then(Value::as_array) else {
        return PullRequestChecks::default();
    };
    let mut counts = PullRequestChecks::default();
    for check in checks.iter().filter(|check| !check.is_null()) {
        match outcome_of(check) {
            Outcome::Passing => counts.passing += 1,
            Outcome::Failing => counts.failing += 1,
            Outcome::Pending => counts.pending += 1,
        }
    }
    counts.state = if counts.total() == 0 {
        ChecksState::None
    } else if counts.failing > 0 {
        ChecksState::Failing
    } else if counts.pending > 0 {
        ChecksState::Pending
    } else {
        ChecksState::Passing
    };
    counts
}

fn outcome_of(check: &Value) -> Outcome {
    let typename = check.get("__typename").and_then(Value::as_str);
    match typename {
        Some("CheckRun") => check_run_outcome(check),
        Some("StatusContext") => status_context_outcome(check),
        // gh omits `__typename` in some versions; the field names identify the
        // shape just as well.
        _ if check.get("conclusion").is_some() || check.get("status").is_some() => {
            check_run_outcome(check)
        }
        _ => status_context_outcome(check),
    }
}

fn check_run_outcome(check: &Value) -> Outcome {
    let status = check.get("status").and_then(Value::as_str).unwrap_or("");
    let conclusion = check
        .get("conclusion")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !status.eq_ignore_ascii_case("COMPLETED") {
        // An absent status is an older payload that only carried the verdict.
        if status.is_empty() && !conclusion.is_empty() {
            return conclusion_outcome(conclusion);
        }
        return Outcome::Pending;
    }
    conclusion_outcome(conclusion)
}

fn conclusion_outcome(conclusion: &str) -> Outcome {
    match conclusion.to_ascii_uppercase().as_str() {
        "SUCCESS" | "NEUTRAL" | "SKIPPED" => Outcome::Passing,
        "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" | "STALE" => {
            Outcome::Failing
        }
        // A completed run without a verdict, or a verdict this build does not
        // know, is not evidence that the pull request is green.
        _ => Outcome::Pending,
    }
}

fn status_context_outcome(check: &Value) -> Outcome {
    match check
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase()
        .as_str()
    {
        "SUCCESS" => Outcome::Passing,
        "FAILURE" | "ERROR" => Outcome::Failing,
        _ => Outcome::Pending,
    }
}

/// Runs one `gh` command in `workdir`, with stdin closed and a hard timeout.
fn run(
    binary: &Path,
    args: &[&str],
    workdir: &Path,
    proxy: &crate::proxy::ProxyPolicy,
) -> Result<Output> {
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // gh must never wait for a terminal answer: it is started from a
        // window with no terminal attached.
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    proxy.apply(&mut command);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // No console window flashes for a background call.
        command.creation_flags(0x0800_0000);
    }

    let child = command
        .spawn()
        .map_err(|error| Error::other(format!("could not run `gh`: {error}")))?;
    let output = wait_with_timeout(child, TIMEOUT)?;
    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Err(Error::other(format!(
        "gh {} failed: {}",
        args.get(1).copied().unwrap_or("command"),
        short_reason(&detail)
    )))
}

/// Waits for `child`, killing it after `timeout`.
///
/// The pipes are drained on their own threads: a large list can fill a pipe
/// buffer, and a child blocked writing into it would never reach its exit
/// status.
fn wait_with_timeout(mut child: Child, timeout: Duration) -> Result<Output> {
    let stdout = child.stdout.take().map(read_in_background);
    let stderr = child.stderr.take().map(read_in_background);
    let deadline = Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout.map(Join::join);
                let _ = stderr.map(Join::join);
                return Err(Error::other(format!(
                    "gh did not answer within {} seconds",
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(Error::other(format!("could not wait for gh: {error}"))),
        }
    };

    Ok(Output {
        status,
        stdout: stdout.map(Join::join).unwrap_or_default(),
        stderr: stderr.map(Join::join).unwrap_or_default(),
    })
}

/// A reader thread plus its handle, kept together so the caller cannot join
/// before taking the bytes.
struct Join(std::thread::JoinHandle<Vec<u8>>);

impl Join {
    fn join(self) -> Vec<u8> {
        self.0.join().unwrap_or_default()
    }
}

fn read_in_background<R: Read + Send + 'static>(mut stream: R) -> Join {
    Join(std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        bytes
    }))
}

/// The first line of a command's complaint, which is the one that names the
/// problem. Long lines are cut so the status bar stays readable.
fn short_reason(detail: &str) -> String {
    let line = detail
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output");
    let mut reason: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        reason.push('…');
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"[
      {
        "number": 12,
        "title": "Add the forge layer",
        "state": "OPEN",
        "isDraft": false,
        "headRefName": "feat/forge",
        "headRepositoryOwner": {"id": "MDQ6VXNlcjE=", "login": "me"},
        "baseRefName": "main",
        "url": "https://github.com/me/lamina/pull/12",
        "reviewDecision": "APPROVED",
        "statusCheckRollup": [
          {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS"},
          {"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "FAILURE"},
          {"__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "NEUTRAL"},
          {"__typename": "CheckRun", "name": "docs", "status": "COMPLETED", "conclusion": "SKIPPED"},
          {"__typename": "StatusContext", "context": "ci/license", "state": "SUCCESS"},
          {"__typename": "StatusContext", "context": "deploy/preview", "state": "PENDING"},
          {"__typename": "CheckRun", "name": "e2e", "status": "IN_PROGRESS", "conclusion": null}
        ],
        "updatedAt": "2026-01-04T10:00:00Z"
      },
      {
        "number": 7,
        "title": "Work in progress",
        "state": "OPEN",
        "isDraft": true,
        "headRefName": "wip",
        "headRepositoryOwner": {"login": "me"},
        "baseRefName": "main",
        "url": "https://github.com/me/lamina/pull/7",
        "reviewDecision": null,
        "statusCheckRollup": null,
        "updatedAt": "2026-01-03T10:00:00Z"
      },
      {
        "number": 5,
        "title": "Already merged",
        "state": "MERGED",
        "isDraft": false,
        "headRefName": "old",
        "headRepositoryOwner": {"login": "me"},
        "baseRefName": "main",
        "url": "https://github.com/me/lamina/pull/5",
        "reviewDecision": "APPROVED",
        "statusCheckRollup": [],
        "updatedAt": "2026-01-02T10:00:00Z"
      },
      {
        "number": 3,
        "title": "Abandoned",
        "state": "CLOSED",
        "isDraft": false,
        "headRefName": "closed-branch",
        "headRepositoryOwner": {"login": "me"},
        "baseRefName": "main",
        "url": "https://github.com/me/lamina/pull/3",
        "reviewDecision": "CHANGES_REQUESTED",
        "statusCheckRollup": [
          {"__typename": "StatusContext", "context": "ci/build", "state": "ERROR"}
        ],
        "updatedAt": "2026-01-01T10:00:00Z"
      },
      {
        "number": 9,
        "title": "From a fork",
        "state": "OPEN",
        "isDraft": false,
        "headRefName": "fix-fork",
        "headRepositoryOwner": {"login": "contributor"},
        "baseRefName": "main",
        "url": "https://github.com/me/lamina/pull/9",
        "reviewDecision": "REVIEW_REQUIRED",
        "statusCheckRollup": [
          {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS"}
        ],
        "updatedAt": "2026-01-05T10:00:00Z"
      }
    ]"#;

    #[test]
    fn the_fixture_covers_every_state_review_and_check_shape() {
        let prs = parse_pull_requests(FIXTURE).expect("the fixture parses");
        assert_eq!(prs.len(), 5);

        let open = &prs[0];
        assert_eq!(open.number, 12);
        assert_eq!(open.title, "Add the forge layer");
        assert_eq!(open.state, PullRequestState::Open);
        assert_eq!(open.head_branch, "feat/forge");
        assert_eq!(open.head_owner.as_deref(), Some("me"));
        assert_eq!(open.base, "main");
        assert_eq!(open.url, "https://github.com/me/lamina/pull/12");
        assert_eq!(open.review, ReviewState::Approved);
        assert_eq!(
            open.checks,
            PullRequestChecks {
                state: ChecksState::Failing,
                passing: 4,
                failing: 1,
                pending: 2,
            },
            "SUCCESS, NEUTRAL, SKIPPED and a green StatusContext pass; \
             FAILURE fails; PENDING and an unfinished CheckRun wait"
        );
        assert_eq!(open.checks.total(), 7);

        let draft = &prs[1];
        assert_eq!(draft.state, PullRequestState::Draft);
        assert_eq!(draft.review, ReviewState::None);
        assert_eq!(draft.checks, PullRequestChecks::default());
        assert_eq!(draft.checks.state, ChecksState::None);

        let merged = &prs[2];
        assert_eq!(merged.state, PullRequestState::Merged);
        assert_eq!(merged.checks.state, ChecksState::None, "an empty rollup");

        let closed = &prs[3];
        assert_eq!(closed.state, PullRequestState::Closed);
        assert_eq!(closed.review, ReviewState::ChangesRequested);
        assert_eq!(closed.checks.state, ChecksState::Failing);

        let fork = &prs[4];
        assert_eq!(fork.state, PullRequestState::Open);
        assert_eq!(fork.head_owner.as_deref(), Some("contributor"));
        assert_eq!(fork.review, ReviewState::ReviewRequired);
        assert_eq!(fork.checks.state, ChecksState::Passing);
        assert_eq!(fork.checks.passing, 1);
        assert_eq!(fork.updated_at, "2026-01-05T10:00:00Z");
    }

    #[test]
    fn a_missing_owner_and_missing_fields_leave_a_usable_row() {
        let prs = parse_pull_requests(
            r#"[{"number": 1, "state": "OPEN", "isDraft": false, "headRefName": "one"}]"#,
        )
        .expect("a sparse row still parses");
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].head_owner, None);
        assert_eq!(prs[0].title, "");
        assert_eq!(prs[0].review, ReviewState::None);
        assert_eq!(prs[0].checks.state, ChecksState::None);
    }

    #[test]
    fn check_shapes_without_a_typename_are_read_from_their_fields() {
        let prs = parse_pull_requests(
            r#"[{
                "number": 2,
                "state": "OPEN",
                "isDraft": false,
                "headRefName": "two",
                "statusCheckRollup": [
                    {"status": "COMPLETED", "conclusion": "SUCCESS"},
                    {"context": "ci", "state": "FAILURE"},
                    {"status": "QUEUED", "conclusion": null}
                ]
            }]"#,
        )
        .expect("the fixture parses");
        let checks = prs[0].checks;
        assert_eq!(checks.state, ChecksState::Failing);
        assert_eq!((checks.passing, checks.failing, checks.pending), (1, 1, 1));
    }

    #[test]
    fn neutral_and_skipped_checks_alone_count_as_passing() {
        let prs = parse_pull_requests(
            r#"[{
                "number": 4,
                "state": "OPEN",
                "isDraft": false,
                "headRefName": "four",
                "statusCheckRollup": [
                    {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "NEUTRAL"},
                    {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SKIPPED"}
                ]
            }]"#,
        )
        .expect("the fixture parses");
        assert_eq!(prs[0].checks.state, ChecksState::Passing);
        assert_eq!(prs[0].checks.total(), 2);
    }

    #[test]
    fn completed_checks_without_a_verdict_wait_rather_than_fail() {
        let prs = parse_pull_requests(
            r#"[{
                "number": 6,
                "state": "OPEN",
                "isDraft": false,
                "headRefName": "six",
                "statusCheckRollup": [
                    {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": null}
                ]
            }]"#,
        )
        .expect("the fixture parses");
        assert_eq!(prs[0].checks.state, ChecksState::Pending);
    }

    #[test]
    fn malformed_json_is_a_short_error_instead_of_a_panic() {
        assert!(parse_pull_requests("not json").is_err());
        assert!(parse_pull_requests(r#"{"number": 1}"#).is_err());
        assert!(parse_pull_requests(r#"[{"title": "no number"}]"#).is_err());
    }

    #[test]
    fn the_list_call_asks_for_every_field_the_parser_reads() {
        let args = list_arguments();
        assert_eq!(
            args,
            [
                "pr",
                "list",
                "--state",
                "all",
                "--limit",
                "100",
                "--json",
                LIST_FIELDS
            ]
        );
        for field in [
            "number",
            "title",
            "state",
            "isDraft",
            "headRefName",
            "headRepositoryOwner",
            "baseRefName",
            "url",
            "reviewDecision",
            "statusCheckRollup",
            "updatedAt",
        ] {
            assert!(
                args[7].split(',').any(|wanted| wanted == field),
                "the list call must ask for {field}"
            );
        }
    }

    #[test]
    fn checkout_passes_the_number_as_an_argument() {
        assert_eq!(checkout_arguments(123), ["pr", "checkout", "123"]);
    }

    #[test]
    fn reasons_keep_one_short_line() {
        let error = "HTTP 403: Resource not accessible by integration\n\nTry gh auth login\n";
        assert_eq!(
            short_reason(error),
            "HTTP 403: Resource not accessible by integration"
        );
        assert_eq!(short_reason(" \n\t\n"), "no output");
        let long = "x".repeat(500);
        let reason = short_reason(&long);
        assert_eq!(reason.chars().count(), 201);
        assert!(reason.ends_with('…'));
    }

    /// Read-only smoke test against a real clone and a logged-in `gh`:
    ///
    /// `LAMINA_PR_SMOKE_REPO=/tmp/gh-pr-smoke cargo test -p lamina-core \
    ///   --lib smoke_pull_requests -- --ignored --nocapture`
    ///
    /// It is ignored by default because it needs the network and credentials.
    #[test]
    #[ignore = "needs a real gh login and a clone in LAMINA_PR_SMOKE_REPO"]
    fn smoke_pull_requests_from_a_clone() {
        let path = std::env::var_os("LAMINA_PR_SMOKE_REPO")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/gh-pr-smoke"));
        assert!(path.is_dir(), "{} is not a clone", path.display());
        let started = Instant::now();
        let prs = super::super::pull_requests_for_branches(&path).expect("gh pr list must answer");
        let elapsed = started.elapsed();
        eprintln!(
            "smoke: parsed {} pull requests from {} in {:?}",
            prs.len(),
            path.display(),
            elapsed
        );
        assert!(!prs.is_empty(), "the repository should have pull requests");
    }

    #[cfg(unix)]
    #[test]
    fn proxy_policy_applies_to_gh_child_process() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::TempDir::new().unwrap();
        let captured = temp.path().join("env.txt");
        let fake_gh = temp.path().join("fake-gh");
        std::fs::write(
            &fake_gh,
            format!(
                "#!/bin/sh\n\
                 printf 'HTTP_PROXY=%s\\nHTTPS_PROXY=%s\\nNO_PROXY=%s\\n' \
                 \"${{HTTP_PROXY-<unset>}}\" \"${{HTTPS_PROXY-<unset>}}\" \"${{NO_PROXY-<unset>}}\" > {}\n\
                 printf '[]'\n",
                captured.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();

        let config = crate::proxy::ProxyConfig {
            endpoint: Some(crate::proxy::ProxyEndpoint {
                scheme: crate::proxy::ProxyScheme::Http,
                host: "proxy.example".into(),
                port: 8080,
                username: None,
                password: None,
            }),
            no_proxy: vec!["localhost".into()],
        };
        let gh = Gh::new(&fake_gh).with_proxy(crate::proxy::ProxyPolicy::Set(config));
        let prs = gh.pull_requests_for_branches(temp.path()).unwrap();
        assert!(prs.is_empty());
        let env_text = std::fs::read_to_string(&captured).unwrap();
        assert!(env_text.contains("HTTP_PROXY=http://proxy.example:8080\n"));
        assert!(env_text.contains("HTTPS_PROXY=http://proxy.example:8080\n"));
        assert!(env_text.contains("NO_PROXY=localhost\n"));

        let gh = Gh::new(&fake_gh).with_proxy(crate::proxy::ProxyPolicy::Clear);
        gh.checkout_pull_request(temp.path(), 42).unwrap();
        let env_text = std::fs::read_to_string(&captured).unwrap();
        assert!(env_text.contains("HTTP_PROXY=<unset>\n"));
        assert!(env_text.contains("HTTPS_PROXY=<unset>\n"));
        assert!(env_text.contains("NO_PROXY=<unset>\n"));
    }
}
