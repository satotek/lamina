//! Integration tests for history content search (`-S` and `-G`, with
//! optional path limiting) against `git log` in isolated `TempDir`
//! repositories, plus `Session::content_search_streaming`.

use std::fs;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use lamina_core::Oid;
use lamina_core::repo::{ContentSearchMode, ContentSearchQuery, Repo};
use lamina_core::session::{HistoryScope, QueryTicket, Session, SessionEvent};
use lamina_core::test_support::TestRepo;

const BASE_TIME: i64 = 1_700_000_000;
const TIMEOUT: Duration = Duration::from_secs(60);

/// The shared fixture plus a clock that advances with every command, so
/// commits land in a known order.
struct Fixture {
    repo: TestRepo,
    next_time: i64,
}

impl Fixture {
    fn new() -> Self {
        Self {
            repo: TestRepo::with_time(BASE_TIME),
            next_time: BASE_TIME,
        }
    }

    fn path(&self) -> &Path {
        self.repo.path()
    }

    fn git(&mut self, args: &[&str]) -> String {
        let date = self.next_time;
        self.next_time += 10;
        self.git_at(date, args)
    }

    fn git_at(&self, date: i64, args: &[&str]) -> String {
        self.repo.git_dated(date, args).trim_end().to_owned()
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        self.repo.write_bytes(rel, bytes);
    }

    fn commit_all(&mut self, msg: &str) -> Oid {
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "--allow-empty", "-m", msg]);
        self.head()
    }

    fn head(&self) -> Oid {
        self.git_at(self.next_time, &["rev-parse", "HEAD"])
            .parse()
            .expect("head oid")
    }

    fn git_log_ids(&self, args: &[&str]) -> Vec<Oid> {
        let mut full = vec!["log", "--format=%H", "--date-order"];
        full.extend_from_slice(args);
        let out = self.git_at(self.next_time, &full);
        out.lines()
            .filter(|line| !line.is_empty())
            .map(|line| line.parse().expect("valid oid"))
            .collect()
    }
}

fn open_session(path: &Path) -> (Session, Receiver<SessionEvent>) {
    let (tx, rx) = mpsc::channel();
    let session = Session::open(path, move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");
    (session, rx)
}

fn collect_session_content_search(
    session: &Session,
    events: &Receiver<SessionEvent>,
    query: ContentSearchQuery,
) -> (Vec<Oid>, Vec<Option<String>>, usize, usize, bool) {
    let ticket = session.content_search_streaming(query);
    let mut ids = Vec::new();
    let mut paths = Vec::new();
    let mut saw_progress = false;
    let mut last_scanned = 0usize;
    let mut expected_total = None;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(rem) {
            Ok(SessionEvent::ContentSearchBatch {
                ticket: t,
                matches,
                scanned,
                total,
            }) if t == ticket => {
                assert!(scanned >= last_scanned, "progress must be monotonic");
                if let Some(prev_total) = expected_total {
                    assert_eq!(total, prev_total);
                }
                expected_total = Some(total);
                last_scanned = scanned;
                for m in matches.iter() {
                    ids.push(m.commit);
                    paths.push(m.matched_path.clone());
                }
            }
            Ok(SessionEvent::ContentSearchProgress {
                ticket: t,
                scanned,
                total,
            }) if t == ticket => {
                saw_progress = true;
                assert!(scanned >= last_scanned, "progress must be monotonic");
                if let Some(prev_total) = expected_total {
                    assert_eq!(total, prev_total);
                }
                expected_total = Some(total);
                last_scanned = scanned;
            }
            Ok(SessionEvent::ContentSearchDone {
                ticket: t,
                scanned,
                total,
            }) if t == ticket => {
                assert!(scanned >= last_scanned);
                assert_eq!(scanned, total, "done reports full scan count");
                return (ids, paths, scanned, total, saw_progress);
            }
            Ok(SessionEvent::QueryFailed { ticket: t, error }) if t == ticket => {
                panic!("content search failed unexpectedly: {error}");
            }
            Ok(_) => {}
            Err(err) => panic!("timed out waiting for content search {ticket:?}: {err}"),
        }
    }
}

