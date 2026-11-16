//! What a remote URL says about the service hosting the repository.
//!
//! The app needs two host-neutral answers from a remote: which service it is
//! (for the menu label and the CLI that can answer pull-request questions),
//! and how to link a commit, a file, a branch or a pull request on that
//! service's web pages. Both are pure functions of the URL, so every spelling
//! a clone URL takes is covered by a test instead of a network call.
//!
//! GitHub Enterprise hosts are only recognised when `gh` reported an account
//! for them: a self-hosted server that speaks a similar API would otherwise
//! turn every link into a broken one.

use crate::tools::{GitHubRemote, encode, percent_decode, split_remote};

/// The hosting services the app knows how to link to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForgeKind {
    GitHub,
    AzureDevOps,
    Bitbucket,
}

/// A repository on one of the supported hosting services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeRemote {
    GitHub(GitHubRemote),
    AzureDevOps(AzureDevOpsRemote),
    Bitbucket(BitbucketRemote),
}

/// A repository on Azure DevOps, in the spelling its web pages use.
///
/// The clone URL and the web URL disagree on purpose: SSH clones live under
/// `ssh.dev.azure.com/v3/...`, while every page lives under
/// `dev.azure.com/{org}/{project}/_git/{repo}`. Parsing normalises both into
/// this shape so a link never depends on how the repository was cloned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureDevOpsRemote {
    /// The web host: `dev.azure.com`, or `{org}.visualstudio.com` for the
    /// legacy service.
    pub host: String,
    pub organization: String,
    pub project: String,
    pub repo: String,
}

/// A repository on Bitbucket Cloud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketRemote {
    pub workspace: String,
    pub repo: String,
}

impl ForgeRemote {
    /// Parses a remote URL, accepting a GitHub host when it is `github.com` or
    /// one `gh` reported an account for.
    pub fn parse(url: &str, known_hosts: &[String]) -> Option<Self> {
        if let Some(github) = GitHubRemote::parse(url, known_hosts) {
            return Some(Self::GitHub(github));
        }
        let (host, path) = split_remote(url)?;
        if host.eq_ignore_ascii_case("bitbucket.org") {
            return bitbucket_remote(&path).map(Self::Bitbucket);
        }
        azure_remote(&host, &path).map(Self::AzureDevOps)
    }

    pub fn kind(&self) -> ForgeKind {
        match self {
            Self::GitHub(_) => ForgeKind::GitHub,
            Self::AzureDevOps(_) => ForgeKind::AzureDevOps,
            Self::Bitbucket(_) => ForgeKind::Bitbucket,
        }
    }

    /// The host a CLI must be logged in to, when it reports accounts per host.
    /// Azure DevOps accounts are organization-agnostic, so its web host is
    /// informational only.
    pub fn host(&self) -> &str {
        match self {
            Self::GitHub(remote) => &remote.host,
            Self::AzureDevOps(remote) => &remote.host,
            Self::Bitbucket(remote) => remote.host(),
        }
    }

    /// The owner of the repository: a GitHub user or organization, the Bitbucket
    /// workspace, or the Azure DevOps organization.
    pub fn owner(&self) -> &str {
        match self {
            Self::GitHub(remote) => &remote.owner,
            Self::AzureDevOps(remote) => &remote.organization,
            Self::Bitbucket(remote) => &remote.workspace,
        }
    }

    pub fn repo(&self) -> &str {
        match self {
            Self::GitHub(remote) => &remote.repo,
            Self::AzureDevOps(remote) => &remote.repo,
            Self::Bitbucket(remote) => &remote.repo,
        }
    }

    /// The address every page of the repository extends.
    pub fn web_base(&self) -> String {
        match self {
            Self::GitHub(remote) => {
                format!("https://{}/{}/{}", remote.host, remote.owner, remote.repo)
            }
            Self::AzureDevOps(remote) => remote.web_base(),
            Self::Bitbucket(remote) => remote.web_base(),
        }
    }

    /// The page for one commit.
    pub fn commit_url(&self, commit: &str) -> String {
        match self {
            Self::GitHub(remote) => remote.commit_url(commit),
            Self::AzureDevOps(remote) => {
                format!("{}/commit/{}", remote.web_base(), encode(commit, false))
            }
            Self::Bitbucket(remote) => {
                format!("{}/commits/{}", remote.web_base(), encode(commit, false))
            }
        }
    }

