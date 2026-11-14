//! Branch and remote management: renames, upstreams and remote configuration.
//!
//! These are the actions the sidebar's branch and remote menus offer. Every
//! name is checked before it reaches git so a name that would read as an option
//! cannot change what the command does; everything else is left to git so its
//! own message reaches the user.

use crate::session::RemoteBranch;
use crate::{Error, Result};

use super::{Git, checked_ref};

impl Git {
    /// `git branch -m <name> <new_name>`.
    ///
    /// With `rename_upstream`, the branch's remote branch is renamed to match:
    /// `new_name` is pushed to `upstream.remote`, the old remote branch is
    /// deleted, and the local branch's upstream is pointed at the new remote
    /// branch.
    ///
    /// The new name is checked with `git check-ref-format --branch`, which is
    /// git's own definition of a branch name, so a refusal here matches what
    /// `git branch` would say. `git branch -m` also works on the checked-out
    /// branch, which is why renaming the current branch needs no special case.
    pub fn rename_branch(
        &self,
        name: &str,
        new_name: &str,
        rename_upstream: Option<&RemoteBranch>,
    ) -> Result<()> {
        let name = checked_ref(name, "branch")?;
        let new_name = checked_ref(new_name, "branch")?;
        if name == new_name {
            return Err(Error::other("the branch already has that name"));
        }
        self.check_branch_name(new_name)?;
        self.run_unit(&owned(&["branch", "-m", "--", name, new_name]), None)?;

        let Some(upstream) = rename_upstream else {
            return Ok(());
        };
        let remote = checked_ref(&upstream.remote, "remote")?;
        let old_remote_branch = checked_ref(&upstream.branch, "remote branch")?;
        // Push first and delete second: if the delete fails, both branches
        // exist on the remote and nothing is lost. The reverse order would
        // leave the remote without the branch when the push then failed.
        self.push(Some(remote), Some(new_name), false, false)?;
        self.run_unit(
            &owned(&["push", "--progress", remote, "--delete", old_remote_branch]),
            None,
        )?;
        self.set_branch_upstream(new_name, remote, new_name)
    }

    /// `git branch --set-upstream-to=<remote>/<remote_branch> <branch>`.
    ///
    /// The remote is passed separately instead of parsed out of a tracking ref
    /// name because git accepts remote names that contain `/`.
    pub fn set_branch_upstream(
        &self,
        branch: &str,
        remote: &str,
        remote_branch: &str,
    ) -> Result<()> {
        let branch = checked_ref(branch, "branch")?;
        let remote = checked_ref(remote, "remote")?;
        let remote_branch = checked_ref(remote_branch, "remote branch")?;
        let tracking = format!("{remote}/{remote_branch}");
        self.run_unit(
            &owned(&[
                "branch",
                &format!("--set-upstream-to={tracking}"),
                "--",
                branch,
            ]),
            None,
        )
    }

    /// `git branch --unset-upstream <branch>`.
    pub fn unset_branch_upstream(&self, branch: &str) -> Result<()> {
        let branch = checked_ref(branch, "branch")?;
        self.run_unit(&owned(&["branch", "--unset-upstream", "--", branch]), None)
    }

    /// `git remote add <name> <url>`, then a fetch when `fetch` is set.
    ///
    /// A remote that was just added has no remote-tracking branches; fetching
    /// is what makes its branches appear in the sidebar.
    pub fn add_remote(&self, name: &str, url: &str, fetch: bool) -> Result<()> {
        let name = checked_ref(name, "remote")?;
        let url = checked_url(url)?;
        self.run_unit(&owned(&["remote", "add", "--", name, url]), None)?;
        if fetch {
            self.fetch(Some(name), false)?;
        }
        Ok(())
    }

    /// `git remote rename <name> <new_name>`.
    ///
    /// git moves the remote-tracking refs and rewrites every
    /// `branch.<name>.remote` that pointed at the old name, so a branch's
    /// upstream follows the rename.
    pub fn rename_remote(&self, name: &str, new_name: &str) -> Result<()> {
        let name = checked_ref(name, "remote")?;
        let new_name = checked_ref(new_name, "remote")?;
        if name == new_name {
            return Err(Error::other("the remote already has that name"));
        }
        self.run_unit(&owned(&["remote", "rename", "--", name, new_name]), None)
    }

