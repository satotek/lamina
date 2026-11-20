//! Optional ghq and GitHub CLI integration.
//!
//! Both programs are optional: every function here is called only when the app
//! has already found the executable, and detection reports a plain error
//! instead of making the rest of Lamina depend on it.
//!
//! The commands are run directly, never through a shell, so paths and branch
//! names reach the programs exactly as they are. Nothing here reads a token:
//! `gh` keeps its own credentials.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::{Error, Result};

/// The hostname whose remotes are always treated as GitHub.
pub const GITHUB_HOST: &str = "github.com";

/// What a working `ghq` installation reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhqInfo {
    pub binary: PathBuf,
    /// `ghq --version`'s first line, when the program has that option.
    pub version: Option<String>,
    /// The effective `ghq root`, after root overrides.
    pub root: PathBuf,
}

/// What a working `gh` installation reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhInfo {
    pub binary: PathBuf,
    pub version: String,
    /// Hosts `gh auth status` reports an account for, in its own order.
    pub hosts: Vec<String>,
    pub logged_in: bool,
}

/// A repository on a GitHub-compatible host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRemote {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl GitHubRemote {
    /// Parses a remote URL, accepting a host when it is `github.com` or one
    /// `gh` reported an account for.
    ///
    /// Enterprise hosts are deliberately not guessed from the URL alone: a
    /// self-hosted Git server that happens to speak GitHub's API would
    /// otherwise turn every link into a broken one.
    pub fn parse(url: &str, known_hosts: &[String]) -> Option<Self> {
        let (host, path) = split_remote(url)?;
        if !host.eq_ignore_ascii_case(GITHUB_HOST)
            && !known_hosts
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&host))
        {
            return None;
        }

        let mut parts = path.split('/').filter(|part| !part.is_empty());
        let owner = parts.next()?;
        let repo = parts.next()?.trim_end_matches(".git");
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(Self {
            host,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        })
    }

    fn base_url(&self) -> String {
        format!("https://{}/{}/{}", self.host, self.owner, self.repo)
    }

    /// `https://host/owner/repo/commit/<sha>`.
    pub fn commit_url(&self, commit: &str) -> String {
        format!("{}/commit/{}", self.base_url(), encode(commit, false))
    }

    /// `https://host/owner/repo/tree/<branch>`.
    pub fn tree_url(&self, branch: &str) -> String {
        format!("{}/tree/{}", self.base_url(), encode(branch, true))
    }

    /// `https://host/owner/repo/blob/<sha>/<path>`, with `#L10-L20` for a
    /// range of lines. A single line gets `#L10`.
    pub fn blob_url(&self, commit: &str, path: &str, lines: Option<(u32, u32)>) -> String {
        let mut url = format!(
            "{}/blob/{}/{}",
            self.base_url(),
            encode(commit, false),
            encode(path, true)
        );
        if let Some((start, end)) = lines {
            if start == 0 {
                return url;
            }
            if end > start {
                let _ = write!(url, "#L{start}-L{end}");
            } else {
                let _ = write!(url, "#L{start}");
            }
        }
        url
    }
}

/// Finds `program` on `PATH`, without running it.
pub fn find_binary(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(format!("{program}{}", std::env::consts::EXE_SUFFIX)))
        .find(|candidate| candidate.is_file())
}

/// Runs `ghq root` (and its version) and reports the installation.
///
/// `root_override` makes the detection read that root instead of the one ghq
/// stores, matching how `ghq list` will later be asked for its repositories.
pub fn detect_ghq(root_override: Option<&Path>) -> Result<GhqInfo> {
    let binary = find_binary("ghq").ok_or_else(|| Error::other("ghq is not installed"))?;
    let version = run_tool(&binary, &["--version"], root_override, None)
        .ok()
        .and_then(|output| first_line(&output).map(str::to_owned));

    let root = match root_override {
        Some(root) => root.to_path_buf(),
        None => {
            let output = run_tool(&binary, &["root"], None, None)?;
            let text = String::from_utf8_lossy(&output.stdout);
            parse_ghq_roots(&text)
                .into_iter()
                .next()
                .ok_or_else(|| Error::other("ghq root printed no directory"))?
        }
    };
    Ok(GhqInfo {
        binary,
        version,
        root,
    })
}

/// Runs `gh --version` and `gh auth status`.
///
/// A logged-out `gh` is a successful detection with `logged_in == false`; only
/// a missing or unusable program is an error, so the settings page can tell
/// "not installed" from "run gh auth login".
pub fn detect_gh() -> Result<GhInfo> {
    detect_gh_with_proxy(&crate::proxy::ProxyPolicy::Inherit)
}