#[test]
fn substring_search_tracks_occurrence_count_changes_not_mere_presence() {
    let mut fx = Fixture::new();

    // c1: introduces 1 occurrence in src/a.txt.
    fx.write("src/a.txt", b"alpha\nneedle one\nomega\n");
    let c1 = fx.commit_all("c1 add 1 needle");

    // c2: edits unrelated line; count stays 1 -> MUST NOT match -S.
    fx.write("src/a.txt", b"alpha changed\nneedle one\nomega\n");
    let _c2 = fx.commit_all("c2 unchanged count");

    // c3: increases count from 1 to 2 -> MUST match -S.
    fx.write(
        "src/a.txt",
        b"alpha changed\nneedle one\nneedle two\nomega\n",
    );
    let c3 = fx.commit_all("c3 count 1->2");

    // c4: moves needle to another line in same file (still 2 occurrences) -> MUST NOT match -S.
    fx.write(
        "src/a.txt",
        b"needle one\nalpha changed\nomega\nneedle two\n",
    );
    let _c4 = fx.commit_all("c4 move line same count");

    // c5: unrelated file, plus same-count change -> MUST NOT match -S.
    fx.write("src/b.txt", b"no match here\n");
    let _c5 = fx.commit_all("c5 unrelated");

    // c6: deletes all occurrences in a.txt -> MUST match -S.
    fx.write("src/a.txt", b"alpha changed\nomega\n");
    let c6 = fx.commit_all("c6 remove needles");

    let repo = Repo::open(fx.path()).expect("open");
    let query = ContentSearchQuery {
        mode: ContentSearchMode::Substring("needle".into()),
        path: None,
    };
    let got: Vec<Oid> = repo
        .content_search(&query)
        .expect("search")
        .map(|e| e.expect("entry").commit)
        .collect();
    let expected = fx.git_log_ids(&["-Sneedle"]);
    assert_eq!(got, vec![c6, c3, c1]);
    assert_eq!(got, expected);
}

#[test]
fn regex_search_matches_added_or_removed_lines_and_respects_crlf_and_anchors() {
    let mut fx = Fixture::new();

    // c1: adds a matching line and a context line.
    fx.write("src/lib.rs", b"fn context_match() {}\nfn keep() {}\n");
    let c1 = fx.commit_all("c1 add context_match");

    // c2: modifies an unrelated line; `context_match` stays unchanged -> MUST NOT match -G.
    fx.write(
        "src/lib.rs",
        b"fn context_match() {}\nfn changed_other() {}\n",
    );
    let _c2 = fx.commit_all("c2 context unchanged");

    // c3: modifies the `context_match` line -> MUST match -G.
    fx.write(
        "src/lib.rs",
        b"fn context_match_v2() {}\nfn changed_other() {}\n",
    );
    let c3 = fx.commit_all("c3 edit matching line");

    // c4: adds CRLF line `exact\r\n` -> `^exact$` must NOT match because git strips only `\n`.
    fx.write("src/crlf.txt", b"exact\r\n");
    let _c4 = fx.commit_all("c4 add crlf exact");

    // c5: adds LF line `exact\n` -> `^exact$` MUST match.
    fx.write("src/lf.txt", b"exact\n");
    let c5 = fx.commit_all("c5 add lf exact");

    let repo = Repo::open(fx.path()).expect("open");

    let q1 = ContentSearchQuery {
        mode: ContentSearchMode::Regex("context_match".into()),
        path: None,
    };
    let got1: Vec<Oid> = repo
        .content_search(&q1)
        .expect("search")
        .map(|e| e.expect("entry").commit)
        .collect();
    assert_eq!(got1, vec![c3, c1]);
    assert_eq!(got1, fx.git_log_ids(&["-Gcontext_match"]));

    let q2 = ContentSearchQuery {
        mode: ContentSearchMode::Regex("^exact$".into()),
        path: None,
    };
    let got2: Vec<Oid> = repo
        .content_search(&q2)
        .expect("search")
        .map(|e| e.expect("entry").commit)
        .collect();
    assert_eq!(got2, vec![c5]);
    assert_eq!(got2, fx.git_log_ids(&["-G^exact$"]));
}

