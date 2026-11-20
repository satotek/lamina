//! Times ref enumeration, the commit walk and the layout pass on a real
//! repository.
//!
//! cargo run -p lamina-core --release --example walk_bench -- <repo>

use std::time::Instant;

use lamina_core::graph::{GraphLayout, GraphRows};
use lamina_core::repo::Repo;

fn main() -> lamina_core::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());

    let started = Instant::now();
    let repo = Repo::open(&path)?;
    let open = started.elapsed();

    // Listing refs is a separate UI call, timed here because peeling refs is
    // the other half of what makes the log appear.
    let refs_started = Instant::now();
    let refs = repo.refs()?;
    let list_refs = refs_started.elapsed();

    let setup_started = Instant::now();
    let mut walk = repo.walk_commits()?;
    let setup = setup_started.elapsed();

    let first_started = Instant::now();
    let first = walk.next().transpose()?;
    let first_commit = first_started.elapsed();
    let to_first = started.elapsed();

    let rest_started = Instant::now();
    let mut total = usize::from(first.is_some());
    for commit in walk {
        commit?;
        total += 1;
    }
    let rest = rest_started.elapsed();

    eprintln!("open: {open:?}");
    eprintln!("refs: {} refs in {list_refs:?}", refs.len());
    eprintln!("walk setup (tips + graph): {setup:?}");
    eprintln!("first commit: {first_commit:?} ({to_first:?} from start)");
    eprintln!("remaining {} commits: {rest:?}", total.saturating_sub(1));
    eprintln!(
        "walk total: {:?} ({total} commits)",
        setup + first_commit + rest
    );

    let layout_started = Instant::now();
    let mut layout = GraphLayout::new();
    let mut rows = GraphRows::new();
    for commit in repo.walk_commits()? {
        let commit = commit?;
        layout.push(commit.id, &commit.parents, &mut rows);
    }
    eprintln!("walk + layout: {:?}", layout_started.elapsed());
    Ok(())
}