/// Like [`detect_gh`], applying `proxy` to `gh` child processes.
pub fn detect_gh_with_proxy(proxy: &crate::proxy::ProxyPolicy) -> Result<GhInfo> {
    let binary = find_binary("gh").ok_or_else(|| Error::other("gh is not installed"))?;
    let version_output = run_tool(&binary, &["--version"], None, None)?;
    let version = first_line(&version_output)
        .unwrap_or("gh")
        .trim()
        .to_owned();

    let mut command = command(&binary, &["auth", "status"], None, None);
    proxy.apply(&mut command);
    let auth = command
        .output()
        .map_err(|error| Error::other(format!("could not run `gh auth status`: {error}")))?;
    let mut text = String::from_utf8_lossy(&auth.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&auth.stderr));
    let hosts = parse_gh_auth_hosts(&text);
    Ok(GhInfo {
        binary,
        version,
        logged_in: auth.status.success() && !hosts.is_empty(),
        hosts,
    })
}

/// `ghq list -p`, one repository root per line.
pub fn ghq_list(binary: &Path, root_override: Option<&Path>) -> Result<Vec<PathBuf>> {
    let output = run_tool(binary, &["list", "-p"], root_override, None)?;
    Ok(parse_ghq_list(&String::from_utf8_lossy(&output.stdout)))
}

/// Parses every non-empty line of `ghq root --all`-shaped output.
pub fn parse_ghq_roots(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Parses `ghq list -p` output.
pub fn parse_ghq_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The directory ghq would clone `url` into: `<root>/<host>/<owner>/<repo>`.
///
/// Handles `https://host/owner/repo`, `ssh://git@host:port/owner/repo` and the
/// scp-like `git@host:owner/repo`. A URL ghq would not place under a host (a
/// local path, a malformed authority) returns `None`, leaving the dialog's own
/// default alone.
pub fn ghq_destination(root: &Path, url: &str) -> Option<PathBuf> {
    let (host, path) = split_remote(url)?;
    if host.is_empty() {
        return None;
    }
    let mut destination = root.join(host);
    let mut parts = path.split('/').filter(|part| !part.is_empty()).peekable();
    let mut count = 0;
    while let Some(part) = parts.next() {
        if part == "." || part == ".." {
            return None;
        }
        let part = if parts.peek().is_none() {
            part.trim_end_matches(".git")
        } else {
            part
        };
        if part.is_empty() {
            return None;
        }
        destination.push(part);
        count += 1;
    }
    (count >= 2).then_some(destination)
}

/// Runs `gh pr create --web --head <branch>` in `workdir`.
///
/// `--web` opens the browser, so the app never handles a token or a form.
pub fn create_pull_request(gh: &Path, workdir: &Path, branch: &str) -> Result<()> {
    create_pull_request_with_proxy(gh, workdir, branch, &crate::proxy::ProxyPolicy::Inherit)
}

/// Like [`create_pull_request`], applying `proxy` to the `gh` child process.
pub fn create_pull_request_with_proxy(
    gh: &Path,
    workdir: &Path,
    branch: &str,
    proxy: &crate::proxy::ProxyPolicy,
) -> Result<()> {
    let mut command = command(gh, &["pr", "create", "--web", "--head", branch], None, None);
    command.current_dir(workdir);
    proxy.apply(&mut command);
    let output = command
        .output()
        .map_err(|error| Error::other(format!("could not run `gh pr create`: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        Err(Error::other(format!(
            "gh pr create failed: {}",
            detail.trim()
        )))
    }
}

fn run_tool(
    binary: &Path,
    args: &[&str],
    root_override: Option<&Path>,
    workdir: Option<&Path>,
) -> Result<Output> {
    let output = command(binary, args, root_override, workdir)
        .output()
        .map_err(|error| {
            Error::other(format!(
                "could not run {}: {error}",
                binary.to_string_lossy()
            ))
        })?;
    if output.status.success() {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        Err(Error::other(format!(
            "{} failed: {}",
            binary.to_string_lossy(),
            detail.trim()
        )))
    }
}

fn command(
    binary: &Path,
    args: &[&str],
    root_override: Option<&Path>,
    workdir: Option<&Path>,
) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(root) = root_override {
        command.env("GHQ_ROOT", root);
    }
    if let Some(workdir) = workdir {
        command.current_dir(workdir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // No console window flashes for a background check.
        command.creation_flags(0x0800_0000);
    }
    command
}

fn first_line(output: &Output) -> Option<&str> {
    std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|text| text.lines().next())
}