#[test]
fn merges_are_skipped_without_m_and_renames_follow_git_semantics() {
    let mut fx = Fixture::new();

    fx.write("dir/old.txt", b"base\ntoken_x\n");
    let c1 = fx.commit_all("c1 base");

    // Pure rename R100 without changing count -> neither -S nor -G matches.
    fx.git(&["mv", "dir/old.txt", "dir/new.txt"]);
    let _c_pure_rename = fx.commit_all("c2 pure rename");

    // Rename + edit adding a second token_x -> both -S and -G match.
    fx.write("dir/moved.txt", b"base\ntoken_x\ntoken_x added_line\n");
    fs::remove_file(fx.path().join("dir/new.txt")).expect("remove old");
    let c_rename_edit = fx.commit_all("c3 rename and edit");

    // Branch off c1, commit another file, and create an evil merge that adds `token_x` only in the merge commit.
    fx.git(&["checkout", "-q", "-b", "topic", &c1.to_string()]);
    fx.write("side.txt", b"side\n");
    let _c_topic = fx.commit_all("c4 topic");
    fx.git(&["checkout", "-q", "main"]);
    fx.git(&["merge", "--no-ff", "--no-commit", "topic"]);
    fx.write("evil.txt", b"token_x introduced in merge\n");
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "--quiet", "-m", "c5 evil merge"]);

    let repo = Repo::open(fx.path()).expect("open");
    for (mode, git_arg) in [
        (ContentSearchMode::Substring("token_x".into()), "-Stoken_x"),
        (ContentSearchMode::Regex("token_x".into()), "-Gtoken_x"),
    ] {
        let q = ContentSearchQuery { mode, path: None };
        let got: Vec<Oid> = repo
            .content_search(&q)
            .expect("search")
            .map(|e| e.expect("entry").commit)
            .collect();
        let expected = fx.git_log_ids(&["--all", git_arg]);
        assert_eq!(got, vec![c_rename_edit, c1]);
        assert_eq!(got, expected, "mismatch for {git_arg}");
    }
}

#[test]
fn binary_and_oversized_blobs_are_skipped() {
    let mut fx = Fixture::new();

    // Text commit that should match.
    fx.write("text.txt", b"hello target_sym\n");
    let c_text = fx.commit_all("c1 text match");

    // Binary commit containing NUL byte within first 8000 bytes + target_sym.
    fx.write("bin.dat", b"\x00\x01\x02target_sym\x03");
    let _c_bin = fx.commit_all("c2 binary skipped");

    // Oversized blob (> 4 MiB MAX_STAT_BYTES) containing target_sym.
    let mut big = vec![b'a'; 4 * 1024 * 1024 + 128];
    big.extend_from_slice(b"\ntarget_sym\n");
    fx.write("huge.txt", &big);
    let _c_big = fx.commit_all("c3 huge skipped");

    let repo = Repo::open(fx.path()).expect("open");
    for mode in [
        ContentSearchMode::Substring("target_sym".into()),
        ContentSearchMode::Regex("target_sym".into()),
    ] {
        let q = ContentSearchQuery { mode, path: None };
        let got: Vec<Oid> = repo
            .content_search(&q)
            .expect("search")
            .map(|e| e.expect("entry").commit)
            .collect();
        assert_eq!(got, vec![c_text]);
    }

    // Confirm git's default -G skips binary files too (git's -S scans raw
    // binary bytes, whereas Lamina intentionally skips binary and over-cap blobs).
    assert_eq!(
        fx.git_log_ids(&["-Gtarget_sym", "--", "bin.dat"]),
        Vec::<Oid>::new()
    );
}

#[test]
fn path_limiting_works_with_and_without_commit_graph_bloom_filters() {
    let mut fx = Fixture::new();

    fx.write("sub/keep.rs", b"let hit_sym = 1;\n");
    fx.write("other/ignore.rs", b"let hit_sym = 1;\n");
    let c1 = fx.commit_all("c1 both");

    fx.write("other/ignore.rs", b"let hit_sym = 2;\nlet hit_sym = 3;\n");
    let c2 = fx.commit_all("c2 only other");

    fx.write("sub/nested/deep.rs", b"fn hit_sym() {}\n");
    let c3 = fx.commit_all("c3 sub nested");

    fx.write("sub/keep.rs", b"let hit_sym = 1;\nlet hit_sym = 2;\n");
    let c4 = fx.commit_all("c4 sub keep");

    let check_all = |fx: &Fixture, label: &str| {
        let repo = Repo::open(fx.path()).expect("open");

        let q_sub = ContentSearchQuery {
            mode: ContentSearchMode::Substring("hit_sym".into()),
            path: Some("./sub/".into()),
        };
        let got_sub: Vec<Oid> = repo
            .content_search(&q_sub)
            .expect("search")
            .map(|e| e.expect("entry").commit)
            .collect();
        assert_eq!(got_sub, vec![c4, c3, c1], "{label} -S sub");
        assert_eq!(got_sub, fx.git_log_ids(&["-Shit_sym", "--", "sub"]));

        let q_file = ContentSearchQuery {
            mode: ContentSearchMode::Regex("hit_sym".into()),
            path: Some("sub/keep.rs".into()),
        };
        let got_file: Vec<Oid> = repo
            .content_search(&q_file)
            .expect("search")
            .map(|e| e.expect("entry").commit)
            .collect();
        assert_eq!(got_file, vec![c4, c1], "{label} -G sub/keep.rs");
        assert_eq!(
            got_file,
            fx.git_log_ids(&["-Ghit_sym", "--", "sub/keep.rs"])
        );

        let q_other = ContentSearchQuery {
            mode: ContentSearchMode::Substring("hit_sym".into()),
            path: Some("other/ignore.rs".into()),
        };
        let got_other: Vec<Oid> = repo
            .content_search(&q_other)
            .expect("search")
            .map(|e| e.expect("entry").commit)
            .collect();
        assert_eq!(got_other, vec![c2, c1], "{label} -S other/ignore.rs");
    };

    check_all(&fx, "without commit-graph");

    fx.git(&["commit-graph", "write", "--reachable", "--changed-paths"]);
    check_all(&fx, "with commit-graph bloom");
}