    /// The page for a file at `revision`, with a line anchor where the host
    /// understands one. Azure DevOps ignores the anchor here and needs its
    /// query-parameter form, which [`ForgeRemote::file_at_branch_url`] builds.
    pub fn file_url(&self, revision: &str, path: &str, lines: Option<(u32, u32)>) -> String {
        match self {
            Self::GitHub(remote) => remote.blob_url(revision, path, lines),
            Self::AzureDevOps(remote) => format!(
                "{}?path=/{}&version=GC{}",
                remote.web_base(),
                encode(path, true),
                encode(revision, false)
            ),
            Self::Bitbucket(remote) => {
                let mut url = format!(
                    "{}/src/{}/{}",
                    remote.web_base(),
                    encode(revision, false),
                    encode(path, true)
                );
                if let Some((start, _)) = lines.filter(|(start, _)| *start > 0) {
                    // Bitbucket anchors a whole line, not a character range.
                    url.push_str(&format!("#lines-{start}"));
                }
                url
            }
        }
    }

    /// The page for a file on a branch rather than at a commit.
    pub fn file_at_branch_url(&self, branch: &str, path: &str) -> String {
        match self {
            Self::GitHub(remote) => remote.blob_url(branch, path, None),
            Self::AzureDevOps(remote) => format!(
                "{}?path=/{}&version=GB{}",
                remote.web_base(),
                encode(path, true),
                encode(branch, false)
            ),
            Self::Bitbucket(remote) => format!(
                "{}/src/{}/{}",
                remote.web_base(),
                encode(branch, false),
                encode(path, true)
            ),
        }
    }

    /// The page for a branch.
    pub fn branch_url(&self, branch: &str) -> String {
        match self {
            Self::GitHub(remote) => remote.tree_url(branch),
            Self::AzureDevOps(remote) => {
                format!("{}?version=GB{}", remote.web_base(), encode(branch, false))
            }
            Self::Bitbucket(remote) => {
                format!("{}/branch/{}", remote.web_base(), encode(branch, true))
            }
        }
    }

    /// The form that asks for a pull request from `branch`.
    ///
    /// `target` is the branch it should merge into. `None` leaves the choice
    /// to the host, which preselects the repository's default branch.
    pub fn create_pull_request_url(&self, branch: &str, target: Option<&str>) -> String {
        match self {
            Self::GitHub(remote) => remote.create_pull_request_url(branch, target),
            Self::AzureDevOps(remote) => {
                let mut url = format!(
                    "{}/pullrequestcreate?sourceRef={}",
                    remote.web_base(),
                    encode(branch, false)
                );
                if let Some(target) = target {
                    url.push_str(&format!("&targetRef={}", encode(target, false)));
                }
                url
            }
            Self::Bitbucket(remote) => {
                let mut url = format!(
                    "{}/pull-requests/new?source={}",
                    remote.web_base(),
                    encode(branch, false)
                );
                if let Some(target) = target {
                    url.push_str(&format!("&dest={}", encode(target, false)));
                }
                url
            }
        }
    }

    /// The page for one pull request.
    pub fn pull_request_url(&self, number: u64) -> String {
        match self {
            Self::GitHub(remote) => remote.pull_request_url(number),
            Self::AzureDevOps(remote) => format!("{}/pullrequest/{number}", remote.web_base()),
            Self::Bitbucket(remote) => format!("{}/pull-requests/{number}", remote.web_base()),
        }
    }
}

impl AzureDevOpsRemote {
    /// The organisation URL `az --organization` takes.
    ///
    /// Legacy organizations are addressed by their own host; the modern
    /// service keeps every organization under `dev.azure.com`.
    pub fn organization_url(&self) -> String {
        if self.host.eq_ignore_ascii_case("dev.azure.com") {
            format!(
                "https://dev.azure.com/{}",
                encode(&self.organization, false)
            )
        } else {
            format!("https://{}", self.host)
        }
    }

    pub fn web_base(&self) -> String {
        if self.host.eq_ignore_ascii_case("dev.azure.com") {
            format!(
                "https://dev.azure.com/{}/{}/_git/{}",
                encode(&self.organization, false),
                encode(&self.project, true),
                encode(&self.repo, true)
            )
        } else {
            format!(
                "https://{}/{}/_git/{}",
                self.host,
                encode(&self.project, true),
                encode(&self.repo, true)
            )
        }
    }
}

