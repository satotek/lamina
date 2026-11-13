//! Tests for the text diff: hunk geometry, line endings, emphasis and the
//! round-trip property, plus a cross-check against the real `git`.

use std::process::Command;
use std::time::{Duration, Instant};

use super::lines;
use super::*;

fn diff(old: &str, new: &str) -> FileDiff {
    diff_text(old.as_bytes(), new.as_bytes(), &DiffOptions::default())
}

fn diff_with_context(old: &str, new: &str, context_lines: u32) -> FileDiff {
    diff_text(
        old.as_bytes(),
        new.as_bytes(),
        &DiffOptions { context_lines },
    )
}

fn contents(hunk: &Hunk) -> Vec<(&'static str, &str)> {
    hunk.lines
        .iter()
        .map(|line| {
            let prefix = match line.kind {
                LineKind::Context => " ",
                LineKind::Added => "+",
                LineKind::Removed => "-",
            };
            (prefix, line.content.as_str())
        })
        .collect()
}

fn emphasised(line: &DiffLine) -> Vec<&str> {
    line.emphasis
        .iter()
        .map(|range| &line.content[range.clone()])
        .collect()
}

/// Rebuilds the new text by applying `diff` to `old`, using only the public
/// hunk data. Any structural mistake in the hunks shows up here.
fn reconstruct(diff: &FileDiff, old: &str) -> String {
    let old_lines = lines::split(old);
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut pos = 0;
    for hunk in &diff.hunks {
        let start = if hunk.old_len == 0 {
            hunk.old_start as usize
        } else {
            hunk.old_start as usize - 1
        };
        assert!(start >= pos, "hunks must be ordered and non-overlapping");
        assert!(start <= old_lines.len(), "hunk starts past the old text");
        for line in &old_lines[pos..start] {
            out.push((line.content.to_string(), !line.no_newline));
        }
        for line in &hunk.lines {
            if line.kind != LineKind::Removed {
                out.push((line.content.clone(), !line.no_newline_at_eof));
            }
        }
        pos = start + hunk.old_len as usize;
        assert!(pos <= old_lines.len(), "hunk runs past the old text");
    }
    for line in &old_lines[pos..] {
        out.push((line.content.to_string(), !line.no_newline));
    }
    join(&out)
}

#[test]
fn identical_files_have_no_hunks() {
    let file_diff = diff("a\nb\nc\n", "a\nb\nc\n");
    assert!(file_diff.hunks.is_empty());
    assert!(!file_diff.binary);
    assert_eq!(to_unified(&file_diff, "a", "b"), "");
}

#[test]
fn nul_bytes_mark_a_file_binary() {
    let file_diff = diff_text(b"a\0b\n", b"a\0c\n", &DiffOptions::default());
    assert!(file_diff.binary);
    assert!(file_diff.hunks.is_empty());
    assert_eq!(
        to_unified(&file_diff, "old.bin", "new.bin"),
        "Binary files old.bin and new.bin differ\n"
    );

    let mut late_nul = vec![b'x'; 9000];
    late_nul[8500] = 0;
    assert!(!diff_text(&late_nul, &late_nul, &DiffOptions::default()).binary);
}

#[test]
fn hunk_headers_and_line_numbers() {
    let old: Vec<String> = (1..=20).map(|n| format!("line {n}\n")).collect();
    let new: Vec<String> = (1..=20)
        .map(|n| match n {
            2 => "line two\n".to_string(),
            20 => "line twenty\n".to_string(),
            n => format!("line {n}\n"),
        })
        .collect();
    let old = old.concat();
    let new = new.concat();
    let file_diff = diff(&old, &new);
    assert_eq!(file_diff.hunks.len(), 2);

    let first = &file_diff.hunks[0];
    assert_eq!(
        (
            first.old_start,
            first.old_len,
            first.new_start,
            first.new_len
        ),
        (1, 5, 1, 5)
    );
    assert_eq!(
        contents(first),
        vec![
            (" ", "line 1"),
            ("-", "line 2"),
            ("+", "line two"),
            (" ", "line 3"),
            (" ", "line 4"),
            (" ", "line 5"),
        ]
    );
    assert_eq!(
        (
            first.lines[1].old_lineno,
            first.lines[1].new_lineno,
            first.lines[2].old_lineno,
            first.lines[2].new_lineno
        ),
        (Some(2), None, None, Some(2))
    );

    let second = &file_diff.hunks[1];
    assert_eq!(
        (
            second.old_start,
            second.old_len,
            second.new_start,
            second.new_len
        ),
        (17, 4, 17, 4)
    );
    assert_eq!(
        contents(second),
        vec![
            (" ", "line 17"),
            (" ", "line 18"),
            (" ", "line 19"),
            ("-", "line 20"),
            ("+", "line twenty"),
        ]
    );
}

