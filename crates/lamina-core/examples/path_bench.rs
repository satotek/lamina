//! Benchmarks `Repo::path_history_entries` on a repository given by
//! `LAMINA_BENCH_REPO` (or argv[1]) for one or more paths.
//!
//! Run in release mode:
//!   LAMINA_BENCH_REPO=/path/to/repo cargo run -p lamina-core --release --example path_bench -- MAINTAINERS kernel/sched/core.c COPYING Documentation/process

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use lamina_core::repo::{PathHistoryEntry, Repo};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            record(new_size);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        new
    }
}

fn record(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn rss_bytes() -> Option<usize> {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let kb: usize = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kb * 1024)
}

fn main() -> lamina_core::Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let dump_dir = if let Some(pos) = args.iter().position(|a| a == "--dump") {
        let dir = args.get(pos + 1).cloned();
        args.drain(pos..=(pos + 1).min(args.len() - 1));
        dir
    } else {
        None
    };
    let explicit_commits = if let Some(pos) = args.iter().position(|a| a == "--commits") {
        let n = args.get(pos + 1).and_then(|s| s.parse::<usize>().ok());
        args.drain(pos..=(pos + 1).min(args.len() - 1));
        n
    } else {
        None
    };

    let (repo_path, mut paths) = match std::env::var("LAMINA_BENCH_REPO")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(path) => (path, args),
        None => {
            let repo = args.first().cloned().unwrap_or_else(|| ".".into());
            let rest = if args.len() > 1 {
                args[1..].to_vec()
            } else {
                Vec::new()
            };
            (repo, rest)
        }
    };
    if paths.is_empty() {
        paths = vec![
            "MAINTAINERS".into(),
            "kernel/sched/core.c".into(),
            "COPYING".into(),
            "Documentation/process".into(),
        ];
    }

    let repo = Repo::open(&repo_path)?;
    // Count total reachable commits once to compute commits visited/sec.
    let total_commits = if let Some(n) = explicit_commits {
        n
    } else {
        let started = Instant::now();
        let count = repo.walk_commits()?.count();
        eprintln!(
            "total reachable commits: {count} (walk_commits took {:?})",
            started.elapsed()
        );
        count
    };

    for path in &paths {
        let baseline = LIVE.load(Ordering::Relaxed);
        PEAK.store(baseline, Ordering::Relaxed);

        let started = Instant::now();
        let mut iter = repo.path_history_entries(path)?;
        let setup_elapsed = started.elapsed();

        let mut first_row_elapsed: Option<Duration> = None;
        let mut rows = 0usize;
        let mut hasher = DefaultHasher::new();
        let mut dumped = String::new();

        for item in iter.by_ref() {
            let PathHistoryEntry { commit, path: p } = item?;
            if first_row_elapsed.is_none() {
                first_row_elapsed = Some(started.elapsed());
            }
            commit.hash(&mut hasher);
            p.hash(&mut hasher);
            if dump_dir.is_some() {
                use std::fmt::Write;
                let _ = writeln!(dumped, "{commit}\t{p}");
            }
            rows += 1;
        }
        let total_elapsed = started.elapsed();
        let peak_heap = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
        let rss = rss_bytes().unwrap_or(0);
        let visited_per_sec = total_commits as f64 / total_elapsed.as_secs_f64();

        if let Some(dir) = &dump_dir {
            std::fs::create_dir_all(dir)?;
            let safe = path.replace('/', "__");
            std::fs::write(format!("{dir}/{safe}.txt"), dumped)?;
        }

        println!(
            "path={path} rows={rows} hash={:016x} setup={setup_elapsed:?} first_row={:?} total={total_elapsed:?} visited/s={visited_per_sec:.0} heap_peak={:.1}MB rss={:.1}MB",
            hasher.finish(),
            first_row_elapsed.unwrap_or(total_elapsed),
            peak_heap as f64 / (1024.0 * 1024.0),
            rss as f64 / (1024.0 * 1024.0),
        );
    }

    Ok(())
}