/// Hosts from `gh auth status`, one per account block.
///
/// Newer gh prints `github.com` as a heading and `Logged in to github.com
/// account …` below it; older versions print only the sentence. Both are
/// accepted, and blank or decorative lines are ignored.
pub fn parse_gh_auth_hosts(text: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let candidate = if let Some((_, rest)) = trimmed.split_once("Logged in to ") {
            rest.split_whitespace().next()
        } else if !line.starts_with([' ', '\t']) {
            trimmed.split_whitespace().next()
        } else {
            None
        };
        if let Some(host) = candidate
            && looks_like_host(host)
            && !hosts.iter().any(|known| known.eq_ignore_ascii_case(host))
        {
            hosts.push(host.to_owned());
        }
    }
    hosts
}

fn looks_like_host(token: &str) -> bool {
    !token.is_empty() && token.contains('.') && !token.contains('/') && !token.starts_with('-')
}

/// Splits a remote into `(host, path)` without consulting git config.
///
/// Accepted forms are a URL with a scheme and the scp-like `user@host:path`.
/// Ports and credentials are dropped; a local path has no host and returns
/// `None`.
fn split_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return None;
    }

    let (authority, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            if !matches!(
                scheme.to_ascii_lowercase().as_str(),
                "http" | "https" | "ssh" | "git"
            ) {
                return None;
            }
            let (authority, path) = rest.split_once('/')?;
            (authority, path)
        }
        None => {
            let (authority, path) = url.split_once(':')?;
            // A Windows drive or a plain path has no user/host part.
            if !authority.contains('@') && !authority.contains('.') {
                return None;
            }
            (authority, path)
        }
    };

    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .trim_start_matches('[')
        .split([':', ']'])
        .next()
        .filter(|host| !host.is_empty())?;
    if path.is_empty() {
        return None;
    }
    Some((host.to_owned(), path.to_owned()))
}