    /// `git remote set-url` for the fetch URL and, when given, the push URL.
    ///
    /// `push_url` is `Some` only for a remote that already pushes somewhere
    /// other than it fetches; `None` leaves git's fetch-URL fallback alone.
    pub fn set_remote_url(
        &self,
        name: &str,
        fetch_url: &str,
        push_url: Option<&str>,
    ) -> Result<()> {
        let name = checked_ref(name, "remote")?;
        let fetch_url = checked_url(fetch_url)?;
        self.run_unit(&owned(&["remote", "set-url", "--", name, fetch_url]), None)?;
        if let Some(push_url) = push_url {
            let push_url = checked_url(push_url)?;
            self.run_unit(
                &owned(&["remote", "set-url", "--push", "--", name, push_url]),
                None,
            )?;
        }
        Ok(())
    }

    /// `git remote remove <name>`: also drops the remote's tracking branches
    /// and the upstreams that pointed at it.
    pub fn remove_remote(&self, name: &str) -> Result<()> {
        let name = checked_ref(name, "remote")?;
        self.run_unit(&owned(&["remote", "remove", "--", name]), None)
    }

    /// Run git's own branch-name validation, so a name the dialog accepts with
    /// its pure rule set is also one `git branch` cannot refuse.
    fn check_branch_name(&self, name: &str) -> Result<()> {
        self.run(&["check-ref-format", "--branch", name], None)
            .map(|_| ())
    }
}

