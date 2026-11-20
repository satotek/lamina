//! End-to-end tests: real repositories, real `git diff` output, real
//! `git apply`.
//!
//! The parser below exists only so the tests can turn git's unified diff into
//! a [`FileDiff`] without depending on `diff_text`, which is still a stub. It
//! follows the same convention as the real thing: `DiffLine::content` has
//! neither `\n` nor `\r`, and the file's line ending is carried separately.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use lamina_core::diff::{DiffLine, FileDiff, Hunk, LineKind};
use lamina_core::patch::{LineEnding, LineSelection, PatchFile, PatchMode, build_patch};

#[test]
fn stages_only_the_selected_added_line() {
    let repo = Repo::new();
    repo.write("file.txt", "one\ntwo\nthree\n");
    repo.commit("initial");
    repo.write("file.txt", "one\ntwo\nthree\nfour\nfive\nsix\n");

    let file = repo.diff_file(false, "file.txt");
    assert_eq!(file.diff.hunks.len(), 1);
    let added = change_lines(&file, LineKind::Added);
    assert_eq!(added.len(), 3);

    let patch = build(&file, &selection(&added[1..2]), PatchMode::Stage).expect("patch");
    repo.apply(&patch, PatchMode::Stage);

    assert_eq!(text(repo.index("file.txt")), "one\ntwo\nthree\nfive\n");
    // The untouched additions stay in the working tree.
    assert_eq!(
        text(repo.read("file.txt")),
        "one\ntwo\nthree\nfour\nfive\nsix\n"
    );
}

#[test]
fn stages_only_the_selected_removed_line() {
    let repo = Repo::new();
    repo.write("file.txt", "a\nb\nc\nd\ne\n");
    repo.commit("initial");
    repo.write("file.txt", "a\nb\nd\n");

    let file = repo.diff_file(false, "file.txt");
    let removed = change_lines(&file, LineKind::Removed);
    assert_eq!(removed.len(), 2);

    let patch = build(&file, &selection(&removed[0..1]), PatchMode::Stage).expect("patch");
    repo.apply(&patch, PatchMode::Stage);

    // The removal of `c` is staged, the removal of `e` is still pending.
    assert_eq!(text(repo.index("file.txt")), "a\nb\nd\ne\n");
    assert_eq!(text(repo.read("file.txt")), "a\nb\nd\n");
}

#[test]
fn stages_only_the_removal_of_a_replacement() {
    let repo = Repo::new();
    repo.write("file.txt", "one\ntwo\nthree\n");
    repo.commit("initial");
    repo.write("file.txt", "one\nTWO\nthree\n");

    let file = repo.diff_file(false, "file.txt");
    let removed = change_lines(&file, LineKind::Removed);
    assert_eq!(removed.len(), 1);

    let patch = build(&file, &selection(&removed), PatchMode::Stage).expect("patch");
    repo.apply(&patch, PatchMode::Stage);

    assert_eq!(text(repo.index("file.txt")), "one\nthree\n");
    assert_eq!(text(repo.read("file.txt")), "one\nTWO\nthree\n");

    // Only the addition is left to stage.
    let unstaged = repo.diff_file(false, "file.txt");
    assert_eq!(change_lines(&unstaged, LineKind::Added).len(), 1);
    assert!(change_lines(&unstaged, LineKind::Removed).is_empty());
}

#[test]
fn stages_only_the_second_hunk() {
    let repo = Repo::new();
    repo.write("file.txt", &numbered(1..=30, |_| None));
    repo.commit("initial");
    repo.write(
        "file.txt",
        &numbered(1..=30, |n| match n {
            2 => Some("line 2 changed".to_string()),
            25 => Some("line 25 changed".to_string()),
            _ => None,
        }),
    );

    let file = repo.diff_file(false, "file.txt");
    assert_eq!(file.diff.hunks.len(), 2);

    let patch = build(
        &file,
        &LineSelection::all_of_hunk(&file.diff, 1),
        PatchMode::Stage,
    )
    .expect("patch");

    // One hunk with the original position of the second hunk; nothing before
    // it was emitted, so there is nothing to shift.
    assert_eq!(patch.matches("@@ -").count(), 1);
    assert!(
        patch.contains("@@ -22,7 +22,7 @@"),
        "unexpected hunk header in:\n{patch}"
    );

    repo.apply(&patch, PatchMode::Stage);

    let expected = numbered(1..=30, |n| {
        if n == 25 {
            Some("line 25 changed".to_string())
        } else {
            None
        }
    });
    assert_eq!(text(repo.index("file.txt")), expected);
}

