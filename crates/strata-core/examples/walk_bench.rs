//! Times the commit walk alone, then walk + layout, on a real repository.
//!
//! cargo run -p strata-core --release --example walk_bench -- <repo>

use std::time::Instant;

use strata_core::graph::GraphLayout;
use strata_core::repo::Repo;

fn main() -> strata_core::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let repo = Repo::open(&path)?;

    let started = Instant::now();
    let mut walk = repo.walk_commits()?;
    let first = walk.next().transpose()?;
    let to_first = started.elapsed();
    let rest = walk.count();
    let total = rest + usize::from(first.is_some());
    eprintln!(
        "walk: first commit after {to_first:?}, {total} commits in {:?}",
        started.elapsed()
    );

    let started = Instant::now();
    let mut layout = GraphLayout::new();
    for commit in repo.walk_commits()? {
        let commit = commit?;
        layout.push(commit.id, &commit.parents);
    }
    eprintln!("walk + layout: {:?}", started.elapsed());
    Ok(())
}
