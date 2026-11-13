//! Rough heap usage of loaded history, by part, to decide what to slim down.
//!
//! cargo run -p strata-core --release --example mem_estimate -- <repo>

use std::collections::HashSet;
use std::mem::size_of;

use strata_core::graph::{GraphLayout, GraphRows};
use strata_core::repo::Repo;
use strata_core::{CommitInfo, Oid};

// Typical malloc bookkeeping per allocation.
const ALLOC_OVERHEAD: usize = 16;

fn string_bytes(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.len() + ALLOC_OVERHEAD
    }
}

fn main() -> strata_core::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let repo = Repo::open(&path)?;
    let mut layout = GraphLayout::new();
    let mut rows = GraphRows::new();
    let (mut n, mut commit_inline, mut summary, mut names, mut emails, mut parents) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut unique_people = HashSet::new();
    for commit in repo.walk_commits()? {
        let c = commit?;
        layout.push(c.id, &c.parents, &mut rows);
        n += 1;
        commit_inline += size_of::<CommitInfo>();
        summary += string_bytes(&c.summary);
        names += string_bytes(&c.author_name);
        emails += string_bytes(&c.author_email);
        parents += c.parents.len() * size_of::<Oid>() + ALLOC_OVERHEAD;
        unique_people.insert((c.author_name, c.author_email));
    }
    rows.shrink_to_fit();
    let passes: usize = rows.iter().map(|r| r.passes.len()).sum();
    let edges: usize = rows.iter().map(|r| r.edges.len()).sum();
    let graph = rows.heap_bytes();

    let mb = |b: usize| b as f64 / 1e6;
    let per = |b: usize| b as f64 / n as f64;
    println!(
        "{n} commits, {} unique authors, avg {:.1} pass-throughs and {:.1} edges per row",
        unique_people.len(),
        passes as f64 / n as f64,
        edges as f64 / n as f64
    );
    println!(
        "sizeof CommitInfo={} Oid={}",
        size_of::<CommitInfo>(),
        size_of::<Oid>()
    );
    for (label, b) in [
        ("CommitInfo inline", commit_inline),
        ("summary strings", summary),
        ("author names", names),
        ("author emails", emails),
        ("parents vecs", parents),
        ("graph rows", graph),
    ] {
        println!("{label:>18}: {:7.2} MB  ({:6.1} B/commit)", mb(b), per(b));
    }
    Ok(())
}