impl BitbucketRemote {
    pub fn host(&self) -> &str {
        "bitbucket.org"
    }

    pub fn web_base(&self) -> String {
        format!(
            "https://bitbucket.org/{}/{}",
            encode(&self.workspace, true),
            encode(&self.repo, true)
        )
    }
}

/// Reads the web spelling out of an Azure clone URL.
fn azure_remote(host: &str, path: &str) -> Option<AzureDevOpsRemote> {
    let host = host.to_ascii_lowercase();
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();

    if host == "ssh.dev.azure.com" || host == "vs-ssh.visualstudio.com" {
        let [version, organization, project, repo] = segments.as_slice() else {
            return None;
        };
        if !version.eq_ignore_ascii_case("v3") {
            return None;
        }
        let organization = percent_decode(organization);
        let web_host = if host == "ssh.dev.azure.com" {
            "dev.azure.com".to_owned()
        } else {
            format!("{organization}.visualstudio.com")
        };
        return Some(AzureDevOpsRemote {
            host: web_host,
            organization,
            project: percent_decode(project),
            repo: percent_decode(repo),
        });
    }

    if let Some(organization) = host.strip_suffix(".visualstudio.com") {
        // The legacy service accepts an optional collection segment before
        // the project; web pages never include it.
        let segments: Vec<&str> = segments
            .into_iter()
            .filter(|part| !part.eq_ignore_ascii_case("DefaultCollection"))
            .collect();
        let [project, marker, repo] = segments.as_slice() else {
            return None;
        };
        if !marker.eq_ignore_ascii_case("_git") {
            return None;
        }
        return Some(AzureDevOpsRemote {
            host: format!("{organization}.visualstudio.com"),
            organization: organization.to_owned(),
            project: percent_decode(project),
            repo: percent_decode(repo),
        });
    }

    if host != "dev.azure.com" {
        return None;
    }
    let marker = segments
        .iter()
        .position(|part| part.eq_ignore_ascii_case("_git"))?;
    // `dev.azure.com/{org}/{project}/_git/{repo}`: at least an organization,
    // a project and one repository segment around the marker.
    if marker < 2 || marker + 2 != segments.len() {
        return None;
    }
    let project = percent_decode(&segments[1..marker].join("/"));
    Some(AzureDevOpsRemote {
        host,
        organization: percent_decode(segments[0]),
        project,
        repo: percent_decode(segments[marker + 1]),
    })
}

