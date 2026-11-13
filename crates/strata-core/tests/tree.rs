//! Integration tests for tree listings and blob reads at a commit.
//!
//! Entries are checked against `git ls-tree`, so the contract is git's tree
//! model rather than this implementation's own expectations.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use strata_core::repo::Repo;
use strata_core::{TreeEntry, TreeEntryKind};
use tempfile::TempDir;

/// A repository in a temp directory driven by the `git` CLI with an isolated
/// configuration, so nothing depends on the machine's git setup.
struct Fixture {
    /// Owns the temp directory; never read, only kept alive.
    _tmp: TempDir,
    repo: PathBuf,
    empty_config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let repo = tmp.path().join("repo");
        let empty_config = tmp.path().join("empty-gitconfig");
        fs::write(&empty_config, b"").expect("empty config");
        fs::create_dir_all(&repo).expect("repo dir");

        let fixture = Self {
            _tmp: tmp,
            repo,
            empty_config,
        };
        fixture.git(&["init", "--quiet", "--initial-branch=main"]);
        fixture
    }

    fn git(&self, args: &[&str]) -> String {
        self.run(args, "")
    }

    fn git_input(&self, args: &[&str], input: &str) -> String {
        self.run(args, input)
    }

    fn run(&self, args: &[&str], input: &str) -> String {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(&self.repo)
            .env("GIT_CONFIG_GLOBAL", &self.empty_config)
            .env("GIT_CONFIG_SYSTEM", &self.empty_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Strata Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_COMMITTER_NAME", "Strata Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env("GIT_AUTHOR_DATE", "@1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "@1700000000 +0000")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("git is runnable");
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(input.as_bytes())
            .expect("write git stdin");
        let output = child.wait_with_output().expect("git finishes");

        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    }

    fn write(&self, file: &str, content: &str) {
        let path = self.repo.join(file);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("file parent");
        }
        fs::write(&path, content).expect("write file");
    }

    fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "-m", message]);
        self.head()
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    fn oid(&self, spec: &str) -> String {
        self.git(&["rev-parse", spec])
    }

    /// Store `content` as a blob and return its id, for index entries that have
    /// no worktree file (symlinks and submodules).
    fn blob(&self, content: &str) -> String {
        self.git_input(&["hash-object", "-w", "--stdin"], content)
    }

    fn open(&self) -> Repo {
        Repo::open(&self.repo).expect("open repo")
    }

    fn entries(&self, commit: &str, dir: &str) -> Vec<TreeEntry> {
        self.open()
            .tree_entries(commit.parse().expect("object id"), dir)
            .unwrap_or_else(|error| panic!("tree entries of {dir:?}: {error}"))
    }

    /// `git ls-tree` names directly below `dir`, without directories-first
    /// ordering, for comparing the listed set.
    fn git_names(&self, commit: &str, dir: &str) -> Vec<String> {
        let spec = format!("{commit}:{dir}");
        let mut names: Vec<String> = self
            .git(&["ls-tree", "--name-only", &spec])
            .lines()
            .map(str::to_string)
            .collect();
        names.sort();
        names
    }
}

fn names(entries: &[TreeEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.name.clone()).collect()
}

#[test]
fn tree_entries_lists_one_level_directories_first() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "one\n");
    fixture.write("z.txt", "z\n");
    fixture.write("b/c.txt", "c\n");
    fixture.write("b/d/e.txt", "e\n");
    fixture.write("m/n.txt", "n\n");
    let root = fixture.commit_all("root");
    fixture.write("a.txt", "changed\n");
    let second = fixture.commit_all("change a");

    let entries = fixture.entries(&root, "");
    assert_eq!(
        names(&entries),
        vec!["b", "m", "a.txt", "z.txt"],
        "directories come first, each group sorted by name"
    );
    assert_eq!(entries[0].kind, TreeEntryKind::Directory);
    assert_eq!(entries[0].path, "b");
    assert_eq!(entries[0].mode, 0o040000);
    assert_eq!(entries[0].size, None);
    assert_eq!(entries[2].kind, TreeEntryKind::File);
    assert_eq!(entries[2].mode, 0o100644);
    assert_eq!(entries[2].size, Some(4), "worktree file size when cheap");

    // The listing belongs to the requested commit, not HEAD.
    let at_second = fixture.entries(&second, "");
    assert_eq!(
        at_second
            .iter()
            .find(|entry| entry.name == "a.txt")
            .expect("a.txt")
            .size,
        Some(8),
        "the second commit changed a.txt"
    );
}

