//! Timings for the first-parent tree diff a commit selection needs.
//!
//! Ignored by default: it reads a real repository named by `STRATA_BENCH_REPO`
//! and measures whatever is on disk. Run it in release mode:
//!
//! ```text
//! STRATA_BENCH_REPO=/path/to/linux \
//!     cargo test -p strata-core --release --test commit_diff_bench -- --ignored --nocapture
//! ```
//!
//! `STRATA_BENCH_COMMIT` selects the commit and defaults to the huge merge this
//! harness was written for. The repository is only read.

use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use strata_core::Oid;
use strata_core::repo::Repo;

/// The merge whose file list and line counts are the slow case: 2066 files
/// against the first parent.
const DEFAULT_COMMIT: &str = "734bef548a96e8774d6eef5dfa2026c6edc4e80b";

fn bench_repo() -> Option<PathBuf> {
    std::env::var_os("STRATA_BENCH_REPO")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn bench_commit() -> Oid {
    std::env::var("STRATA_BENCH_COMMIT")
        .unwrap_or_else(|_| DEFAULT_COMMIT.to_owned())
        .parse()
        .expect("STRATA_BENCH_COMMIT must be a commit id")
}

fn millis(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1_000.0
}

fn report(label: &str, stat_ms: f64, files_ms: f64, files: usize) {
    eprintln!("{label}: stat {stat_ms:.1} ms, files {files_ms:.1} ms ({files} files)");
}

#[test]
#[ignore = "perf: reads STRATA_BENCH_REPO"]
fn merge_commit_selection_tree_diff() {
    let Some(path) = bench_repo() else {
        eprintln!("STRATA_BENCH_REPO is unset; skipping");
        return;
    };
    let commit = bench_commit();

    // A fresh handle per order, so the previous order's cache cannot hide the
    // work the second one would do.
    let repo = Repo::open(&path).expect("open benchmark repository");
    let start = Instant::now();
    let stat = repo.commit_stat(commit).expect("commit stat");
    let stat_first = millis(start.elapsed());
    let start = Instant::now();
    let files = repo.commit_changes(commit).expect("commit changes");
    let files_second = millis(start.elapsed());
    report("stat then files", stat_first, files_second, files.len());

    let repo = Repo::open(&path).expect("open benchmark repository");
    let start = Instant::now();
    let files_first = repo.commit_changes(commit).expect("commit changes");
    let files_ms = millis(start.elapsed());
    let start = Instant::now();
    let stat_second = repo.commit_stat(commit).expect("commit stat");
    let stat_ms = millis(start.elapsed());
    report("files then stat", stat_ms, files_ms, files_first.len());

    // Both orders must report the same commit.
    assert_eq!(stat, stat_second, "the stats of both orders differ");
    assert_eq!(files, files_first, "the file lists of both orders differ");

    if let Some(dump_dir) = std::env::var_os("STRATA_BENCH_DUMP")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        use std::fmt::Write as _;
        std::fs::create_dir_all(&dump_dir).expect("create STRATA_BENCH_DUMP dir");
        let mut out = format!(
            "added={} removed={} files={}\n",
            stat.added,
            stat.removed,
            stat.files.len()
        );
        for file in &stat.files {
            let _ = writeln!(
                out,
                "{}\t{}\t{}\t{}\t{}",
                file.path,
                file.old_path.as_deref().unwrap_or(""),
                file.added,
                file.removed,
                file.binary
            );
        }
        std::fs::write(dump_dir.join(format!("{commit}.stat.txt")), out).expect("write stat dump");
    }

    // The selection path asks for both at once; the second caller should wait
    // for the tree diff instead of running it again.
    let repo = Repo::open(&path).expect("open benchmark repository");
    let barrier = Arc::new(Barrier::new(2));
    let stat_thread = {
        let repo = repo.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            let start = Instant::now();
            let stat = repo.commit_stat(commit).expect("commit stat");
            (millis(start.elapsed()), stat)
        })
    };
    let files_thread = {
        let repo = repo.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            let start = Instant::now();
            let files = repo.commit_changes(commit).expect("commit changes");
            (millis(start.elapsed()), files)
        })
    };
    let (concurrent_stat_ms, concurrent_stat) = stat_thread.join().expect("stat thread");
    let (concurrent_files_ms, concurrent_files) = files_thread.join().expect("files thread");
    report(
        "both at once",
        concurrent_stat_ms,
        concurrent_files_ms,
        concurrent_files.len(),
    );
    assert_eq!(
        stat, concurrent_stat,
        "the stat of the concurrent run differs"
    );
    assert_eq!(
        files, concurrent_files,
        "the file list of the concurrent run differs"
    );
}
