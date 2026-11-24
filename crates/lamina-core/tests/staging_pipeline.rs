//! End-to-end check of the line staging pipeline as the UI drives it:
//! read sides with `Repo` (gix) -> `diff_text` -> `build_patch` -> `Git::apply_patch`.
//! Each module has its own tests; this one catches disagreements between them,
//! e.g. hunk coordinates from `diff_text` that `git apply` rejects, or `Repo`
//! returning a stale index after the git CLI rewrote it.
//!
//! Five random seeds per test: each seed drives the full loop until no single
//! line is left to apply, so extra seeds repeated the same code paths on a
//! different edit order instead of covering new ones.

use std::fs;

use lamina_core::diff::{DiffOptions, LineKind, diff_text};
use lamina_core::git_cli::{ApplyMode, Git};
use lamina_core::patch::{LineSelection, PatchFile, PatchMode, build_patch};
use lamina_core::repo::Repo;
use lamina_core::test_support::TestRepo;

const FILE: &str = "src/lib.txt";

struct Fixture {
    repo: TestRepo,
    git: Git,
}

impl Fixture {
    fn new(head_content: &str) -> Self {
        Self::with_autocrlf(head_content, false)
    }

    fn with_autocrlf(head_content: &str, autocrlf: bool) -> Self {
        let repo = TestRepo::new();
        // Repo-local config wins over the fixture's global file, so the line
        // ending behaviour applies to both the index and git's own commands.
        repo.git(&[
            "config",
            "core.autocrlf",
            if autocrlf { "true" } else { "false" },
        ]);
        repo.write(FILE, head_content);
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "base"]);

        let git = repo.git_handle();
        Self { repo, git }
    }

    fn repo(&self) -> Repo {
        // Reopened on purpose: the UI may hold a Repo across git CLI writes,
        // which `reused_repo_sees_index_updates` covers separately.
        self.repo.open()
    }

    fn index(&self) -> String {
        String::from_utf8(self.repo().file_in_index(FILE).unwrap().unwrap()).unwrap()
    }

    fn worktree(&self) -> String {
        fs::read_to_string(self.repo.path().join(FILE)).unwrap()
    }

    /// The worktree file as git would store it, which is what a diff must use
    /// whenever `core.autocrlf` or a `text` attribute is in play.
    fn worktree_as_git(&self) -> String {
        let bytes = self.repo().file_in_worktree_as_git(FILE).unwrap().unwrap();
        String::from_utf8(bytes).unwrap()
    }

    fn head(&self) -> String {
        self.repo.git(&["show", &format!("HEAD:{FILE}")])
    }

    /// Run git in the fixture repository with the same isolated environment.
    fn git_output(&self, args: &[&str]) -> std::process::Output {
        self.repo.git_output(args)
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
    for seed in 1..=5 {
        let base = base_text();
        let fx = Fixture::new(&base);
        let mut rng = Lcg(seed);
        fs::write(fx.repo.path().join(FILE), edited(&base, &mut rng)).unwrap();

        let mut steps = 0;
        while fx.apply_one_line(&fx.index(), &fx.worktree(), PatchMode::Stage, rng.next()) {
            steps += 1;
            assert!(steps < 200, "seed {seed}: staging does not converge");
        }
        assert_eq!(fx.index(), fx.worktree(), "seed {seed}");
    }
}

/// The same pipeline on a repository where the checkout holds CRLF. The worktree
/// side of the diff has to be the git form, or every line would look changed and
/// the patches would not apply.
#[test]
fn staging_every_line_one_by_one_with_autocrlf() {
    for seed in 1..=5 {
        let base = base_text();
        let fx = Fixture::with_autocrlf(&base, true);
        let mut rng = Lcg(seed);
        let edited = edited(&base, &mut rng);
        // A Windows editor writes CRLF; git stores LF in the index.
        fs::write(fx.repo.path().join(FILE), edited.replace('\n', "\r\n")).unwrap();

        assert_ne!(fx.worktree(), fx.worktree_as_git(), "seed {seed}");
        assert_eq!(fx.worktree_as_git(), edited, "seed {seed}");

        let mut steps = 0;
        while fx.apply_one_line(
            &fx.index(),
            &fx.worktree_as_git(),
            PatchMode::Stage,
            rng.next(),
        ) {
            steps += 1;
            assert!(steps < 200, "seed {seed}: staging does not converge");
        }
        assert_eq!(fx.index(), edited, "seed {seed}");
        assert_eq!(fx.index(), fx.worktree_as_git(), "seed {seed}");

        // `git diff` converts the worktree the same way, so nothing is left.
        let diff = fx.git_output(&["diff", "--", FILE]);
        assert_eq!(
            String::from_utf8_lossy(&diff.stdout),
            "",
            "seed {seed}: git still sees a worktree diff"
        );
        assert!(
            fx.git_output(&["diff", "--quiet", "--", FILE])
                .status
                .success(),
            "seed {seed}: git diff is not quiet"
        );
    }
}

#[test]
fn unstaging_every_line_one_by_one_reaches_head() {
    for seed in 1..=5 {
        let base = base_text();
        let fx = Fixture::new(&base);
        let mut rng = Lcg(seed * 7919);
        fs::write(fx.repo.path().join(FILE), edited(&base, &mut rng)).unwrap();
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
    for seed in 1..=5 {
        let base = base_text();
        let fx = Fixture::new(&base);
        let mut rng = Lcg(seed * 104_729);
        fs::write(fx.repo.path().join(FILE), edited(&base, &mut rng)).unwrap();

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
    fs::write(fx.repo.path().join(FILE), &new).unwrap();
    fx.git.stage_paths(&[FILE]).unwrap();

    let staged = repo.file_in_index(FILE).unwrap().unwrap();
    assert_eq!(String::from_utf8(staged).unwrap(), new);
    let status = repo.status().unwrap();
    assert_eq!(status.staged.len(), 1);
    assert!(status.unstaged.is_empty());
}
