//! Japanese repositories end to end: paths, refs, authors, messages, diffs,
//! blame, renames, stashes and legacy encodings.
//!
//! These run against real `git` in a temporary repository so the expected names
//! come from git itself, not from this crate's own parser.

use crate::diff::{DiffOptions, LineKind, diff_bytes};
use crate::encoding::{TextEncoding, decode_for_display};
use crate::git_cli::ApplyMode;
use crate::patch::{BytePatchFile, LineSelection, PatchMode, build_patch_bytes};
use crate::test_support::TestRepo;
use crate::{ChangeKind, HeadInfo, RepoState};

const PATH: &str = "日本語/テスト.txt";
const AUTHOR: &str = "山田 太郎";
const EMAIL: &str = "yamada@example.invalid";
const MESSAGE: &str = "日本語のコミット本文";

#[test]
fn japanese_paths_refs_authors_and_messages_survive_every_read() {
    let root = TestRepo::new();
    root.write(PATH, "一行目\n二行目\n");
    let first = root.commit_as(AUTHOR, EMAIL, MESSAGE);

    root.write(PATH, "一行目\n二行目を変更\n");
    root.write("日本語/追加.txt", "追加\n");
    let status = root.open().status().expect("status");
    let mut paths: Vec<&str> = status
        .unstaged
        .iter()
        .map(|change| change.path.as_str())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, [PATH, "日本語/追加.txt"]);

    // A branch name with Japanese components and a slash is still a ref.
    root.git(&["checkout", "-q", "-b", "機能/検索"]);
    let repo = root.open();
    let refs = repo.refs().expect("refs");
    assert!(
        refs.iter()
            .any(|reference| reference.short_name == "機能/検索"),
        "the branch must keep its Japanese name: {refs:?}"
    );
    match repo.head().expect("head") {
        HeadInfo::Branch { short_name, .. } => assert_eq!(short_name, "機能/検索"),
        other => panic!("expected a branch, got {other:?}"),
    }

    root.write(PATH, "一行目\n二行目を変更\n");
    let second = root.commit_as(AUTHOR, EMAIL, "二度目の日本語コミット");

    // Log: summary and author are the bytes git stored, decoded as UTF-8.
    let commits: Vec<_> = repo
        .walk_commits()
        .expect("walk")
        .collect::<crate::Result<Vec<_>>>()
        .expect("commits");
    let head = commits.first().expect("HEAD commit");
    assert_eq!(head.id, second);
    assert_eq!(head.summary, "二度目の日本語コミット");
    assert_eq!(head.author_name, AUTHOR);
    assert_eq!(head.author_email, EMAIL);
    assert!(commits.iter().any(|commit| commit.id == first));
    assert!(
        repo.commit_message(first)
            .expect("message")
            .contains(MESSAGE)
    );

    // Diff and blame see the real path and content.
    let change = repo
        .commit_changes(second)
        .expect("changes")
        .into_iter()
        .find(|change| change.path == PATH)
        .expect("the Japanese path in the commit");
    assert_eq!(change.kind, ChangeKind::Modified);
    let blamed = repo.blame(second, PATH).expect("blame");
    assert_eq!(blamed.lines, ["一行目", "二行目を変更"]);
    assert!(
        blamed
            .commits
            .values()
            .all(|commit| commit.author_name == AUTHOR),
        "every attributed commit keeps the Japanese author: {:?}",
        blamed.commits
    );
    assert_eq!(blamed.encoding, TextEncoding::UTF_8);

    // The CLI blame path keeps the Japanese name too, not C-style escapes.
    let cli_blame = root
        .git_handle()
        .blame_incremental(second, PATH, &|| false)
        .expect("git blame")
        .expect("not cancelled");
    assert!(
        cli_blame.hunks.iter().all(|hunk| hunk.orig_path == PATH),
        "git blame must report the real path: {:?}",
        cli_blame.hunks
    );

    // File history follows the path.
    let history: Vec<_> = repo
        .path_history(PATH)
        .expect("history")
        .collect::<crate::Result<Vec<_>>>()
        .expect("history entries");
    assert!(history.contains(&first));
    assert!(history.contains(&second));

    // Rename detection reports both names.
    root.git(&["mv", PATH, "日本語/移転.txt"]);
    let renamed = root.commit_as(AUTHOR, EMAIL, "名前を変更");
    let changes = repo.commit_changes(renamed).expect("rename changes");
    let rename = changes
        .iter()
        .find(|change| change.path == "日本語/移転.txt")
        .expect("renamed path");
    assert_eq!(rename.kind, ChangeKind::Renamed);
    assert_eq!(rename.old_path.as_deref(), Some(PATH));

    // Stash messages keep their Japanese text.
    root.write("日本語/移転.txt", "一行目\nさらに変更\n");
    root.git(&["stash", "push", "-q", "-m", "日本語のスタッシュ"]);
    let stashes = repo.stashes().expect("stashes");
    assert_eq!(stashes.len(), 1);
    assert!(stashes[0].message.contains("日本語のスタッシュ"));

    // The CLI stash listing uses the same names.
    let listed = root.git_handle().stash_list().expect("stash list");
    assert_eq!(listed.len(), 1);
    assert!(listed[0].message.contains("日本語のスタッシュ"));
}

