//! Renders the commit graph of a repository to SVG, to eyeball layout quality
//! on real histories without the GUI.
//!
//! cargo run -p strata-core --release --example graph_svg -- <repo> [rows] > graph.svg

use std::fmt::Write as _;
use std::time::Instant;

use strata_core::graph::{GraphLayout, Half};
use strata_core::repo::Repo;

const ROW_H: f32 = 22.0;
const LANE_W: f32 = 14.0;
const PALETTE: [&str; 8] = [
    "#4e79a7", "#f28e2b", "#59a14f", "#e15759", "#76b7b2", "#edc948", "#b07aa1", "#9c755f",
];

fn main() -> strata_core::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| ".".into());
    let limit: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(300);

    let repo = Repo::open(&path)?;
    let started = Instant::now();
    let mut layout = GraphLayout::new();
    let mut summaries = Vec::new();
    let mut total = 0usize;
    for commit in repo.walk_commits()? {
        let commit = commit?;
        layout.push(commit.id, &commit.parents);
        if summaries.len() < limit {
            summaries.push(commit.summary);
        }
        total += 1;
    }
    eprintln!(
        "{total} commits walked and laid out in {:?}",
        started.elapsed()
    );

    let rows = &layout.rows()[..summaries.len()];
    let max_width = rows.iter().map(|r| r.width).max().unwrap_or(1) as f32;
    let text_x = max_width * LANE_W + 12.0;
    let height = rows.len() as f32 * ROW_H;
    let x = |lane: u16| lane as f32 * LANE_W + LANE_W / 2.0;

    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{height}" font-family="sans-serif" font-size="12"><rect width="100%" height="100%" fill="white"/>"#,
        text_x + 600.0
    );
    for (i, (row, summary)) in rows.iter().zip(&summaries).enumerate() {
        let top = i as f32 * ROW_H;
        let mid = top + ROW_H / 2.0;
        for seg in &row.segments {
            let (y0, y1) = match seg.half {
                Half::Top => (top, mid),
                Half::Bottom => (mid, top + ROW_H),
            };
            let (x0, x1) = (x(seg.from_lane), x(seg.to_lane));
            let ym = (y0 + y1) / 2.0;
            let color = PALETTE[seg.color as usize % PALETTE.len()];
            let _ = writeln!(
                svg,
                r#"<path d="M{x0} {y0} C{x0} {ym} {x1} {ym} {x1} {y1}" stroke="{color}" stroke-width="2" fill="none"/>"#
            );
        }
        let color = PALETTE[row.node_color as usize % PALETTE.len()];
        let _ = writeln!(
            svg,
            r#"<circle cx="{}" cy="{mid}" r="4" fill="{color}" stroke="white"/>"#,
            x(row.node_lane)
        );
        let escaped = summary
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let _ = writeln!(
            svg,
            r#"<text x="{text_x}" y="{}">{escaped}</text>"#,
            mid + 4.0
        );
    }
    svg.push_str("</svg>\n");
    print!("{svg}");
    Ok(())
}
