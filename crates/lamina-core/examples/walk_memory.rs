//! Measures steady and peak heap usage, large-allocation churn, RSS, and
//! load/walk time for a repository given by `LAMINA_BENCH_REPO` (or argv[1]).
//!
//! Run in release mode:
//!   LAMINA_BENCH_REPO=/path/to/repo cargo run -p lamina-core --release --example walk_memory

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lamina_core::repo::Repo;
use lamina_core::session::{Session, SessionEvent};

const LARGE_THRESHOLD: usize = 1024 * 1024;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LARGE_ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

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
    if size >= LARGE_THRESHOLD {
        LARGE_ALLOC_BYTES.fetch_add(size, Ordering::Relaxed);
    }
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

fn print_vmmap_summary(count: usize) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let pid = std::process::id().to_string();
    let Ok(out) = std::process::Command::new("vmmap")
        .args(["--summary", &pid])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Physical footprint")
            || trimmed.starts_with("Malloc Large")
            || trimmed.starts_with("Malloc Small")
            || trimmed.starts_with("Malloc Tiny")
            || trimmed.starts_with("mapped file")
        {
            if let Some(rest) = trimmed.strip_prefix("Physical footprint:")
                && let Some(token) = rest.split_whitespace().next()
                && let Some(fp) = parse_size(token)
            {
                let mb = fp as f64 / (1024.0 * 1024.0);
                let per = if count == 0 {
                    0.0
                } else {
                    fp as f64 / count as f64
                };
                println!("  phys_footprint={mb:.1} MB ({per:.1} B/c)");
                continue;
            }
            println!("  vmmap: {trimmed}");
        }
    }
}

fn parse_size(token: &str) -> Option<usize> {
    let (num, mult) = if let Some(s) = token.strip_suffix('G') {
        (s, 1024.0 * 1024.0 * 1024.0)
    } else if let Some(s) = token.strip_suffix('M') {
        (s, 1024.0 * 1024.0)
    } else if let Some(s) = token.strip_suffix('K') {
        (s, 1024.0)
    } else if let Some(s) = token.strip_suffix('B') {
        (s, 1.0)
    } else {
        (token, 1.0)
    };
    let val: f64 = num.parse().ok()?;
    Some((val * mult) as usize)
}

fn main() -> lamina_core::Result<()> {
    let path = std::env::var("LAMINA_BENCH_REPO")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::args().nth(1))
        .unwrap_or_else(|| ".".into());

    let mode = std::env::args().nth(2).unwrap_or_else(|| "all".into());

    if mode == "all" || mode == "walk" {
        let repo = Repo::open(&path)?;
        let baseline = LIVE.load(Ordering::Relaxed);
        PEAK.store(baseline, Ordering::Relaxed);
        LARGE_ALLOC_BYTES.store(0, Ordering::Relaxed);

        let started = Instant::now();
        let mut count = 0usize;
        for commit in repo.walk_commits()? {
            commit?;
            count += 1;
        }
        let elapsed = started.elapsed();
        let live = LIVE.load(Ordering::Relaxed).saturating_sub(baseline);
        let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
        let large = LARGE_ALLOC_BYTES.load(Ordering::Relaxed);
        let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
        let per = |b: usize| {
            if count == 0 {
                0.0
            } else {
                b as f64 / count as f64
            }
        };
        println!(
            "walk_commits: {count} commits in {elapsed:?} ({:.0} commits/s)",
            count as f64 / elapsed.as_secs_f64()
        );
        println!(
            "  walk live={:.1} MB ({:.1} B/c)  peak={:.1} MB ({:.1} B/c)  large_alloc={:.1} MB",
            mb(live),
            per(live),
            mb(peak),
            per(peak),
            mb(large)
        );
    }

    if mode == "all" || mode == "session" {
        let baseline = LIVE.load(Ordering::Relaxed);
        PEAK.store(baseline, Ordering::Relaxed);
        LARGE_ALLOC_BYTES.store(0, Ordering::Relaxed);

        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let session = Session::open(&path, move |event| {
            if matches!(
                event,
                SessionEvent::SnapshotUpdated { .. } | SessionEvent::LoadFailed { .. }
            ) {
                let _ = tx.send(event);
            }
        })?;

        let snapshot = loop {
            match rx.recv_timeout(Duration::from_secs(120)) {
                Ok(SessionEvent::SnapshotUpdated { .. }) => {
                    let snap = session.snapshot();
                    if snap.commits.complete {
                        break snap;
                    }
                }
                Ok(SessionEvent::LoadFailed { error }) => {
                    return Err(lamina_core::Error::other(error));
                }
                Ok(_) => {}
                Err(err) => {
                    return Err(lamina_core::Error::other(format!(
                        "timed out waiting for session load: {err}"
                    )));
                }
            }
        };
        let elapsed = started.elapsed();

        let count = snapshot.commits.len();
        let live = LIVE.load(Ordering::Relaxed).saturating_sub(baseline);
        let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
        let large = LARGE_ALLOC_BYTES.load(Ordering::Relaxed);
        let graph_bytes = snapshot.commits.graph_heap_bytes();
        let rss = rss_bytes();

        let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
        let per = |b: usize| {
            if count == 0 {
                0.0
            } else {
                b as f64 / count as f64
            }
        };
        println!("session_load: {count} commits in {elapsed:?}");
        println!(
            "  steady heap={:.1} MB ({:.1} B/c)  peak heap={:.1} MB ({:.1} B/c)",
            mb(live),
            per(live),
            mb(peak),
            per(peak)
        );
        println!(
            "  graph_rows={:.1} MB ({:.1} B/c)  large_alloc_churn={:.1} MB",
            mb(graph_bytes),
            per(graph_bytes),
            mb(large)
        );
        if let Some(rss) = rss {
            println!("  rss={:.1} MB ({:.1} B/c)", mb(rss), per(rss));
        }
        print_vmmap_summary(count);

        drop(snapshot);
        drop(session);
    }

    Ok(())
}