#[test]
fn shifts_the_second_hunk_after_a_partial_first_hunk() {
    let repo = Repo::new();
    repo.write("file.txt", &numbered(1..=40, |_| None));
    repo.commit("initial");
    repo.write(
        "file.txt",
        &with_insertions(&[(5, "added 1\nadded 2\nadded 3"), (30, "added 4")]),
    );

    let file = repo.diff_file(false, "file.txt");
    assert_eq!(file.diff.hunks.len(), 2);

    // Drop two of the three additions of the first hunk, so the second hunk
    // starts two lines earlier than in the original diff.
    let added = change_lines(&file, LineKind::Added);
    assert_eq!(added.len(), 4);
    let second = change_lines_in(&file, 1, LineKind::Added);
    let patch = build(&file, &selection(&[added[0], second[0]]), PatchMode::Stage).expect("patch");

    let hunk = &file.diff.hunks[1];
    let expected = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start,
        hunk.old_len,
        hunk.new_start - 2,
        hunk.new_len
    );
    assert!(patch.contains(&expected), "missing {expected} in:\n{patch}");

    repo.apply(&patch, PatchMode::Stage);

    let expected_index = with_insertions(&[(5, "added 1"), (30, "added 4")]);
    assert_eq!(text(repo.index("file.txt")), expected_index);
}

#[test]
fn unstages_only_the_selected_added_line() {
    let repo = Repo::new();
    repo.write("file.txt", "one\ntwo\nthree\n");
    repo.commit("initial");
    repo.write("file.txt", "one\ninserted 1\ntwo\ninserted 2\nthree\n");
    repo.git(&["add", "file.txt"]);

    let file = repo.diff_file(true, "file.txt");
    let added = change_lines(&file, LineKind::Added);
    assert_eq!(added.len(), 2);

    let patch = build(&file, &selection(&added[0..1]), PatchMode::Unstage).expect("patch");
    repo.apply(&patch, PatchMode::Unstage);

    assert_eq!(
        text(repo.index("file.txt")),
        "one\ntwo\ninserted 2\nthree\n"
    );
    assert_eq!(
        text(repo.read("file.txt")),
        "one\ninserted 1\ntwo\ninserted 2\nthree\n"
    );
    assert_eq!(text(repo.show("HEAD:file.txt")), "one\ntwo\nthree\n");
}

#[test]
fn discards_only_the_selected_added_line() {
    let repo = Repo::new();
    repo.write("file.txt", "one\ntwo\n");
    repo.commit("initial");
    repo.write("file.txt", "one\ninserted 1\ntwo\ninserted 2\n");

    let file = repo.diff_file(false, "file.txt");
    let added = change_lines(&file, LineKind::Added);
    assert_eq!(added.len(), 2);

    let patch = build(&file, &selection(&added[1..2]), PatchMode::Discard).expect("patch");
    repo.apply(&patch, PatchMode::Discard);

    assert_eq!(text(repo.read("file.txt")), "one\ninserted 1\ntwo\n");
    assert_eq!(text(repo.index("file.txt")), "one\ntwo\n");
}

