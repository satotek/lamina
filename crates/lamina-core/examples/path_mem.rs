//! Heap and peak-memory cost of a path-limited history walk, next to a full
//! commit walk. The allocator counter reports the walk's own heap, which
//! `VmHWM` cannot separate from the memory-mapped pack file. RLIMIT-style
//! `VmHWM` is read on Linux only.
//!
//! cargo run -p lamina-core --release --example path_mem -- <repo> [path|walk]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use lamina_core::repo::Repo;

/// Tracks live heap bytes so the walk's own memory can be compared with the
/// process high-water mark, which file-backed pages inflate.
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

fn hwm() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .filter(|line| line.starts_with("VmHWM:") || line.starts_with("VmRSS:"))
        .collect::<Vec<_>>()
        .join("  ")
}

fn main() -> lamina_core::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let target = std::env::args().nth(2).unwrap_or_else(|| "file.txt".into());
    let repo = Repo::open(&path)?;
    let baseline = LIVE.load(Ordering::Relaxed);

    let started = Instant::now();
    let walked = if target == "walk" {
        let mut count = 0usize;
        for commit in repo.walk_commits()? {
            commit?;
            count += 1;
        }
        count
    } else {
        let mut count = 0usize;
        for entry in repo.path_history(&target)? {
            entry?;
            count += 1;
        }
        count
    };

    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    let hwm = hwm();
    println!(
        "{walked} commits in {:?}; walk heap peak {:.1} MB; {}",
        started.elapsed(),
        peak as f64 / (1024.0 * 1024.0),
        if hwm.is_empty() {
            "no /proc/self/status"
        } else {
            &hwm
        }
    );
    Ok(())
}