#[test]
fn changes_with_gap_equal_to_twice_the_context_are_merged() {
    // Changes on lines 1 and 8: six common lines in between.
    let merged = diff(
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n",
        "X\n2\n3\n4\n5\n6\n7\nY\n9\n10\n",
    );
    assert_eq!(merged.hunks.len(), 1, "gap 6 must merge under -U3");
    assert_eq!(
        (merged.hunks[0].old_start, merged.hunks[0].old_len),
        (1, 10)
    );
    assert_eq!(merged.hunks[0].lines.len(), 12);

    // One more common line and git emits two hunks.
    let split = diff(
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n",
        "X\n2\n3\n4\n5\n6\n7\n8\nY\n10\n11\n",
    );
    assert_eq!(split.hunks.len(), 2, "gap 7 must split under -U3");
    assert_eq!(split.hunks[0].old_len, 4);
    assert_eq!(split.hunks[1].old_start, 6);
}

#[test]
fn zero_context_keeps_only_changed_lines() {
    let file_diff = diff_with_context("a\nb\nc\n", "a\nB\nc\n", 0);
    let hunk = &file_diff.hunks[0];
    assert_eq!((hunk.old_start, hunk.old_len), (2, 1));
    assert_eq!(contents(hunk), vec![("-", "b"), ("+", "B")]);

    // Touching changes share a hunk; one common line between them splits it.
    let touching = diff_with_context("a\nb\nc\nd\n", "a\nX\nY\nd\n", 0);
    assert_eq!(touching.hunks.len(), 1);
    assert_eq!(
        (touching.hunks[0].old_start, touching.hunks[0].old_len),
        (2, 2)
    );
    let separated = diff_with_context("a\nb\nc\nd\ne\n", "a\nX\nc\nY\ne\n", 0);
    assert_eq!(separated.hunks.len(), 2);
}

#[test]
fn insertion_into_an_empty_file_starts_at_line_zero() {
    let file_diff = diff("", "p\nq\n");
    let hunk = &file_diff.hunks[0];
    assert_eq!(
        (hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len),
        (0, 0, 1, 2)
    );
    assert_eq!(
        to_unified(&file_diff, "old", "new"),
        "--- old\n+++ new\n@@ -0,0 +1,2 @@\n+p\n+q\n"
    );
    assert_eq!(reconstruct(&file_diff, ""), "p\nq\n");
}

#[test]
fn deleting_everything_leaves_an_empty_new_side() {
    let file_diff = diff("p\nq\n", "");
    let hunk = &file_diff.hunks[0];
    assert_eq!(
        (hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len),
        (1, 2, 0, 0)
    );
    assert_eq!(
        to_unified(&file_diff, "o", "n"),
        "--- o\n+++ n\n@@ -1,2 +0,0 @@\n-p\n-q\n"
    );
}

#[test]
fn appending_after_the_last_line_shows_leading_context() {
    let file_diff = diff("a\nb\n", "a\nb\nc\n");
    let hunk = &file_diff.hunks[0];
    assert_eq!(
        (hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len),
        (1, 2, 1, 3)
    );
    assert_eq!(contents(hunk), vec![(" ", "a"), (" ", "b"), ("+", "c")]);

    // Without context the empty old range falls back to "the line before".
    let bare = diff_with_context("a\nb\n", "a\nb\nc\n", 0);
    let hunk = &bare.hunks[0];
    assert_eq!(
        (hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len),
        (2, 0, 3, 1)
    );
    assert_eq!(
        to_unified(&bare, "o", "n"),
        "--- o\n+++ n\n@@ -2,0 +3 @@\n+c\n"
    );
}