#[test]
fn conflicts_on_japanese_paths_are_listed_with_their_names() {
    let root = TestRepo::new();
    root.write(PATH, "共通\n変更前\n");
    root.commit_as(AUTHOR, EMAIL, "base");

    root.git(&["checkout", "-q", "-b", "機能/甲"]);
    root.write(PATH, "共通\n甲の変更\n");
    root.commit_as(AUTHOR, EMAIL, "甲");

    root.git(&["checkout", "-q", "main"]);
    root.write(PATH, "共通\n本流の変更\n");
    root.commit_as(AUTHOR, EMAIL, "本流");
    root.git(&["checkout", "-q", "機能/甲"]);
    let merge = root.git_output(&["merge", "main"]);
    assert!(!merge.status.success(), "the merge must conflict");

    let repo = root.open();
    assert_eq!(repo.state().expect("state"), RepoState::Merging);
    let conflicts = repo.conflicts().expect("conflicts");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, PATH);

    // The CLI's conflict listing is the same set, with NUL separators.
    let output = root
        .git_handle()
        .run(&["ls-files", "--unmerged", "-z"], None)
        .expect("ls-files");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains(PATH), "ls-files must name the path: {text}");
}

#[test]
fn a_shift_jis_hunk_stages_the_original_bytes() {
    let root = TestRepo::new();
    let encoding = TextEncoding::from_label("Shift_JIS").expect("Shift_JIS");
    let before = "設定\n速度は遅い\n終わり\n";
    root.write_bytes("設定.txt", &encoding.encode(before));
    root.commit_as(AUTHOR, EMAIL, "Shift_JIS のファイル");
    let commit = root.head();

    let after = "設定\n速度は速い\n終わり\n";
    root.write_bytes("設定.txt", &encoding.encode(after));

    let repo = root.open();
    let old = repo
        .file_in_index("設定.txt")
        .expect("index")
        .expect("file");
    let new = std::fs::read(root.path().join("設定.txt")).expect("worktree");
    assert_ne!(old, new);
    assert_eq!(
        repo.file_in_worktree_as_git("設定.txt")
            .expect("git form")
            .expect("file"),
        new,
        "without a working-tree-encoding attribute the disk bytes are the git form"
    );

    // Display decodes the legacy bytes and says so.
    let (shown, shown_encoding) = decode_for_display(&new, || Some("Shift_JIS".to_owned()));
    assert_eq!(shown, after);
    assert_eq!(shown_encoding.label(), "Shift_JIS");

    let diff = diff_bytes(&old, &new, &DiffOptions::default());
    let hunk = diff.hunks.first().expect("a changed line");
    assert!(hunk.lines.iter().any(
        |line| line.kind != LineKind::Context && line.content == encoding.encode("速度は速い")
    ));

    let selection = LineSelection::all_of_hunk(
        &super::diff::diff_text(&old, &new, &DiffOptions::default()),
        0,
    );
    let file = BytePatchFile {
        path: "設定.txt",
        old_path: Some("設定.txt"),
        diff: &diff,
        line_ending: crate::patch::LineEnding::Lf,
        is_deleted: false,
    };
    let patch = build_patch_bytes(&file, &selection, PatchMode::Stage).expect("patch");
    assert!(
        patch
            .windows(encoding.encode("速度は速い").len())
            .any(|window| window == encoding.encode("速度は速い")),
        "the patch carries the Shift_JIS bytes, not replacement characters"
    );

    root.git_handle()
        .apply_patch_bytes(&patch, ApplyMode::Stage)
        .expect("git apply accepts the raw-byte patch");
    let staged = repo
        .file_in_index("設定.txt")
        .expect("index")
        .expect("file");
    assert_eq!(staged, new, "staging must not re-encode the file");

    // The commit records the same bytes.
    root.git(&["commit", "-q", "-m", "速度を変更"]);
    let stored = repo
        .file_at_commit(root.head(), "設定.txt")
        .expect("blob")
        .expect("file");
    assert_eq!(stored, new);
    assert_eq!(commit, root.rev("HEAD^1"));
}