#[test]
fn stages_part_of_a_new_file() {
    let repo = Repo::new();
    repo.write("other.txt", "keep\n");
    repo.commit("initial");
    repo.write("new.txt", "alpha\nbeta\ngamma\n");
    // Without intent-to-add an untracked file is not part of `git diff`.
    repo.git(&["add", "--intent-to-add", "new.txt"]);

    let file = repo.diff_file(false, "new.txt");
    assert_eq!(file.old_path, None);
    let added = change_lines(&file, LineKind::Added);
    assert_eq!(added.len(), 3);

    let patch = build(&file, &selection(&added[0..2]), PatchMode::Stage).expect("patch");
    assert!(
        patch.starts_with("new file mode 100644\n--- /dev/null\n+++ b/new.txt\n"),
        "unexpected header in:\n{patch}"
    );
    repo.apply(&patch, PatchMode::Stage);

    assert_eq!(text(repo.index("new.txt")), "alpha\nbeta\n");
    assert_eq!(text(repo.read("new.txt")), "alpha\nbeta\ngamma\n");
}

#[test]
fn unstages_part_of_a_new_file() {
    let repo = Repo::new();
    repo.write("other.txt", "keep\n");
    repo.commit("initial");
    repo.write("new.txt", "alpha\nbeta\ngamma\n");
    repo.git(&["add", "new.txt"]);

    let file = repo.diff_file(true, "new.txt");
    assert_eq!(file.old_path, None);
    let added = change_lines(&file, LineKind::Added);
    assert_eq!(added.len(), 3);

    let patch = build(&file, &selection(&added[0..1]), PatchMode::Unstage).expect("patch");
    // The index keeps the other lines, so the old side cannot be `/dev/null`.
    assert!(
        patch.starts_with("--- a/new.txt\n+++ b/new.txt\n"),
        "unexpected header in:\n{patch}"
    );
    assert!(
        patch.contains("@@ -1,2 +1,3 @@"),
        "unexpected hunk header in:\n{patch}"
    );
    repo.apply(&patch, PatchMode::Unstage);

    assert_eq!(text(repo.index("new.txt")), "beta\ngamma\n");
    assert_eq!(text(repo.read("new.txt")), "alpha\nbeta\ngamma\n");
}

#[test]
fn keeps_a_missing_newline_when_staging_an_addition() {
    let repo = Repo::new();
    repo.write("file.txt", "a\nb\nc");
    repo.commit("initial");
    repo.write("file.txt", "a\nB\nc");

    let file = repo.diff_file(false, "file.txt");
    let added = change_lines(&file, LineKind::Added);
    assert_eq!(added.len(), 1);

    let patch = build(&file, &selection(&added), PatchMode::Stage).expect("patch");
    assert!(
        patch.ends_with(" c\n\\ No newline at end of file\n"),
        "unexpected patch tail:\n{patch}"
    );
    repo.apply(&patch, PatchMode::Stage);

    assert_eq!(text(repo.index("file.txt")), "a\nb\nB\nc");
}

#[test]
fn keeps_a_missing_newline_when_staging_a_removal() {
    let repo = Repo::new();
    repo.write("file.txt", "alpha\nbeta");
    repo.commit("initial");
    repo.write("file.txt", "alpha\nBETA");

    let file = repo.diff_file(false, "file.txt");
    let removed = change_lines(&file, LineKind::Removed);
    assert_eq!(removed.len(), 1);

    let patch = build(&file, &selection(&removed), PatchMode::Stage).expect("patch");
    assert!(
        patch.contains("-beta\n\\ No newline at end of file\n"),
        "unexpected patch body:\n{patch}"
    );
    repo.apply(&patch, PatchMode::Stage);

    assert_eq!(text(repo.index("file.txt")), "alpha\n");
}

#[test]
fn stages_a_single_added_line_in_a_crlf_file() {
    let repo = Repo::new();
    repo.write("file.txt", "one\r\ntwo\r\n");
    repo.commit("initial");
    repo.write("file.txt", "one\r\ntwo\r\nthree\r\nfour\r\n");

    let file = repo.diff_file(false, "file.txt");
    assert_eq!(file.line_ending, LineEnding::CrLf);
    let added = change_lines(&file, LineKind::Added);
    assert_eq!(added.len(), 2);

    let patch = build(&file, &selection(&added[1..2]), PatchMode::Stage).expect("patch");
    assert!(patch.contains("+four\r\n"), "unexpected patch:\n{patch}");
    repo.apply(&patch, PatchMode::Stage);

    assert_eq!(text(repo.index("file.txt")), "one\r\ntwo\r\nfour\r\n");
}