#[test]
fn crlf_to_lf_is_a_change_but_the_carriage_return_is_hidden() {
    let file_diff = diff("a\r\nb\r\n", "a\nb\n");
    assert_eq!(file_diff.hunks.len(), 1);
    let hunk = &file_diff.hunks[0];
    assert_eq!(
        contents(hunk),
        vec![("-", "a"), ("-", "b"), ("+", "a"), ("+", "b")]
    );
    assert!(
        hunk.lines.iter().all(|line| !line.content.contains('\r')),
        "content must not carry the carriage return"
    );
    // The two sides are textually identical, so there is nothing to emphasise.
    assert!(hunk.lines.iter().all(|line| line.emphasis.is_empty()));

    assert!(diff("a\r\nb\r\n", "a\r\nb\r\n").hunks.is_empty());
}

#[test]
fn missing_trailing_newline_is_reported_on_the_side_that_lacks_it() {
    let file_diff = diff("a\nb", "a\nb\n");
    let hunk = &file_diff.hunks[0];
    assert_eq!(
        (hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len),
        (1, 2, 1, 2)
    );
    assert_eq!(
        (hunk.lines[1].kind, hunk.lines[1].no_newline_at_eof),
        (LineKind::Removed, true)
    );
    assert!(!hunk.lines[2].no_newline_at_eof);
    assert_eq!(
        to_unified(&file_diff, "a", "b"),
        "--- a\n+++ b\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n"
    );

    // No trailing newline on both sides: every emitted line carries the flag.
    let changed = diff("only", "other");
    assert!(changed.hunks[0].lines.iter().all(|l| l.no_newline_at_eof));
    assert_eq!(
        to_unified(&changed, "a", "b"),
        "--- a\n+++ b\n@@ -1 +1 @@\n-only\n\\ No newline at end of file\n+other\n\\ No newline at end of file\n"
    );
}

#[test]
fn invalid_utf8_is_diffed_lossily() {
    let file_diff = diff_text(b"a\xffb\n", b"a\xffc\n", &DiffOptions::default());
    assert!(!file_diff.binary);
    assert_eq!(file_diff.hunks.len(), 1);
    let joined: String = file_diff.hunks[0]
        .lines
        .iter()
        .map(|line| line.content.as_str())
        .collect();
    assert!(joined.contains('\u{FFFD}'), "got {joined:?}");

    // The same broken bytes on both sides mean no change.
    assert!(
        diff_text(b"a\xffb\n", b"a\xffb\n", &DiffOptions::default())
            .hunks
            .is_empty()
    );
}

#[test]
fn emphasis_marks_only_the_changed_word() {
    let file_diff = diff(
        "let value = compute(alpha);\n",
        "let value = compute(beta);\n",
    );
    let hunk = &file_diff.hunks[0];
    let removed = hunk
        .lines
        .iter()
        .find(|line| line.kind == LineKind::Removed)
        .expect("removed line");
    let added = hunk
        .lines
        .iter()
        .find(|line| line.kind == LineKind::Added)
        .expect("added line");

    assert_eq!(emphasised(removed), vec!["alpha"]);
    assert_eq!(emphasised(added), vec!["beta"]);
}

#[test]
fn emphasis_is_skipped_for_noisy_or_huge_lines() {
    // Nothing in common: nearly every byte changed, so the plain colours say
    // more than a highlighted line would.
    let unrelated = diff(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
    );
    assert!(
        unrelated.hunks[0]
            .lines
            .iter()
            .all(|line| line.emphasis.is_empty())
    );

    // A line too long to read emphasis on is left alone.
    let long_old = format!("{} alpha\n", "x".repeat(1200));
    let long_new = format!("{} beta\n", "x".repeat(1200));
    let long = diff(&long_old, &long_new);
    assert!(
        long.hunks[0]
            .lines
            .iter()
            .all(|line| line.emphasis.is_empty())
    );
}