#[test]
fn the_working_tree_encoding_attribute_is_read_from_gitattributes() {
    let root = TestRepo::new();
    root.write(
        ".gitattributes",
        "設定.txt working-tree-encoding=Shift_JIS\n",
    );
    let encoding = TextEncoding::from_label("Shift_JIS").expect("Shift_JIS");
    root.write_bytes("設定.txt", &encoding.encode("日本語\n"));
    root.commit_as(AUTHOR, EMAIL, "attributes");

    let repo = root.open();
    assert_eq!(
        repo.working_tree_encoding("設定.txt").as_deref(),
        Some("Shift_JIS")
    );
    assert_eq!(repo.working_tree_encoding("other.txt"), None);
}

#[test]
fn a_utf16_file_with_a_bom_decodes_without_being_called_binary() {
    let root = TestRepo::new();
    let text = "見出し\n本文\n";
    root.write_bytes("UTF-16.txt", &TextEncoding::UTF_16LE.encode(text));
    root.commit_as(AUTHOR, EMAIL, "UTF-16 のファイル");
    let repo = root.open();
    let bytes = repo
        .file_at_commit(root.head(), "UTF-16.txt")
        .expect("blob")
        .expect("file");
    assert!(
        bytes.contains(&0),
        "the UTF-16 code units do contain NUL bytes"
    );
    let (decoded, encoding) = decode_for_display(&bytes, || None);
    assert_eq!(decoded, text);
    assert_eq!(encoding.label(), "UTF-16LE");
}

#[cfg(unix)]
#[test]
fn a_path_that_is_not_utf8_is_listed_without_panicking() {
    use std::os::unix::ffi::OsStrExt as _;

    let root = TestRepo::new();
    root.write("keep.txt", "keep\n");
    root.commit_as(AUTHOR, EMAIL, "base");
    let name = std::ffi::OsStr::from_bytes(b"broken\xFFname.txt");
    // APFS (macOS) refuses names that are not valid UTF-8 with EILSEQ, so such
    // a path can only reach Strata from a filesystem that stores raw bytes.
    if let Err(err) = std::fs::write(root.path().join(name), b"bytes\n") {
        eprintln!("skipping: this filesystem refuses non-UTF-8 names ({err})");
        return;
    }

    let status = root.open().status().expect("status must not panic");
    assert!(
        status
            .unstaged
            .iter()
            .any(|change| change.path.contains('\u{FFFD}') && change.path.contains("name.txt")),
        "the lossy name is still listed: {:?}",
        status.unstaged
    );
}