#[test]
fn tree_entries_descends_a_nested_directory_and_matches_git() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "one\n");
    fixture.write("b/c.txt", "c\n");
    fixture.write("b/d/e.txt", "e\n");
    fixture.write("b/z.txt", "z\n");
    let head = fixture.commit_all("root");

    let entries = fixture.entries(&head, "b");
    assert_eq!(
        names(&entries),
        vec!["d", "c.txt", "z.txt"],
        "one level only, directories first"
    );
    assert_eq!(entries[0].path, "b/d");
    assert_eq!(entries[1].path, "b/c.txt");
    let mut listed = names(&entries);
    listed.sort();
    assert_eq!(listed, fixture.git_names(&head, "b"));

    // Repeated and trailing slashes name the same tree.
    assert_eq!(names(&fixture.entries(&head, "b//")), names(&entries));

    let repo = fixture.open();
    let commit = head.parse().expect("object id");
    assert_eq!(
        repo.blob_at(commit, "b/d/e.txt").expect("blob"),
        b"e\n",
        "nested blob content"
    );
}

#[test]
fn tree_entries_reports_symlinks_and_submodules() {
    let fixture = Fixture::new();
    fixture.write("target.txt", "a.txt");
    fixture.commit_all("root");

    // Index entries are the portable way to build a symlink and a gitlink: no
    // platform-specific file-system call, and no second repository.
    let link_blob = fixture.blob("a.txt");
    let gitlink = fixture.oid("HEAD");
    fixture.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("120000,{link_blob},link"),
    ]);
    fixture.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{gitlink},vendor/sub"),
    ]);
    // The gitlink and the symlink exist only in the index, so commit the index
    // directly; `git add --all` would stage their removal from the worktree.
    fixture.git(&["commit", "--quiet", "-m", "add link and submodule"]);
    let head = fixture.head();

    let entries = fixture.entries(&head, "");
    assert_eq!(
        names(&entries),
        vec!["vendor", "link", "target.txt"],
        "the gitlink's directory is a directory"
    );
    let link = entries
        .iter()
        .find(|entry| entry.name == "link")
        .expect("symlink entry");
    assert_eq!(link.kind, TreeEntryKind::Symlink);
    assert_eq!(link.mode, 0o120000);
    assert_eq!(link.size, Some(5), "the blob holds the link target");

    let vendor = fixture.entries(&head, "vendor");
    assert_eq!(names(&vendor), vec!["sub"]);
    assert_eq!(vendor[0].kind, TreeEntryKind::Submodule);
    assert_eq!(vendor[0].mode, 0o160000);
    assert_eq!(vendor[0].size, None);
    assert_eq!(vendor[0].path, "vendor/sub");

    // The same modes git reports, so the mapping is not this crate's invention.
    let ls_tree = fixture.git(&["ls-tree", &head]);
    assert!(ls_tree.contains("120000 blob "), "{ls_tree}");
    let gitlink = fixture.git(&["ls-tree", &format!("{head}:vendor")]);
    assert!(gitlink.contains("160000 commit "), "{gitlink}");

    let repo = fixture.open();
    let commit = head.parse().expect("object id");
    assert_eq!(
        repo.blob_at(commit, "link").expect("symlink blob"),
        b"a.txt"
    );
}

#[test]
fn missing_paths_are_errors() {
    let fixture = Fixture::new();
    fixture.write("dir/file.txt", "one\n");
    let head = fixture.commit_all("root");
    let commit = head.parse().expect("object id");
    let repo = fixture.open();

    assert!(repo.tree_entries(commit, "nope").is_err());
    assert!(repo.tree_entries(commit, "dir/file.txt").is_err());
    assert!(repo.blob_at(commit, "nope").is_err());
    assert!(repo.blob_at(commit, "dir").is_err());
    assert!(repo.blob_at(commit, "dir/file.txt/child").is_err());
    assert_eq!(
        repo.blob_at(commit, "dir/file.txt").expect("blob"),
        b"one\n"
    );
}