#[test]
fn emphasis_pairs_unbalanced_runs_by_similarity() {
    // One removed line, two added: the similar pair is emphasised by index.
    let similar = diff("keep\nfoo bar\n", "keep\nfoo baz\nqux\n");
    let hunk = &similar.hunks[0];
    assert_eq!(
        contents(hunk),
        vec![
            (" ", "keep"),
            ("-", "foo bar"),
            ("+", "foo baz"),
            ("+", "qux")
        ]
    );
    assert_eq!(emphasised(&hunk.lines[1]), vec!["bar"]);
    assert_eq!(emphasised(&hunk.lines[2]), vec!["baz"]);
    assert!(hunk.lines[3].emphasis.is_empty());

    // An unbalanced pair that is too different (57% of the line changed) is
    // skipped even though it is under the noise threshold.
    let different = diff("keep\naa bbbb\nextra\n", "keep\naa cccc\n");
    let hunk = &different.hunks[0];
    let removed: Vec<&DiffLine> = hunk
        .lines
        .iter()
        .filter(|line| line.kind == LineKind::Removed)
        .collect();
    assert_eq!(removed.len(), 2);
    assert!(removed[0].emphasis.is_empty());
    assert!(hunk.lines.iter().all(|line| line.emphasis.is_empty()));
}

#[test]
fn emphasis_stays_within_content_bounds() {
    let file_diff = diff(
        "fn main() { println!(\"hello world\"); }\n",
        "fn main() { println!(\"hello there\"); }\n",
    );
    for line in file_diff.hunks.iter().flat_map(|hunk| &hunk.lines) {
        for range in &line.emphasis {
            assert!(range.end <= line.content.len());
            assert!(line.content.is_char_boundary(range.start));
            assert!(line.content.is_char_boundary(range.end));
        }
    }
}

#[test]
fn unified_diff_round_trips_through_the_hunks() {
    let old = "one\ntwo\nthree\nfour";
    let new = "one\nTWO\nthree\nfour\nfive\n";
    let file_diff = diff(old, new);
    assert_eq!(
        to_unified(&file_diff, "before.txt", "after.txt"),
        "--- before.txt\n+++ after.txt\n@@ -1,4 +1,5 @@\n one\n-two\n+TWO\n three\n-four\n\\ No newline at end of file\n+four\n+five\n"
    );
    assert_eq!(reconstruct(&file_diff, old), new);
}