#[test]
fn stages_a_deleted_file_only_as_a_whole() {
    let repo = Repo::new();
    repo.write("file.txt", "one\ntwo\nthree\n");
    repo.commit("initial");
    repo.remove("file.txt");

    let file = repo.diff_file(false, "file.txt");
    assert!(file.is_deleted);

    let removed = change_lines(&file, LineKind::Removed);
    assert_eq!(removed.len(), 3);
    assert!(
        build(&file, &selection(&removed[0..1]), PatchMode::Stage).is_none(),
        "a partial deletion cannot be staged"
    );

    let patch = build(&file, &LineSelection::all(&file.diff), PatchMode::Stage).expect("patch");
    assert!(
        patch.starts_with("deleted file mode 100644\n--- a/file.txt\n+++ /dev/null\n"),
        "unexpected header in:\n{patch}"
    );
    repo.apply(&patch, PatchMode::Stage);

    assert_eq!(
        repo.git(&["diff", "--cached", "--name-status"]),
        "D\tfile.txt\n"
    );
}

#[test]
fn builds_no_patch_without_selected_changes() {
    let repo = Repo::new();
    repo.write("file.txt", "one\ntwo\nthree\n");
    repo.commit("initial");
    repo.write("file.txt", "one\nTWO\nthree\n");

    let file = repo.diff_file(false, "file.txt");
    assert!(build(&file, &LineSelection::default(), PatchMode::Stage).is_none());
    assert!(build(&file, &LineSelection::new([(0, 0)]), PatchMode::Stage).is_none());
    assert!(build(&file, &LineSelection::new([(7, 7)]), PatchMode::Stage).is_none());
    assert!(
        build(
            &file,
            &selection(&change_lines(&file, LineKind::Added)),
            PatchMode::Stage
        )
        .is_some()
    );
}

fn selection(lines: &[(usize, usize)]) -> LineSelection {
    lines.iter().copied().collect()
}

fn text(bytes: impl Into<Vec<u8>>) -> String {
    String::from_utf8(bytes.into()).expect("utf-8 content")
}

/// Lines `1..=last` as `line N`, with `replace` able to swap a line for any
/// number of lines (the string's `\n`s split it).
fn numbered(
    range: std::ops::RangeInclusive<u32>,
    replace: impl Fn(u32) -> Option<String>,
) -> String {
    let mut out = String::new();
    for n in range {
        match replace(n) {
            Some(replacement) => {
                out.push_str(&replacement);
                out.push('\n');
            }
            None => out.push_str(&format!("line {n}\n")),
        }
    }
    out
}

/// `line 1` to `line 40`, with `extra` inserted after the numbered line.
fn with_insertions(insertions: &[(u32, &str)]) -> String {
    let mut out = String::new();
    for n in 1..=40u32 {
        out.push_str(&format!("line {n}\n"));
        if let Some((_, extra)) = insertions.iter().find(|(after, _)| *after == n) {
            out.push_str(extra);
            out.push('\n');
        }
    }
    out
}