/// The argument vector [`Git::run_unit`] expects for a static-and-borrowed list.
fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Reject URLs git would read as an option, and empty ones.
fn checked_url(url: &str) -> Result<&str> {
    if url.trim().is_empty() {
        return Err(Error::other("remote URL is empty"));
    }
    if url.starts_with('-') {
        return Err(Error::other(format!(
            "remote URL must not start with `-`: {url}"
        )));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::repo::UpstreamTarget;
    use crate::test_support::TestRepo;

    /// A bare repository the fixture can push into. It lives in its own
    /// temporary directory so `git add -A` in the worktree cannot see it.
    struct BareRemote {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl BareRemote {
        fn new(repo: &TestRepo, name: &str) -> Self {
            let dir = tempfile::TempDir::new().expect("remote directory");
            let path = dir.path().join(format!("{name}.git"));
            repo.git(&["init", "--bare", "-q", path.to_str().expect("utf-8 path")]);
            Self { _dir: dir, path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn url(&self) -> String {
            self.path.display().to_string()
        }

        /// Branch names the remote currently has, as `git ls-remote` reports.
        fn branches(&self, repo: &TestRepo) -> Vec<String> {
            repo.git(&[
                "ls-remote",
                "--heads",
                self.path.to_str().expect("utf-8 path"),
            ])
            .lines()
            .filter_map(|line| line.split_once("refs/heads/"))
            .map(|(_, branch)| branch.to_owned())
            .collect()
        }
    }

    /// A repository with one commit, a bare `origin` and `main` pushed with
    /// `-u`, which is the state a remote rename has to keep working.
    fn repo_with_origin() -> (TestRepo, BareRemote) {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("one");
        let origin = BareRemote::new(&repo, "origin");
        repo.git(&["remote", "add", "origin", &origin.url()]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);
        (repo, origin)
    }

    fn ref_exists(repo: &TestRepo, reference: &str) -> bool {
        repo.git_output(&["rev-parse", "--verify", "--quiet", reference])
            .status
            .success()
    }

    fn config(repo: &TestRepo, key: &str) -> Option<String> {
        let output = repo.git_output(&["config", "--get", key]);
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    #[test]
    fn a_branch_renames_without_touching_a_remote() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("one");
        let git = repo.git_handle();
        git.create_branch("topic", None, false)
            .expect("create topic");
        let tip = repo.rev("topic");

        git.rename_branch("topic", "renamed", None)
            .expect("rename topic");

        assert!(!ref_exists(&repo, "refs/heads/topic"));
        assert_eq!(repo.rev("renamed"), tip, "the rename moves the ref only");
        assert_eq!(config(&repo, "branch.renamed.remote"), None);
    }

    #[test]
    fn git_refuses_a_name_its_own_rule_set_rejects() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("one");
        let git = repo.git_handle();
        git.create_branch("topic", None, false)
            .expect("create topic");

        for bad in ["bad..name", "bad name", "topic.lock", "-dash"] {
            assert!(
                git.rename_branch("topic", bad, None).is_err(),
                "{bad} must be refused"
            );
            assert!(
                ref_exists(&repo, "refs/heads/topic"),
                "a refused rename leaves the branch alone"
            );
        }

        assert!(
            git.rename_branch("topic", "topic", None).is_err(),
            "renaming a branch to itself is not a rename"
        );
    }

    #[test]
    fn a_checked_out_branch_renames_and_stays_checked_out() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("one");
        let git = repo.git_handle();

        git.rename_branch("main", "trunk", None)
            .expect("rename the current branch");

        assert_eq!(
            repo.git(&["symbolic-ref", "--short", "HEAD"]).trim(),
            "trunk"
        );
        assert!(!ref_exists(&repo, "refs/heads/main"));
    }

    #[test]
    fn renaming_a_branch_renames_its_remote_branch_and_moves_the_upstream() {
        let (repo, origin) = repo_with_origin();
        let git = repo.git_handle();
        git.create_branch("topic", None, false)
            .expect("create topic");
        git.push(Some("origin"), Some("topic"), true, false)
            .expect("push topic");
        let target = repo
            .open()
            .branch_upstream_target("topic")
            .expect("read upstream")
            .expect("topic has an upstream");
        assert_eq!(
            target,
            UpstreamTarget {
                remote: "origin".into(),
                branch: "topic".into()
            }
        );

        git.rename_branch(
            "topic",
            "renamed",
            Some(&RemoteBranch {
                remote: target.remote.clone(),
                branch: target.branch.clone(),
            }),
        )
        .expect("rename topic and its remote branch");

        assert!(!ref_exists(&repo, "refs/heads/topic"));
        assert!(ref_exists(&repo, "refs/heads/renamed"));
        assert_eq!(
            origin.branches(&repo),
            ["main", "renamed"],
            "the old remote branch is gone and the new one is there"
        );
        assert_eq!(
            repo.git(&["rev-parse", "--abbrev-ref", "renamed@{upstream}"])
                .trim(),
            "origin/renamed"
        );
        assert_eq!(
            config(&repo, "branch.renamed.remote").as_deref(),
            Some("origin")
        );
        assert_eq!(
            config(&repo, "branch.renamed.merge").as_deref(),
            Some("refs/heads/renamed")
        );
        assert_eq!(
            config(&repo, "branch.topic.remote"),
            None,
            "the old branch's configuration goes with the rename"
        );
        assert_eq!(
            repo.open().upstreams().expect("read upstreams")["renamed"].name,
            "origin/renamed"
        );
    }

    #[test]
    fn set_and_unset_upstream_change_only_the_tracking_configuration() {
        let (repo, _origin) = repo_with_origin();
        let git = repo.git_handle();
        git.unset_branch_upstream("main").expect("unset upstream");
        assert_eq!(config(&repo, "branch.main.remote"), None);
        assert!(
            !repo
                .open()
                .upstreams()
                .expect("read upstreams")
                .contains_key("main")
        );

        git.set_branch_upstream("main", "origin", "main")
            .expect("set upstream");
        assert_eq!(
            config(&repo, "branch.main.merge").as_deref(),
            Some("refs/heads/main")
        );
        let upstream = repo.open().upstreams().expect("read upstreams");
        assert_eq!(upstream["main"].name, "origin/main");
        assert!(!upstream["main"].gone);
    }

    #[test]
    fn an_upstream_on_a_remote_whose_name_contains_a_slash_is_not_split() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("one");
        let remote = BareRemote::new(&repo, "team");
        let name = "team/one";
        repo.git(&["remote", "add", name, &remote.url()]);
        repo.git(&["push", "-q", name, "main"]);
        repo.git(&["fetch", "-q", name]);
        let git = repo.git_handle();

        git.set_branch_upstream("main", name, "main")
            .expect("set an upstream on a slashed remote");
        // The tracking ref does not exist yet, so the configuration is what
        // proves the remote was passed through whole.
        assert_eq!(
            config(&repo, "branch.main.remote").as_deref(),
            Some(name),
            "a remote name with a slash is one name, not a remote and a branch"
        );
        let target = repo
            .open()
            .branch_upstream_target("main")
            .expect("read target")
            .expect("configured upstream");
        assert_eq!(target.remote, name);
        assert_eq!(target.branch, "main");
    }

    #[test]
    fn a_remote_adds_renames_and_removes() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        repo.commit("one");
        let other = BareRemote::new(&repo, "other");
        repo.git(&[
            "push",
            "-q",
            other.path().to_str().expect("utf-8 path"),
            "main",
        ]);
        let git = repo.git_handle();

        git.add_remote("backup", &other.url(), false)
            .expect("add without fetch");
        let remotes = repo.open().remotes().expect("read remotes");
        let backup = remotes
            .iter()
            .find(|remote| remote.name == "backup")
            .expect("backup listed");
        assert_eq!(backup.fetch_url.as_deref(), Some(other.url().as_str()));

        git.add_remote("mirror", &other.url(), true)
            .expect("add and fetch");
        assert!(
            ref_exists(&repo, "refs/remotes/mirror/main"),
            "fetching a new remote creates its tracking branches"
        );

        git.rename_remote("backup", "archive")
            .expect("rename remote");
        let names: Vec<String> = repo
            .open()
            .remotes()
            .expect("read remotes")
            .into_iter()
            .map(|remote| remote.name)
            .collect();
        assert_eq!(names, ["archive", "mirror"]);
        assert_eq!(
            repo.git(&["remote", "get-url", "archive"]).trim(),
            other.url(),
            "the rename keeps the URL"
        );

        git.remove_remote("archive").expect("remove remote");
        let names: Vec<String> = repo
            .open()
            .remotes()
            .expect("read remotes")
            .into_iter()
            .map(|remote| remote.name)
            .collect();
        assert_eq!(names, ["mirror"]);
    }

    #[test]
    fn removing_a_remote_drops_its_tracking_branches() {
        let (repo, _origin) = repo_with_origin();
        assert!(ref_exists(&repo, "refs/remotes/origin/main"));
        let git = repo.git_handle();

        git.remove_remote("origin").expect("remove origin");

        // Prune may leave an explicitly-fetched ref behind only when the
        // remote still exists; here the remote and its refs go together.
        assert!(!ref_exists(&repo, "refs/remotes/origin/main"));
        assert_eq!(
            config(&repo, "branch.main.remote"),
            None,
            "the upstream that pointed at the removed remote goes too"
        );
    }

    #[test]
    fn renaming_a_remote_moves_the_upstream_of_a_branch_on_it() {
        let (repo, _origin) = repo_with_origin();
        assert_eq!(
            config(&repo, "branch.main.remote").as_deref(),
            Some("origin")
        );
        let git = repo.git_handle();

        git.rename_remote("origin", "backup")
            .expect("rename origin");

        assert_eq!(
            config(&repo, "branch.main.remote").as_deref(),
            Some("backup")
        );
        assert!(ref_exists(&repo, "refs/remotes/backup/main"));
        assert!(!ref_exists(&repo, "refs/remotes/origin/main"));
        assert_eq!(
            repo.open().branch_upstream_target("main").expect("read"),
            Some(UpstreamTarget {
                remote: "backup".into(),
                branch: "main".into()
            })
        );
        assert_eq!(
            repo.open().upstreams().expect("read upstreams")["main"].name,
            "backup/main"
        );
    }

    #[test]
    fn a_remote_url_can_be_replaced_with_and_without_a_push_url() {
        let repo = TestRepo::new();
        let git = repo.git_handle();
        git.add_remote("origin", "/tmp/old.git", false)
            .expect("add remote");

        git.set_remote_url("origin", "/tmp/fetch.git", Some("/tmp/push.git"))
            .expect("set both URLs");
        assert_eq!(
            repo.git(&["remote", "get-url", "origin"]).trim(),
            "/tmp/fetch.git"
        );
        assert_eq!(
            repo.git(&["remote", "get-url", "--push", "origin"]).trim(),
            "/tmp/push.git"
        );
        let remote = repo.open().remotes().expect("read remotes").remove(0);
        assert_eq!(remote.fetch_url.as_deref(), Some("/tmp/fetch.git"));
        assert_eq!(remote.push_url.as_deref(), Some("/tmp/push.git"));

        // Without a push URL the existing push URL stays: `git remote set-url`
        // without `--push` only changes the fetch URL. A push URL equal to the
        // fetch URL is what makes git fall back to it.
        git.set_remote_url("origin", "/tmp/fetch2.git", None)
            .expect("set only the fetch URL");
        assert_eq!(
            repo.git(&["remote", "get-url", "--push", "origin"]).trim(),
            "/tmp/push.git"
        );
        git.set_remote_url("origin", "/tmp/fetch2.git", Some("/tmp/fetch2.git"))
            .expect("point the push URL back at the fetch URL");
        assert_eq!(
            repo.git(&["remote", "get-url", "--push", "origin"]).trim(),
            "/tmp/fetch2.git"
        );
        let remote = repo.open().remotes().expect("read remotes").remove(0);
        assert_eq!(remote.fetch_url.as_deref(), Some("/tmp/fetch2.git"));
        assert_eq!(
            remote.push_url, None,
            "a push URL that matches the fetch URL is not a separate URL"
        );
    }

    #[test]
    fn empty_and_option_like_urls_are_refused_before_git_sees_them() {
        let repo = TestRepo::new();
        let git = repo.git_handle();
        git.add_remote("origin", "/tmp/repo.git", false)
            .expect("add remote");

        assert!(git.set_remote_url("origin", "", None).is_err());
        assert!(
            git.set_remote_url("origin", "--upload-pack=evil", None)
                .is_err()
        );
        assert!(git.add_remote("other", "-x", false).is_err());
        assert_eq!(
            repo.git(&["remote", "get-url", "origin"]).trim(),
            "/tmp/repo.git",
            "a refused URL leaves the remote pointing where it did"
        );
    }
}