fn bitbucket_remote(path: &str) -> Option<BitbucketRemote> {
    let mut segments = path.split('/').filter(|part| !part.is_empty());
    let workspace = percent_decode(segments.next()?);
    let repo = percent_decode(segments.next()?.trim_end_matches(".git"));
    if segments.next().is_some() || workspace.is_empty() || repo.is_empty() {
        return None;
    }
    Some(BitbucketRemote { workspace, repo })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> ForgeRemote {
        ForgeRemote::parse(url, &[]).unwrap_or_else(|| panic!("{url} is a remote"))
    }

    #[test]
    fn github_stays_the_same_and_keeps_enterprise_hosts() {
        assert_eq!(
            parse("https://github.com/torvalds/linux.git"),
            ForgeRemote::GitHub(GitHubRemote {
                host: "github.com".into(),
                owner: "torvalds".into(),
                repo: "linux".into(),
            })
        );
        assert!(ForgeRemote::parse("https://github.example.com/me/repo", &[]).is_none());
        assert_eq!(
            ForgeRemote::parse(
                "git@github.example.com:me/repo.git",
                &["github.example.com".into()]
            )
            .map(|remote| remote.kind()),
            Some(ForgeKind::GitHub)
        );
    }

    #[test]
    fn azure_devops_https_clone_urls_name_the_web_repository() {
        for url in [
            "https://dev.azure.com/myorg/My%20Project/_git/my%20repo",
            "https://dev.azure.com/myorg/My Project/_git/my repo",
            "https://user@dev.azure.com/myorg/My%20Project/_git/my%20repo",
            "ssh://git@ssh.dev.azure.com/v3/myorg/My%20Project/my%20repo",
            "git@ssh.dev.azure.com:v3/myorg/My%20Project/my%20repo",
        ] {
            let remote = parse(url);
            assert_eq!(remote.kind(), ForgeKind::AzureDevOps, "{url}");
            assert_eq!(
                remote.web_base(),
                "https://dev.azure.com/myorg/My%20Project/_git/my%20repo",
                "{url}"
            );
            let ForgeRemote::AzureDevOps(azure) = remote else {
                unreachable!()
            };
            assert_eq!(azure.organization, "myorg");
            assert_eq!(azure.project, "My Project", "{url}");
            assert_eq!(azure.repo, "my repo", "{url}");
            assert_eq!(azure.organization_url(), "https://dev.azure.com/myorg");
        }
    }

    #[test]
    fn legacy_visual_studio_urls_keep_their_own_host() {
        for url in [
            "https://myorg.visualstudio.com/DefaultCollection/My%20Project/_git/my%20repo",
            "https://myorg.visualstudio.com/My%20Project/_git/my%20repo",
            "myorg@vs-ssh.visualstudio.com:v3/myorg/My%20Project/my%20repo",
            "ssh://myorg@vs-ssh.visualstudio.com/v3/myorg/My%20Project/my%20repo",
        ] {
            let remote = parse(url);
            let ForgeRemote::AzureDevOps(azure) = &remote else {
                panic!("{url} is Azure DevOps")
            };
            assert_eq!(azure.organization, "myorg", "{url}");
            assert_eq!(azure.project, "My Project", "{url}");
            assert_eq!(azure.repo, "my repo", "{url}");
            assert_eq!(
                remote.web_base(),
                "https://myorg.visualstudio.com/My%20Project/_git/my%20repo",
                "{url}"
            );
            assert_eq!(azure.organization_url(), "https://myorg.visualstudio.com");
        }
    }

    #[test]
    fn malformed_azure_urls_are_not_guessed() {
        for url in [
            "https://dev.azure.com/myorg/_git/repo",
            "https://dev.azure.com/myorg/project/_git/repo/extra",
            "https://dev.azure.com/myorg/project/repo",
            "git@ssh.dev.azure.com:v2/myorg/project/repo",
            "git@ssh.dev.azure.com:v3/myorg/project",
            "https://myorg.visualstudio.com/myproject/repo",
        ] {
            assert!(
                !matches!(
                    ForgeRemote::parse(url, &[]),
                    Some(ForgeRemote::AzureDevOps(_))
                ),
                "{url}"
            );
        }
    }

    #[test]
    fn bitbucket_cloud_clone_urls_parse_in_every_spelling() {
        for url in [
            "https://bitbucket.org/my-workspace/my-repo",
            "https://bitbucket.org/my-workspace/my-repo.git",
            "https://user@bitbucket.org/my-workspace/my-repo.git",
            "git@bitbucket.org:my-workspace/my-repo.git",
            "ssh://git@bitbucket.org/my-workspace/my-repo",
        ] {
            let remote = parse(url);
            assert_eq!(remote.kind(), ForgeKind::Bitbucket, "{url}");
            assert_eq!(remote.owner(), "my-workspace");
            assert_eq!(remote.repo(), "my-repo");
            assert_eq!(
                remote.web_base(),
                "https://bitbucket.org/my-workspace/my-repo"
            );
        }
        assert!(ForgeRemote::parse("https://bitbucket.org/only-workspace", &[]).is_none());
    }

    #[test]
    fn a_remote_with_no_known_host_is_not_a_forge_remote() {
        assert!(ForgeRemote::parse("https://gitlab.com/me/repo", &[]).is_none());
        assert!(ForgeRemote::parse("ssh://git@example.com/me/repo", &[]).is_none());
        assert!(ForgeRemote::parse("/tmp/repo", &[]).is_none());
    }

    #[test]
    fn github_links_keep_the_existing_paths() {
        let remote = parse("https://github.com/me/strata.git");
        assert_eq!(
            remote.commit_url("abcdef"),
            "https://github.com/me/strata/commit/abcdef"
        );
        assert_eq!(
            remote.file_url("abcdef", "src/a b.rs", Some((10, 20))),
            "https://github.com/me/strata/blob/abcdef/src/a%20b.rs#L10-L20"
        );
        assert_eq!(
            remote.branch_url("feat/ui"),
            "https://github.com/me/strata/tree/feat/ui"
        );
        assert_eq!(
            remote.create_pull_request_url("feat/ui", Some("main")),
            "https://github.com/me/strata/compare/main...feat/ui?expand=1"
        );
        assert_eq!(
            remote.create_pull_request_url("feat/ui", None),
            "https://github.com/me/strata/compare/feat/ui?expand=1"
        );
        assert_eq!(
            remote.pull_request_url(12),
            "https://github.com/me/strata/pull/12"
        );
    }

    #[test]
    fn azure_devops_links_use_the_git_route() {
        let remote = parse("https://dev.azure.com/myorg/My Project/_git/my repo");
        assert_eq!(
            remote.commit_url("abcdef"),
            "https://dev.azure.com/myorg/My%20Project/_git/my%20repo/commit/abcdef"
        );
        assert_eq!(
            remote.file_url("abcdef", "src/main.rs", Some((10, 20))),
            "https://dev.azure.com/myorg/My%20Project/_git/my%20repo\
             ?path=/src/main.rs&version=GCabcdef"
        );
        assert_eq!(
            remote.file_at_branch_url("feat/ui", "src/main.rs"),
            "https://dev.azure.com/myorg/My%20Project/_git/my%20repo\
             ?path=/src/main.rs&version=GBfeat%2Fui"
        );
        assert_eq!(
            remote.branch_url("feat/ui"),
            "https://dev.azure.com/myorg/My%20Project/_git/my%20repo?version=GBfeat%2Fui"
        );
        assert_eq!(
            remote.create_pull_request_url("feat/ui", Some("main")),
            "https://dev.azure.com/myorg/My%20Project/_git/my%20repo\
             /pullrequestcreate?sourceRef=feat%2Fui&targetRef=main"
        );
        assert_eq!(
            remote.create_pull_request_url("feat/ui", None),
            "https://dev.azure.com/myorg/My%20Project/_git/my%20repo\
             /pullrequestcreate?sourceRef=feat%2Fui"
        );
        assert_eq!(
            remote.pull_request_url(7),
            "https://dev.azure.com/myorg/My%20Project/_git/my%20repo/pullrequest/7"
        );
    }

    #[test]
    fn azure_devops_ssh_clones_link_to_the_web_host() {
        let remote = parse("git@ssh.dev.azure.com:v3/myorg/My Project/my repo");
        assert_eq!(
            remote.commit_url("abcdef"),
            "https://dev.azure.com/myorg/My%20Project/_git/my%20repo/commit/abcdef"
        );
    }

    #[test]
    fn legacy_azure_links_stay_on_the_legacy_host() {
        let remote =
            parse("https://myorg.visualstudio.com/DefaultCollection/My%20Project/_git/my%20repo");
        assert_eq!(
            remote.pull_request_url(3),
            "https://myorg.visualstudio.com/My%20Project/_git/my%20repo/pullrequest/3"
        );
    }

    #[test]
    fn bitbucket_links_follow_its_web_routes() {
        let remote = parse("git@bitbucket.org:my-workspace/my-repo.git");
        assert_eq!(
            remote.commit_url("abcdef"),
            "https://bitbucket.org/my-workspace/my-repo/commits/abcdef"
        );
        assert_eq!(
            remote.file_url("abcdef", "src/a b.rs", Some((10, 20))),
            "https://bitbucket.org/my-workspace/my-repo/src/abcdef/src/a%20b.rs#lines-10"
        );
        assert_eq!(
            remote.file_url("abcdef", "src/lib.rs", Some((0, 0))),
            "https://bitbucket.org/my-workspace/my-repo/src/abcdef/src/lib.rs"
        );
        assert_eq!(
            remote.branch_url("feat/ui"),
            "https://bitbucket.org/my-workspace/my-repo/branch/feat/ui"
        );
        assert_eq!(
            remote.create_pull_request_url("feat/ui", Some("main")),
            "https://bitbucket.org/my-workspace/my-repo/pull-requests/new\
             ?source=feat%2Fui&dest=main"
        );
        assert_eq!(
            remote.pull_request_url(9),
            "https://bitbucket.org/my-workspace/my-repo/pull-requests/9"
        );
    }

    #[test]
    fn percent_escapes_round_trip_through_a_link() {
        let remote = parse("https://dev.azure.com/org/Proj%20A/_git/repo%23one");
        assert_eq!(remote.owner(), "org");
        assert_eq!(remote.repo(), "repo#one");
        assert!(remote.web_base().ends_with("/_git/repo%23one"));
    }
}
