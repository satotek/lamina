//! End-to-end check of the line staging pipeline as the UI drives it:
//! read sides with `Repo` (gix) -> `diff_text` -> `build_patch` -> `Git::apply_patch`.
//! Each module has its own tests; this one catches disagreements between them,
//! e.g. hunk coordinates from `diff_text` that `git apply` rejects, or `Repo`
//! returning a stale index after the git CLI rewrote it.

use std::fs;
use std::path::Path;
use std::process::Command;

use strata_core::diff::{DiffOptions, LineKind, diff_text};
use strata_core::git_cli::{ApplyMode, Git};
use strata_core::patch::{LineSelection, PatchFile, PatchMode, build_patch};
use strata_core::repo::Repo;
use tempfile::TempDir;

const FILE: &str = "src/lib.txt";

struct Fixture {
    _dir: TempDir,
    root: std::path::PathBuf,
    git: Git,
}

fn isolated(cmd: &mut Command, empty_config: &Path) {
    cmd.env("GIT_CONFIG_GLOBAL", empty_config)
        .env("GIT_CONFIG_SYSTEM", empty_config)
        .env("GIT_AUTHOR_NAME", "Strata Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "Strata Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z");
}

impl Fixture {
    fn new(head_content: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        let empty_config = dir.path().join("empty-gitconfig");
        fs::write(&empty_config, "").unwrap();

        let run = |args: &[&str]| {
            let mut cmd = Command::new("git");
            cmd.args(args).current_dir(&root);
            isolated(&mut cmd, &empty_config);
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "core.autocrlf", "false"]);
        fs::write(root.join(FILE), head_content).unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "base"]);

        let mut git = Git::new(root.clone());
        for (key, value) in [
            ("GIT_CONFIG_GLOBAL", empty_config.as_os_str()),
            ("GIT_CONFIG_SYSTEM", empty_config.as_os_str()),
        ] {
            git = git.with_env(key, value);
        }
        Self {
            _dir: dir,
            root,
            git,
        }
    }

    fn repo(&self) -> Repo {
        // Reopened on purpose: the UI may hold a Repo across git CLI writes,
        // which `reused_repo_sees_index_updates` covers separately.
        Repo::open(&self.root).unwrap()
    }

    fn index(&self) -> String {
        String::from_utf8(self.repo().file_in_index(FILE).unwrap().unwrap()).unwrap()
    }

    fn worktree(&self) -> String {
        fs::read_to_string(self.root.join(FILE)).unwrap()
    }

    fn head(&self) -> String {
        let out = Command::new("git")
            .args(["show", &format!("HEAD:{FILE}")])
            .current_dir(&self.root)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    /// Applies one changed line of the diff between `old` and `new` in `mode`.
    /// Returns false when there is nothing left to apply.
    fn apply_one_line(&self, old: &str, new: &str, mode: PatchMode, pick: usize) -> bool {
        let diff = diff_text(old.as_bytes(), new.as_bytes(), &DiffOptions::default());
        let changed: Vec<(usize, usize)> = diff
            .hunks
            .iter()
            .enumerate()
            .flat_map(|(h, hunk)| {
                hunk.lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.kind != LineKind::Context)
                    .map(move |(i, _)| (h, i))
            })
            .collect();
        if changed.is_empty() {
            return false;
        }
        let selection = LineSelection::new([changed[pick % changed.len()]]);
        let patch = build_patch(&PatchFile::new(FILE, &diff), &selection, mode)
            .expect("a selected changed line yields a patch");
        let apply_mode = match mode {
            PatchMode::Stage => ApplyMode::Stage,
            PatchMode::Unstage => ApplyMode::Unstage,
            PatchMode::Discard => ApplyMode::Discard,
        };
        self.git
            .apply_patch(&patch, apply_mode)
            .unwrap_or_else(|e| panic!("git apply rejected patch:\n{patch}\n{e}"));
        true
    }
}

/// Deterministic pseudo-random numbers so failures reproduce.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as usize
    }
}

fn base_text() -> String {
    (1..=60).map(|i| format!("line {i}\n")).collect()
}

fn edited(base: &str, rng: &mut Lcg) -> String {
    let mut lines: Vec<String> = base.lines().map(str::to_owned).collect();
    for _ in 0..8 {
        let at = rng.next() % (lines.len() + 1);
        match rng.next() % 3 {
            0 if at < lines.len() => {
                lines.remove(at);
            }
            1 if at < lines.len() => lines[at] = format!("changed {}", rng.next() % 1000),
            _ => lines.insert(at, format!("inserted {}", rng.next() % 1000)),
        }
    }
    lines.iter().map(|l| format!("{l}\n")).collect()
}

#[test]
fn staging_every_line_one_by_one_reaches_the_worktree() {
    for seed in 1..=15 {
        let base = base_text();
        let fx = Fixture::new(&base);
        let mut rng = Lcg(seed);
        fs::write(fx.root.join(FILE), edited(&base, &mut rng)).unwrap();

        let mut steps = 0;
        while fx.apply_one_line(&fx.index(), &fx.worktree(), PatchMode::Stage, rng.next()) {
            steps += 1;
            assert!(steps < 200, "seed {seed}: staging does not converge");
        }
        assert_eq!(fx.index(), fx.worktree(), "seed {seed}");
    }
}

#[test]
fn unstaging_every_line_one_by_one_reaches_head() {
    for seed in 1..=15 {
        let base = base_text();
        let fx = Fixture::new(&base);
        let mut rng = Lcg(seed * 7919);
        fs::write(fx.root.join(FILE), edited(&base, &mut rng)).unwrap();
        fx.git.stage_paths(&[FILE]).unwrap();

        let mut steps = 0;
        while fx.apply_one_line(&fx.head(), &fx.index(), PatchMode::Unstage, rng.next()) {
            steps += 1;
            assert!(steps < 200, "seed {seed}: unstaging does not converge");
        }
        assert_eq!(fx.index(), fx.head(), "seed {seed}");
    }
}

#[test]
fn discarding_every_line_one_by_one_restores_the_index() {
    for seed in 1..=15 {
        let base = base_text();
        let fx = Fixture::new(&base);
        let mut rng = Lcg(seed * 104_729);
        fs::write(fx.root.join(FILE), edited(&base, &mut rng)).unwrap();

        let mut steps = 0;
        while fx.apply_one_line(&fx.index(), &fx.worktree(), PatchMode::Discard, rng.next()) {
            steps += 1;
            assert!(steps < 200, "seed {seed}: discarding does not converge");
        }
        assert_eq!(fx.worktree(), fx.index(), "seed {seed}");
    }
}

#[test]
fn reused_repo_sees_index_updates() {
    let base = base_text();
    let fx = Fixture::new(&base);
    let repo = fx.repo();
    let new = base.replace("line 30\n", "line thirty\n");
    fs::write(fx.root.join(FILE), &new).unwrap();
    fx.git.stage_paths(&[FILE]).unwrap();

    let staged = repo.file_in_index(FILE).unwrap().unwrap();
    assert_eq!(String::from_utf8(staged).unwrap(), new);
    let status = repo.status().unwrap();
    assert_eq!(status.staged.len(), 1);
    assert!(status.unstaged.is_empty());
}