/// Percent-encodes a URL path or segment, keeping `/` when `keep_slash`.
fn encode(value: &str, keep_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            b'/' if keep_slash => encoded.push('/'),
            _ => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/home/me/ghq")
    }

    #[test]
    fn ghq_root_and_list_parsing_keep_every_non_empty_line() {
        assert_eq!(
            parse_ghq_roots(" /home/me/ghq \n\n/mnt/work/ghq\r\n"),
            vec![
                PathBuf::from("/home/me/ghq"),
                PathBuf::from("/mnt/work/ghq")
            ],
            "multiple roots are preserved in ghq's order"
        );
        assert!(parse_ghq_roots(" \n\t\n").is_empty());

        let list = "/home/me/ghq/github.com/a/one\n/home/me/ghq/github.com/a/two\n\n";
        assert_eq!(
            parse_ghq_list(list),
            vec![
                PathBuf::from("/home/me/ghq/github.com/a/one"),
                PathBuf::from("/home/me/ghq/github.com/a/two"),
            ]
        );
        assert!(parse_ghq_list("").is_empty());
    }

    #[test]
    fn ghq_destinations_follow_ghq_layout_for_urls_it_understands() {
        for url in [
            "https://github.com/torvalds/linux",
            "https://github.com/torvalds/linux.git",
            "https://github.com/torvalds/linux.git/",
            "git@github.com:torvalds/linux.git",
            "ssh://git@github.com/torvalds/linux.git",
            "ssh://git@github.com:2222/torvalds/linux.git",
        ] {
            assert_eq!(
                ghq_destination(&root(), url),
                Some(root().join("github.com/torvalds/linux")),
                "{url}"
            );
        }
        assert_eq!(
            ghq_destination(&root(), "https://example.com/group/sub/repo.git"),
            Some(root().join("example.com/group/sub/repo")),
            "a nested owner path keeps every component"
        );
    }

    #[test]
    fn ghq_destinations_reject_paths_ghq_would_not_place_under_a_host() {
        assert_eq!(ghq_destination(&root(), ""), None);
        assert_eq!(ghq_destination(&root(), "/tmp/repo.git"), None);
        assert_eq!(ghq_destination(&root(), "https://example.com/"), None);
        assert_eq!(
            ghq_destination(&root(), "https://example.com/org/../repo"),
            None
        );
        assert_eq!(ghq_destination(&root(), "file:///tmp/repo.git"), None);
    }

    #[test]
    fn github_remotes_accept_github_and_known_enterprise_hosts() {
        let known = vec!["github.example.com".to_owned()];
        let https = GitHubRemote::parse("https://github.com/torvalds/linux.git", &[]).unwrap();
        assert_eq!(
            https,
            GitHubRemote {
                host: "github.com".into(),
                owner: "torvalds".into(),
                repo: "linux".into(),
            }
        );
        assert!(GitHubRemote::parse("git@github.com:torvalds/linux.git", &[]).is_some());
        assert!(
            GitHubRemote::parse("ssh://git@github.com:22/torvalds/linux.git", &[]).is_some(),
            "an ssh port is not part of the host"
        );

        assert!(
            GitHubRemote::parse("https://github.example.com/team/repo.git", &[]).is_none(),
            "an enterprise host is not guessed from the URL"
        );
        assert!(GitHubRemote::parse("https://github.example.com/team/repo.git", &known).is_some());
        assert!(
            GitHubRemote::parse("https://gitlab.com/team/repo.git", &known).is_none(),
            "a non-GitHub host stays non-GitHub"
        );
    }

    #[test]
    fn github_urls_escape_paths_but_keep_separators() {
        let remote = GitHubRemote {
            host: "github.com".into(),
            owner: "me".into(),
            repo: "lamina".into(),
        };
        assert_eq!(
            remote.commit_url("0123456789abcdef"),
            "https://github.com/me/lamina/commit/0123456789abcdef"
        );
        assert_eq!(
            remote.tree_url("feat/ui"),
            "https://github.com/me/lamina/tree/feat/ui"
        );
        assert_eq!(
            remote.blob_url("abc123", "src/a b#c%.rs", None),
            "https://github.com/me/lamina/blob/abc123/src/a%20b%23c%25.rs"
        );
        assert_eq!(
            remote.blob_url("abc123", "src/lib.rs", Some((10, 20))),
            "https://github.com/me/lamina/blob/abc123/src/lib.rs#L10-L20"
        );
        assert_eq!(
            remote.blob_url("abc123", "src/lib.rs", Some((10, 10))),
            "https://github.com/me/lamina/blob/abc123/src/lib.rs#L10"
        );
        assert_eq!(
            remote.blob_url("abc123", "src/lib.rs", Some((0, 20))),
            "https://github.com/me/lamina/blob/abc123/src/lib.rs",
            "line zero is not a line"
        );
    }

    #[test]
    fn gh_auth_status_hosts_are_read_from_both_output_shapes() {
        let modern = "github.com\n  ✓ Logged in to github.com account octocat (keyring)\n";
        assert_eq!(parse_gh_auth_hosts(modern), vec!["github.com"]);

        let enterprise =
            "github.example.com\n  ✓ Logged in to github.example.com account me (keyring)\n";
        assert_eq!(parse_gh_auth_hosts(enterprise), vec!["github.example.com"]);

        let old = "Logged in to github.com as octocat\n";
        assert_eq!(parse_gh_auth_hosts(old), vec!["github.com"]);

        assert!(
            parse_gh_auth_hosts("You are not logged into any GitHub hosts. Run gh auth login\n")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_pull_request_applies_proxy_policy() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::TempDir::new().unwrap();
        let captured = temp.path().join("env.txt");
        let fake_gh = temp.path().join("fake-gh");
        std::fs::write(
            &fake_gh,
            format!(
                "#!/bin/sh\n\
                 printf 'HTTP_PROXY=%s\\nhttps_proxy=%s\\n' \
                 \"${{HTTP_PROXY-<unset>}}\" \"${{https_proxy-<unset>}}\" > {}\n",
                captured.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();

        let config = crate::proxy::ProxyConfig {
            endpoint: Some(crate::proxy::ProxyEndpoint {
                scheme: crate::proxy::ProxyScheme::Socks5,
                host: "127.0.0.1".into(),
                port: 1080,
                username: None,
                password: None,
            }),
            no_proxy: Vec::new(),
        };
        create_pull_request_with_proxy(
            &fake_gh,
            temp.path(),
            "feat/proxy",
            &crate::proxy::ProxyPolicy::Set(config),
        )
        .unwrap();
        let env_text = std::fs::read_to_string(&captured).unwrap();
        assert!(env_text.contains("HTTP_PROXY=socks5://127.0.0.1:1080\n"));
        assert!(env_text.contains("https_proxy=socks5://127.0.0.1:1080\n"));

        create_pull_request_with_proxy(
            &fake_gh,
            temp.path(),
            "feat/proxy",
            &crate::proxy::ProxyPolicy::Clear,
        )
        .unwrap();
        let env_text = std::fs::read_to_string(&captured).unwrap();
        assert!(env_text.contains("HTTP_PROXY=<unset>\n"));
        assert!(env_text.contains("https_proxy=<unset>\n"));
    }
}
