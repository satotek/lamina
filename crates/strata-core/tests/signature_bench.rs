//! Read-only benchmark for commit signature detection and verification.
//!
//! Ignored by default: reads the repository named by `STRATA_BENCH_REPO`.
//!
//! ```text
//! STRATA_BENCH_REPO=/path/to/linux \
//!     cargo test -p strata-core --release --test signature_bench -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use strata_core::git_cli::Git;
use strata_core::repo::Repo;

fn bench_repo() -> Option<PathBuf> {
    std::env::var_os("STRATA_BENCH_REPO")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn millis(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1_000.0
}

#[test]
#[ignore = "perf: reads STRATA_BENCH_REPO"]
fn signature_detection_and_verification_batch() {
    let Some(path) = bench_repo() else {
        eprintln!("STRATA_BENCH_REPO is unset; skipping");
        return;
    };

    let repo = Repo::open(&path).expect("open benchmark repository");
    let walk = repo.walk_commits().expect("walk commits");
    let mut oids = Vec::with_capacity(1_000);
    for commit in walk.take(1_000) {
        oids.push(commit.expect("commit").id);
    }

    for count in [50usize, 200, 1_000] {
        let slice = &oids[..count.min(oids.len())];
        let start = Instant::now();
        let detected = repo
            .commit_signature_formats(slice)
            .expect("detect signature formats");
        let elapsed = start.elapsed();
        let signed_count = detected.iter().filter(|(_, fmt)| fmt.is_some()).count();
        eprintln!(
            "gix detect {count} rows: {:.2} ms ({:.2} us/commit, {signed_count} signed)",
            millis(elapsed),
            elapsed.as_secs_f64() * 1_000_000.0 / (slice.len().max(1) as f64)
        );
    }

    let git = Git::new(path);
    let hints = git.signature_config_hints().expect("signature hints");

    // 1. Verify whatever signed commits exist in the first 50 visible rows.
    let first_50 = repo
        .commit_signature_formats(&oids[..50.min(oids.len())])
        .expect("detect first 50");
    let signed_in_50: Vec<_> = first_50
        .into_iter()
        .filter_map(|(id, fmt)| fmt.map(|f| (id, f)))
        .collect();
    let start = Instant::now();
    let verified_50 = git
        .verify_commit_signatures(&signed_in_50, &hints, &|| false)
        .expect("verify first 50")
        .expect("not cancelled");
    eprintln!(
        "git verify first-50-rows signed subset ({} commits): {:.2} ms",
        verified_50.len(),
        millis(start.elapsed())
    );

    // 2. Collect up to 50 signed commits from recent history and verify as one batch.
    let mut signed_batch = Vec::with_capacity(50);
    for commit in repo.walk_commits().expect("walk commits").take(5_000) {
        let id = commit.expect("commit").id;
        if let Some(fmt) = repo.commit_signature_format(id).expect("detect") {
            signed_batch.push((id, fmt));
            if signed_batch.len() >= 50 {
                break;
            }
        }
    }
    let start = Instant::now();
    let verified_batch = git
        .verify_commit_signatures(&signed_batch, &hints, &|| false)
        .expect("verify batch")
        .expect("not cancelled");
    eprintln!(
        "git verify {}-signed-commit batch: {:.2} ms",
        verified_batch.len(),
        millis(start.elapsed())
    );
}