fn change_lines(file: &ParsedFile, kind: LineKind) -> Vec<(usize, usize)> {
    file.diff
        .hunks
        .iter()
        .enumerate()
        .flat_map(|(hunk_index, hunk)| {
            hunk.lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.kind == kind)
                .map(move |(line_index, _)| (hunk_index, line_index))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn change_lines_in(file: &ParsedFile, hunk_index: usize, kind: LineKind) -> Vec<(usize, usize)> {
    file.diff.hunks[hunk_index]
        .lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.kind == kind)
        .map(|(line_index, _)| (hunk_index, line_index))
        .collect()
}

fn build(file: &ParsedFile, selection: &LineSelection, mode: PatchMode) -> Option<String> {
    let patch_file = PatchFile {
        path: &file.path,
        old_path: file.old_path.as_deref(),
        diff: &file.diff,
        line_ending: file.line_ending,
        is_deleted: file.is_deleted,
    };
    build_patch(&patch_file, selection, mode)
}

struct Repo {
    /// Kept alive so the temporary directory outlives the repository.
    _root: tempfile::TempDir,
    work: PathBuf,
    config: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        let work = root.path().join("work");
        fs::create_dir(&work).expect("work tree");
        // An empty file keeps the tests independent of the machine's git config.
        let config = root.path().join("gitconfig");
        fs::write(&config, "").expect("empty git config");
        let repo = Self {
            _root: root,
            work,
            config,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(&self.work)
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_CONFIG_SYSTEM", &self.config)
            .env("GIT_AUTHOR_NAME", "Lamina Test")
            .env("GIT_AUTHOR_EMAIL", "lamina@example.com")
            .env("GIT_COMMITTER_NAME", "Lamina Test")
            .env("GIT_COMMITTER_EMAIL", "lamina@example.com")
            .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
            .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["-c", "core.autocrlf=false"])
            .args(["-c", "core.safecrlf=false"])
            .args(["-c", "diff.noprefix=false"])
            .args(["-c", "core.quotePath=false"]);
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git writes utf-8 here")
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.work.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory");
        }
        fs::write(path, contents).expect("write file");
    }

    fn read(&self, path: &str) -> Vec<u8> {
        fs::read(self.work.join(path)).expect("read file")
    }

    fn remove(&self, path: &str) {
        fs::remove_file(self.work.join(path)).expect("remove file");
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "--", "."]);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn diff(&self, cached: bool) -> String {
        if cached {
            self.git(&["diff", "--cached", "--no-color"])
        } else {
            self.git(&["diff", "--no-color"])
        }
    }

    fn index(&self, path: &str) -> Vec<u8> {
        self.show(&format!(":{path}"))
    }

    fn show(&self, object: &str) -> Vec<u8> {
        let output = self
            .command()
            .args(["show", object])
            .output()
            .expect("run git show");
        assert!(
            output.status.success(),
            "git show {object} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn diff_file(&self, cached: bool, path: &str) -> ParsedFile {
        let output = self.diff(cached);
        parse_diff(&output)
            .into_iter()
            .find(|file| file.path == path || file.old_path.as_deref() == Some(path))
            .unwrap_or_else(|| panic!("no diff for {path} in:\n{output}"))
    }

    /// Runs `git apply --check` first, then the real thing.
    fn apply(&self, patch: &str, mode: PatchMode) {
        let mut check = mode.apply_args().to_vec();
        check.push("--check");
        self.run_apply(patch, &check);
        self.run_apply(patch, mode.apply_args());
    }

    fn run_apply(&self, patch: &str, args: &[&str]) {
        let mut child = self
            .command()
            .arg("apply")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git apply");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(patch.as_bytes())
            .expect("write patch");
        let output = child.wait_with_output().expect("git apply output");
        assert!(
            output.status.success(),
            "git apply {args:?} failed: {}\n--- patch ---\n{patch}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[derive(Debug)]
struct ParsedFile {
    path: String,
    old_path: Option<String>,
    is_deleted: bool,
    line_ending: LineEnding,
    diff: FileDiff,
}

fn parse_diff(output: &str) -> Vec<ParsedFile> {
    let lines: Vec<&str> = output.split_inclusive('\n').collect();
    let mut files: Vec<ParsedFile> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];

        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (old, new) = rest.split_once(" b/").expect("git diff paths");
            files.push(ParsedFile {
                path: new.trim_end().to_string(),
                old_path: Some(old.trim_start_matches("a/").to_string()),
                is_deleted: false,
                line_ending: LineEnding::Lf,
                diff: FileDiff::default(),
            });
            index += 1;
            continue;
        }

        if line.starts_with("new file mode ") {
            files.last_mut().expect("file before mode").old_path = None;
            index += 1;
            continue;
        }

        if line.starts_with("deleted file mode ") {
            files.last_mut().expect("file before mode").is_deleted = true;
            index += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix("--- ") {
            let path = path.trim_end();
            let file = files.last_mut().expect("file before ---");
            file.old_path =
                (path != "/dev/null").then(|| path.trim_start_matches("a/").to_string());
            index += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix("+++ ") {
            let path = path.trim_end();
            let file = files.last_mut().expect("file before +++");
            if path == "/dev/null" {
                file.is_deleted = true;
            } else {
                file.path = path.trim_start_matches("b/").to_string();
            }
            index += 1;
            continue;
        }

        if let Some(header) = line.strip_prefix("@@ ") {
            let header = header.split(" @@").next().expect("hunk ranges");
            let mut parts = header.split(' ');
            let old = parse_range(parts.next().expect("old range").trim_start_matches('-'));
            let new = parse_range(parts.next().expect("new range").trim_start_matches('+'));

            let mut hunk = Hunk {
                old_start: old.0,
                old_len: old.1,
                new_start: new.0,
                new_len: new.1,
                lines: Vec::new(),
            };
            let mut old_lineno = hunk.old_start;
            let mut new_lineno = hunk.new_start;
            let mut old_remaining = hunk.old_len;
            let mut new_remaining = hunk.new_len;
            let mut crlf = false;
            index += 1;

            while old_remaining > 0 || new_remaining > 0 {
                let raw = lines.get(index).expect("truncated hunk");
                index += 1;
                let (kind, content) = match raw.as_bytes().first() {
                    // Belongs to the line above and consumes no range.
                    Some(b'\\') => {
                        hunk.lines
                            .last_mut()
                            .expect("marker after a line")
                            .no_newline_at_eof = true;
                        continue;
                    }
                    Some(b' ') => (LineKind::Context, &raw[1..]),
                    Some(b'+') => (LineKind::Added, &raw[1..]),
                    Some(b'-') => (LineKind::Removed, &raw[1..]),
                    _ => panic!("unexpected hunk line {raw:?}"),
                };
                let (content, had_cr) = split_line_end(content);
                crlf |= had_cr;
                let (old_lineno_opt, new_lineno_opt) = match kind {
                    LineKind::Context => {
                        old_remaining -= 1;
                        new_remaining -= 1;
                        let numbers = (Some(old_lineno), Some(new_lineno));
                        old_lineno += 1;
                        new_lineno += 1;
                        numbers
                    }
                    LineKind::Added => {
                        new_remaining -= 1;
                        let numbers = (None, Some(new_lineno));
                        new_lineno += 1;
                        numbers
                    }
                    LineKind::Removed => {
                        old_remaining -= 1;
                        let numbers = (Some(old_lineno), None);
                        old_lineno += 1;
                        numbers
                    }
                };
                hunk.lines.push(DiffLine {
                    kind,
                    old_lineno: old_lineno_opt,
                    new_lineno: new_lineno_opt,
                    content,
                    no_newline_at_eof: false,
                    emphasis: Vec::new(),
                });
            }

            // The marker can also be the last line of the hunk.
            while let Some(raw) = lines.get(index) {
                if !raw.starts_with('\\') {
                    break;
                }
                hunk.lines
                    .last_mut()
                    .expect("marker after a line")
                    .no_newline_at_eof = true;
                index += 1;
            }

            let file = files.last_mut().expect("file before hunk");
            if crlf {
                file.line_ending = LineEnding::CrLf;
            }
            file.diff.hunks.push(hunk);
            continue;
        }

        index += 1;
    }
    files
}

fn parse_range(text: &str) -> (u32, u32) {
    match text.split_once(',') {
        Some((start, len)) => (
            start.parse().expect("range start"),
            len.parse().expect("range length"),
        ),
        None => (text.parse().expect("range start"), 1),
    }
}

fn split_line_end(raw: &str) -> (String, bool) {
    let body = raw.strip_suffix('\n').unwrap_or(raw);
    match body.strip_suffix('\r') {
        Some(body) => (body.to_string(), true),
        None => (body.to_string(), false),
    }
}