#[test]
fn hunks_match_git() {
    let cases: [(&str, &str); 3] = [
        (
            "un\ndeux\ntrois\nquatre\ncinq\nsix\nsept\nhuit\nneuf\ndix\n\
             onze\ndouze\ntreize\nquatorze\nquinze\nseize\ndix-sept\ndix-huit\n",
            "un\nDEUX\ntrois\nquatre\ncinq\nsix\nsept\nhuit\nneuf\ndix\n\
             onze\ndouze\ntreize\nquatorze\nquinze\nseize\nDIX-SEPT\ndix-huit\nextra\n",
        ),
        // A deletion block and an insertion block with overlapping context.
        (
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n",
            "a\nb\nc\nX\nY\nf\ng\nh\ni\nj\nk\nl\n",
        ),
        // Changes at the very start and end exercise the 0-length and
        // 1-length header forms and the missing-final-newline marker.
        ("first\nmiddle\nlast", "FIRST\nmiddle\nLAST"),
    ];
    for (old, new) in cases {
        let git = git_hunks(old, new);
        let ours = our_hunks(old, new);
        assert_eq!(ours, git, "old: {old:?}\nnew: {new:?}");
        assert_eq!(
            our_hunk_headers(old, new),
            git.lines()
                .filter(|line| line.starts_with("@@"))
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }
}

fn our_hunks(old: &str, new: &str) -> String {
    let text = to_unified(&diff(old, new), "old", "new");
    match text.find("@@") {
        Some(idx) => text[idx..].to_string(),
        None => String::new(),
    }
}

fn our_hunk_headers(old: &str, new: &str) -> Vec<String> {
    our_hunks(old, new)
        .lines()
        .filter(|line| line.starts_with("@@"))
        .map(str::to_string)
        .collect()
}

/// Runs `git diff --no-index --histogram -U3` in a throwaway directory whose
/// git config is empty, so the machine's configuration cannot leak in, and
/// returns the hunks (everything from the first `@@` on).
fn git_hunks(old: &str, new: &str) -> String {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config = dir.path().join("gitconfig");
    std::fs::write(&config, "").expect("write config");
    let old_path = dir.path().join("old.txt");
    let new_path = dir.path().join("new.txt");
    std::fs::write(&old_path, old).expect("write old");
    std::fs::write(&new_path, new).expect("write new");

    let output = Command::new("git")
        .arg("diff")
        .arg("--no-index")
        .arg("--no-color")
        .arg("--histogram")
        .arg("-U3")
        .arg("--")
        .arg(&old_path)
        .arg(&new_path)
        .env("GIT_CONFIG_GLOBAL", &config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("git must be installed");
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "git diff --no-index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let body = match text.find("\n@@") {
        Some(idx) => &text[idx + 1..],
        None => return String::new(),
    };
    let mut normalized = String::new();
    for line in body.lines() {
        if line.starts_with("@@") {
            // `git` appends the enclosing function to the header; the hunk
            // range is everything up to the closing `@@`.
            let end = line.rfind("@@").map_or(line.len(), |idx| idx + 2);
            normalized.push_str(&line[..end]);
        } else {
            normalized.push_str(line);
        }
        normalized.push('\n');
    }
    normalized
}

#[test]
fn hunks_reconstruct_random_new_text() {
    let mut rng = Rng::new(0x5EED_1234_ABCD_0001);
    for case in 0..300 {
        let (old, new) = generate_pair(&mut rng);
        let opts = DiffOptions {
            context_lines: (case % 5) as u32,
        };
        let file_diff = diff_text(old.as_bytes(), new.as_bytes(), &opts);
        assert!(
            !file_diff.binary,
            "generated text must never look binary: {old:?} / {new:?}"
        );
        assert_eq!(
            reconstruct(&file_diff, &old),
            new,
            "case {case} failed\nold: {old:?}\nnew: {new:?}\ndiff: {file_diff:#?}"
        );

        // Headers must describe the lines actually present in each hunk.
        for hunk in &file_diff.hunks {
            let old_lines = hunk
                .lines
                .iter()
                .filter(|line| line.kind != LineKind::Added)
                .count() as u32;
            let new_lines = hunk
                .lines
                .iter()
                .filter(|line| line.kind != LineKind::Removed)
                .count() as u32;
            assert_eq!(old_lines, hunk.old_len);
            assert_eq!(new_lines, hunk.new_len);
            assert!(hunk.old_len > 0 || hunk.new_len > 0);
        }
    }
}

#[test]
fn rendered_diff_can_be_parsed_back() {
    let mut rng = Rng::new(0x0BAD_F00D_0000_0042);
    for _ in 0..50 {
        let (old, new) = generate_pair(&mut rng);
        if old == new {
            // Nothing to render; the mutation cancelled itself out.
            continue;
        }
        // Full context keeps every line in the hunks, so the render can be
        // compared against the whole file.
        let file_diff = diff_with_context(&old, &new, u32::MAX);
        let text = to_unified(&file_diff, "old", "new");
        let (parsed_old, parsed_new) = parse_unified(&text);
        assert_eq!(parsed_old, old, "rendered:\n{text}\nnew: {new:?}");
        assert_eq!(parsed_new, new, "rendered:\n{text}\nold: {old:?}");
    }
}

/// Minimal unified-diff reader used to prove that `to_unified` is well formed.
fn parse_unified(text: &str) -> (String, String) {
    let mut old: Vec<(String, bool)> = Vec::new();
    let mut new: Vec<(String, bool)> = Vec::new();
    let mut in_hunk = false;
    let mut last_prefix = None;
    for line in text.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line == "\\ No newline at end of file" {
            if last_prefix != Some('+')
                && let Some(entry) = old.last_mut()
            {
                entry.1 = false;
            }
            if last_prefix != Some('-')
                && let Some(entry) = new.last_mut()
            {
                entry.1 = false;
            }
            continue;
        }
        let Some(prefix) = line.chars().next() else {
            continue;
        };
        if !matches!(prefix, ' ' | '+' | '-') {
            continue;
        }
        let content = &line[1..];
        if prefix != '+' {
            old.push((content.to_string(), true));
        }
        if prefix != '-' {
            new.push((content.to_string(), true));
        }
        last_prefix = Some(prefix);
    }
    (join(&old), join(&new))
}