#[test]
fn session_streams_content_search_batches_progress_and_respects_scope_and_cancel() {
    let mut fx = Fixture::new();

    fx.write("a.txt", b"common_term\n");
    let c1 = fx.commit_all("c1");

    fx.git(&["checkout", "-q", "-b", "side"]);
    fx.write("b.txt", b"side_only_term\ncommon_term\n");
    let _c_side = fx.commit_all("c_side");

    fx.git(&["checkout", "-q", "main"]);
    for i in 0..10 {
        fx.write("noise.txt", format!("noise {i}\n").as_bytes());
        fx.commit_all(&format!("noise {i}"));
    }
    fx.write("a.txt", b"common_term\ncommon_term\n");
    let c_main_top = fx.commit_all("c_main_top");

    let (session, events) = open_session(fx.path());

    // Default HistoryScope::All sees side branch commit too.
    let (ids_all, paths_all, scanned, total, _saw_progress) = collect_session_content_search(
        &session,
        &events,
        ContentSearchQuery {
            mode: ContentSearchMode::Substring("side_only_term".into()),
            path: None,
        },
    );
    assert_eq!(ids_all.len(), 1);
    assert_eq!(paths_all[0].as_deref(), Some("b.txt"));
    assert_eq!(scanned, total);
    assert_eq!(total, 13);

    // Restrict scope to main: side_only_term has 0 matches across 12 commits.
    session.set_history_scope(HistoryScope::Refs(vec!["refs/heads/main".into()]));
    let (ids_side_on_main, _, scanned_main, total_main, _) = collect_session_content_search(
        &session,
        &events,
        ContentSearchQuery {
            mode: ContentSearchMode::Substring("side_only_term".into()),
            path: None,
        },
    );
    assert!(ids_side_on_main.is_empty());
    assert_eq!(scanned_main, 12);
    assert_eq!(total_main, 12);

    // Regex search on main finds c_main_top and c1 in date order.
    let (ids_main_common, paths_main_common, _, _, _) = collect_session_content_search(
        &session,
        &events,
        ContentSearchQuery {
            mode: ContentSearchMode::Regex("^common_term$".into()),
            path: None,
        },
    );
    assert_eq!(ids_main_common, vec![c_main_top, c1]);
    assert_eq!(
        paths_main_common,
        vec![Some("a.txt".into()), Some("a.txt".into())]
    );

    // Invalid regex emits QueryFailed for that ticket.
    let bad_ticket = session.content_search_streaming(ContentSearchQuery {
        mode: ContentSearchMode::Regex("[unclosed".into()),
        path: None,
    });
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(rem) {
            Ok(SessionEvent::QueryFailed { ticket, error }) if ticket == bad_ticket => {
                assert!(error.contains("invalid regular expression"));
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("expected QueryFailed for invalid regex: {err}"),
        }
    }

    // Immediate cancellation of a queued ticket suppresses Done/Batch for it.
    let cancelled_ticket: QueryTicket = session.content_search_streaming(ContentSearchQuery {
        mode: ContentSearchMode::Substring("common_term".into()),
        path: None,
    });
    session.cancel_query(cancelled_ticket);
    std::thread::sleep(Duration::from_millis(150));
    while let Ok(ev) = events.try_recv() {
        match ev {
            SessionEvent::ContentSearchBatch { ticket, .. }
            | SessionEvent::ContentSearchProgress { ticket, .. }
            | SessionEvent::ContentSearchDone { ticket, .. }
                if ticket == cancelled_ticket =>
            {
                panic!("cancelled ticket must not emit results/done");
            }
            _ => {}
        }
    }
}
