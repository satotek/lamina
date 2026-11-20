//! Benchmarks `Repo::content_search_from_refs` against `git log --format=%H --date-order`
//! on a repository given by `LAMINA_BENCH_REPO` (or argv[1]).
//!
//! Run in release mode:
//!   LAMINA_BENCH_REPO=/path/to/repo cargo run -p lamina-core --release --example content_search_bench

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use lamina_core::Oid;
use lamina_core::repo::{ContentSearchMode, ContentSearchQuery, Repo};

struct Case {
    label: &'static str,
    mode: ContentSearchMode,
    path: Option<&'static str>,
    git_flag: &'static str,
    /// When set, stop `lamina` after scanning this many recent commits and
    /// compare exact prefix equality with `git log -n <lamina_matches>`.
    max_scan: Option<usize>,
}

fn run_git_bench(
    repo: &str,
    git_flag: &str,
    path: Option<&str>,
    max_matches: Option<usize>,
) -> (Vec<Oid>, Option<Duration>, Duration) {
    let start = Instant::now();
    let mut cmd = Command::new("git");
    cmd.args([
        "-C",
        repo,
        "log",
        "--format=%H",
        "--date-order",
        "-E",
        git_flag,
    ]);
    let limit_arg;
    if let Some(n) = max_matches {
        limit_arg = format!("-n{n}");
        cmd.arg(&limit_arg);
    }
    if let Some(p) = path {
        cmd.args(["--", p]);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn git");
    let stdout = child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);
    let mut ids = Vec::new();
    let mut ttfr = None;
    for line in reader.lines() {
        let line = line.expect("read line");
        if line.is_empty() {
            continue;
        }
        if ttfr.is_none() {
            ttfr = Some(start.elapsed());
        }
        ids.push(line.parse::<Oid>().expect("oid"));
    }
    let status = child.wait().expect("wait git");
    assert!(status.success(), "git log failed with {status}");
    (ids, ttfr, start.elapsed())
}

fn main() -> lamina_core::Result<()> {
    let repo_path = std::env::var("LAMINA_BENCH_REPO")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::args().nth(1))
        .unwrap_or_else(|| ".".to_string());

    let cases = [
        Case {
            label: "rare -S with file path (kernel/sched/core.c, full)",
            mode: ContentSearchMode::Substring("sched_core_cookie".into()),
            path: Some("kernel/sched/core.c"),
            git_flag: "-Ssched_core_cookie",
            max_scan: None,
        },
        Case {
            label: "common -S with file path (kernel/sched/core.c, full)",
            mode: ContentSearchMode::Substring("rq_lock".into()),
            path: Some("kernel/sched/core.c"),
            git_flag: "-Srq_lock",
            max_scan: None,
        },
        Case {
            label: "regex -G with file path (kernel/sched/core.c, full)",
            mode: ContentSearchMode::Regex("struct[[:space:]]+sched_entity".into()),
            path: Some("kernel/sched/core.c"),
            git_flag: "-Gstruct[[:space:]]+sched_entity",
            max_scan: None,
        },
        Case {
            label: "rare -S with dir path (kernel/sched, full)",
            mode: ContentSearchMode::Substring("sched_core_cookie".into()),
            path: Some("kernel/sched"),
            git_flag: "-Ssched_core_cookie",
            max_scan: None,
        },
        Case {
            label: "common -S with dir path (mm, full)",
            mode: ContentSearchMode::Substring("folio_put".into()),
            path: Some("mm"),
            git_flag: "-Sfolio_put",
            max_scan: None,
        },
        Case {
            label: "rare -S no path (capped at 200k recent commits)",
            mode: ContentSearchMode::Substring("sched_ext_ops".into()),
            path: None,
            git_flag: "-Ssched_ext_ops",
            max_scan: Some(200_000),
        },
        Case {
            label: "common -S no path (folio_put, capped at 200k recent commits)",
            mode: ContentSearchMode::Substring("folio_put".into()),
            path: None,
            git_flag: "-Sfolio_put",
            max_scan: Some(200_000),
        },
        Case {
            label: "regex -G no path (capped at 200k recent commits)",
            mode: ContentSearchMode::Regex("struct[[:space:]]+sched_ext_ops".into()),
            path: None,
            git_flag: "-Gstruct[[:space:]]+sched_ext_ops",
            max_scan: Some(200_000),
        },
    ];

    let repo = Repo::open(&repo_path)?;
    let refs = vec!["HEAD".to_string()];

    for case in &cases {
        let query = ContentSearchQuery {
            mode: case.mode.clone(),
            path: case.path.map(str::to_string),
        };
        let t0 = Instant::now();
        let mut iter = repo.content_search_from_refs(&query, &refs)?;
        let walk_init = t0.elapsed();
        let total_commits = iter.progress().total;
        let mut ttfr = None;
        let mut got = Vec::new();
        while let Some(item) = iter.next() {
            let entry = item?;
            if ttfr.is_none() {
                ttfr = Some(t0.elapsed());
            }
            got.push(entry.commit);
            if let Some(cap) = case.max_scan
                && iter.progress().scanned >= cap
            {
                break;
            }
        }
        let scanned = iter.progress().scanned;
        let total_time = t0.elapsed();

        let max_matches = case.max_scan.map(|_| got.len());
        let (expected, git_ttfr, git_total) =
            run_git_bench(&repo_path, case.git_flag, case.path, max_matches);
        let exact = got == expected;
        println!(
            "{}:\n  lamina: matches={} scanned={scanned}/{total_commits} init={walk_init:?} ttfr={ttfr:?} total={total_time:?}\n  git:    matches={} ttfr={git_ttfr:?} total={git_total:?}\n  exact_match={exact}",
            case.label,
            got.len(),
            expected.len(),
        );
        if !exact {
            let max_show = 5;
            let extra: Vec<_> = got
                .iter()
                .filter(|id| !expected.contains(id))
                .take(max_show)
                .collect();
            let missing: Vec<_> = expected
                .iter()
                .filter(|id| !got.contains(id))
                .take(max_show)
                .collect();
            println!("  DIFF! extra_in_lamina={extra:?} missing_from_lamina={missing:?}");
        }
    }
    Ok(())
}