fn join(lines: &[(String, bool)]) -> String {
    let mut out = String::new();
    for (idx, (content, _)) in lines.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(content);
    }
    if lines.last().is_some_and(|(_, has_newline)| *has_newline) {
        out.push('\n');
    }
    out
}

/// Deterministic xorshift so the property test needs no extra dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        assert!(n > 0);
        (self.next_u64() % n as u64) as usize
    }
}

const WORDS: [&str; 10] = [
    "alpha",
    "beta",
    "gamma",
    "x = 1;",
    "if x {",
    "}",
    "  return y;",
    "",
    "foo(bar, baz)",
    "end",
];

fn random_line(rng: &mut Rng) -> String {
    let words = 1 + rng.below(3);
    let mut line = String::new();
    for idx in 0..words {
        if idx > 0 {
            line.push(' ');
        }
        line.push_str(WORDS[rng.below(WORDS.len())]);
    }
    line
}

fn generate_pair(rng: &mut Rng) -> (String, String) {
    let count = rng.below(14);
    let old: Vec<String> = (0..count).map(|_| random_line(rng)).collect();
    let mut new = old.clone();
    for _ in 0..rng.below(5) {
        match rng.below(3) {
            0 => {
                let at = rng.below(new.len() + 1);
                new.insert(at, random_line(rng));
            }
            1 if !new.is_empty() => {
                let at = rng.below(new.len());
                new.remove(at);
            }
            2 if !new.is_empty() => {
                let at = rng.below(new.len());
                new[at] = random_line(rng);
            }
            _ => {}
        }
    }
    let old_trailing = rng.below(4) != 0;
    let new_trailing = rng.below(4) != 0;
    (
        join_lines(&old, old_trailing),
        join_lines(&new, new_trailing),
    )
}

fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut text = lines.join("\n");
    if !lines.is_empty() && trailing_newline {
        text.push('\n');
    }
    text
}

/// Wall-clock test: run with `cargo test --release -p strata-core -- --ignored`.
#[test]
#[ignore = "timing assertion; needs --release to be meaningful"]
fn ten_thousand_lines_with_scattered_changes_are_fast() {
    let mut new_lines: Vec<String> = (0..10_000)
        .map(|idx| format!("fn line_{idx}() {{ let value = {idx}; some_work(value); }}"))
        .collect();
    for step in 0..100 {
        let at = (step * 97 + 13) % new_lines.len();
        new_lines[at] = format!(
            "fn line_{step}() {{ let value = {}; changed_work(value); }}",
            step * 7
        );
    }
    let old: Vec<String> = (0..10_000)
        .map(|idx| format!("fn line_{idx}() {{ let value = {idx}; some_work(value); }}"))
        .collect();
    let old = join_lines(&old, true);
    let new = join_lines(&new_lines, true);

    let mut best = Duration::from_secs(60);
    for _ in 0..3 {
        let started = Instant::now();
        let file_diff = diff_text(old.as_bytes(), new.as_bytes(), &DiffOptions::default());
        best = best.min(started.elapsed());
        assert_eq!(file_diff.hunks.len(), 100);
    }
    assert!(
        best < Duration::from_millis(20),
        "diffing 10k lines took {best:?}"
    );
}
