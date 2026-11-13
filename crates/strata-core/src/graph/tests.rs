use super::*;
use std::time::Instant;

fn oid(id: u32) -> Oid {
    Oid::from_hex(format!("{id:040x}").as_bytes()).expect("40 hex digits")
}

/// A row with pass-throughs expanded, so assertions can treat every line the
/// same way regardless of how rows are stored.
#[derive(Debug)]
struct TestRow {
    node_lane: u16,
    node_color: u16,
    width: u16,
    segments: Vec<Segment>,
}

fn layout_rows(commits: &[(u32, &[u32])]) -> GraphRows {
    let mut layout = GraphLayout::new();
    let mut rows = GraphRows::new();
    for (id, parents) in commits {
        let parents: Vec<Oid> = parents.iter().copied().map(oid).collect();
        layout.push(oid(*id), &parents, &mut rows);
    }
    rows
}

fn layout(commits: &[(u32, &[u32])]) -> Vec<TestRow> {
    layout_rows(commits)
        .iter()
        .map(|row| TestRow {
            node_lane: row.node_lane,
            node_color: row.node_color,
            width: row.width,
            segments: expanded_segments(row),
        })
        .collect()
}

fn render(commits: &[(u32, &[u32])]) -> String {
    let rows = layout_rows(commits);
    let labels: Vec<String> = commits.iter().map(|(id, _)| format!("c{id}")).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    render_ascii(&rows, &labels)
}

/// `width` must be one more than the highest lane the row touches.
fn assert_widths(rows: &[TestRow]) {
    for (index, row) in rows.iter().enumerate() {
        let max_lane = row
            .segments
            .iter()
            .flat_map(|segment| [segment.from_lane, segment.to_lane])
            .chain([row.node_lane])
            .max()
            .expect("a row always references its node lane");
        assert_eq!(
            row.width,
            max_lane + 1,
            "row {index} width must cover lane {max_lane}"
        );
    }
}

fn linear_history() -> Vec<(u32, &'static [u32])> {
    vec![(3, &[2][..]), (2, &[1]), (1, &[0]), (0, &[])]
}

#[test]
fn linear_history_keeps_one_lane_and_colour() {
    let rows = layout(&linear_history());

    assert!(rows.iter().all(|row| row.node_lane == 0));
    assert!(rows.iter().all(|row| row.node_color == 0));
    assert!(rows.iter().all(|row| row.width == 1));
    // The first commit has nothing above it; the rest continue straight down.
    assert!(!rows[0].segments.iter().any(|s| s.half == Half::Top));
    assert_eq!(rows[1].segments.len(), 2, "one top, one bottom segment");
    assert!(
        !rows[3].segments.iter().any(|s| s.half == Half::Bottom),
        "root has no bottom segment"
    );
    assert_widths(&rows);

    insta::assert_snapshot!("linear_history", render(&linear_history()));
}

fn branch_and_merge() -> Vec<(u32, &'static [u32])> {
    // m merges a3 and b2; a3 and b2 both descend from a1.
    vec![
        (10, &[11, 12]),
        (11, &[13]),
        (12, &[14]),
        (13, &[14]),
        (14, &[]),
    ]
}

#[test]
fn branch_and_merge_layout() {
    let rows = layout(&branch_and_merge());

    assert_eq!(rows[0].node_lane, 0);
    assert_eq!(rows[2].node_lane, 1);
    assert_eq!(rows[4].node_lane, 0);
    // Both children reserved a lane for a1; the right one bends into the left.
    assert!(rows[4].segments.contains(&Segment {
        from_lane: 1,
        to_lane: 0,
        half: Half::Top,
        color: 1,
    }));
    assert_eq!(rows[4].segments.len(), 2);
    assert_widths(&rows);

    insta::assert_snapshot!("branch_and_merge", render(&branch_and_merge()));
}

fn octopus_merge() -> Vec<(u32, &'static [u32])> {
    vec![
        (10, &[11, 12, 13]),
        (11, &[14]),
        (12, &[14]),
        (13, &[14]),
        (14, &[]),
    ]
}

#[test]
fn octopus_merge_allocates_left_to_right() {
    let rows = layout(&octopus_merge());

    assert_eq!(rows[0].width, 3, "node plus three parents need three lanes");
    for (offset, lane) in [0u16, 1, 2].into_iter().enumerate() {
        assert!(rows[0].segments.contains(&Segment {
            from_lane: 0,
            to_lane: lane,
            half: Half::Bottom,
            color: offset as u16,
        }));
    }
    // The other two parents keep their own lanes and fold back at 14.
    assert!(rows[2].segments.contains(&Segment {
        from_lane: 1,
        to_lane: 1,
        half: Half::Bottom,
        color: 1,
    }));
    assert!(rows[3].segments.contains(&Segment {
        from_lane: 2,
        to_lane: 2,
        half: Half::Bottom,
        color: 2,
    }));
    assert_eq!(rows[4].node_lane, 0);
    for (lane, color) in [(1u16, 1u16), (2, 2)] {
        assert!(rows[4].segments.contains(&Segment {
            from_lane: lane,
            to_lane: 0,
            half: Half::Top,
            color,
        }));
    }
    assert_widths(&rows);

    insta::assert_snapshot!("octopus_merge", render(&octopus_merge()));
}

fn two_roots() -> Vec<(u32, &'static [u32])> {
    // Two unrelated histories interleaved in display order.
    vec![(10, &[12]), (11, &[]), (12, &[])]
}

#[test]
fn two_independent_roots() {
    let rows = layout(&two_roots());

    assert_eq!(rows[0].node_lane, 0);
    assert_eq!(rows[1].node_lane, 1, "root takes the leftmost free lane");
    assert_eq!(
        rows[1].segments.len(),
        2,
        "only the other lane passes through"
    );
    assert_eq!(rows[2].node_lane, 0);
    assert!(rows[1].segments.iter().all(|segment| segment.to_lane == 0));
    assert_widths(&rows);

    insta::assert_snapshot!("two_roots", render(&two_roots()));
}

fn criss_cross_history() -> Vec<(u32, &'static [u32])> {
    // Two merge commits with the same pair of parents: both parents end up
    // with two lanes waiting for them.
    vec![
        (10, &[12, 13]),
        (11, &[12, 13]),
        (12, &[14]),
        (13, &[14]),
        (14, &[]),
    ]
}

#[test]
fn criss_cross_merges() {
    let rows = layout(&criss_cross_history());

    assert_eq!(rows[1].node_lane, 2, "m1 opens a new lane of its own");
    assert_eq!(rows[1].width, 3);
    assert_eq!(rows[2].node_lane, 0, "leftmost waiting lane wins");
    assert!(rows[2].segments.contains(&Segment {
        from_lane: 2,
        to_lane: 0,
        half: Half::Top,
        color: 2,
    }));
    // b's first parent keeps b's lane, so c is now awaited by two lanes.
    assert!(rows[3].segments.contains(&Segment {
        from_lane: 1,
        to_lane: 1,
        half: Half::Bottom,
        color: 1,
    }));
    // Both lanes 0 and 1 waited for c; the right one bends into the node.
    assert!(rows[4].segments.contains(&Segment {
        from_lane: 1,
        to_lane: 0,
        half: Half::Top,
        color: 1,
    }));
    // Every lane that opened is released again at the root.
    assert_eq!(rows[4].segments.len(), 2);
    assert!(
        rows[4]
            .segments
            .iter()
            .all(|segment| segment.half == Half::Top)
    );
    assert_widths(&rows);

    insta::assert_snapshot!("criss_cross_merges", render(&criss_cross_history()));
}

fn long_side_branch() -> Vec<(u32, &'static [u32])> {
    // A feature branch (f0..f5) that merges main in three times; main is
    // pushed after the feature history, as the display order requires.
    vec![
        (100, &[101, 106]),
        (101, &[102]),
        (102, &[103, 108]),
        (103, &[104]),
        (104, &[105, 110]),
        (105, &[111]),
        (106, &[107]),
        (107, &[108]),
        (108, &[109]),
        (109, &[110]),
        (110, &[111]),
        (111, &[]),
    ]
}

#[test]
fn long_running_side_branch_merges_from_main() {
    let rows = layout(&long_side_branch());

    assert_eq!(rows[0].node_lane, 0);
    assert_eq!(rows[0].segments.len(), 2);
    // The side branch keeps lane 0 with its original colour all the way down.
    for row in &rows[..6] {
        assert_eq!(row.node_lane, 0);
        assert_eq!(row.node_color, 0);
    }
    // Merged-in main commits take lanes of their own until they fold back.
    assert_eq!(rows[6].node_lane, 1);
    assert!(rows.iter().all(|row| row.width <= 4));
    assert_widths(&rows);

    insta::assert_snapshot!("long_side_branch", render(&long_side_branch()));
}

fn lane_reuse() -> Vec<(u32, &'static [u32])> {
    vec![
        (10, &[11, 12]),
        (11, &[13]),
        (12, &[]),
        (14, &[15]),
        (13, &[]),
        (15, &[]),
    ]
}

#[test]
fn lane_reuse_after_branch_ends() {
    let rows = layout(&lane_reuse());

    // Lane 1 was reserved for commit 12 and freed when 12 turned out to be a root.
    assert_eq!(rows[2].node_lane, 1);
    assert_eq!(rows[3].node_lane, 1, "new branch reuses the freed lane");
    assert_eq!(rows[3].node_color, 2);
    // The lane still open at 0 stays at 0; nothing shifts left.
    assert_eq!(rows[4].node_lane, 0);
    assert_eq!(rows[5].node_lane, 1);
    assert!(rows[4].segments.iter().any(|segment| segment.to_lane == 1));
    assert_widths(&rows);

    insta::assert_snapshot!("lane_reuse", render(&lane_reuse()));
}

#[test]
fn palette_wraps_after_eight_colours() {
    let mut commits: Vec<(u32, Vec<u32>)> = (0..9).map(|id| (id, vec![100 + id])).collect();
    commits.extend((0..9).map(|offset| (100 + offset, Vec::new())));
    let commits: Vec<(u32, &[u32])> = commits
        .iter()
        .map(|(id, parents)| (*id, parents.as_slice()))
        .collect();

    let rows = layout(&commits);
    let colors: Vec<u16> = rows[..9].iter().map(|row| row.node_color).collect();

    assert_eq!(colors, [0, 1, 2, 3, 4, 5, 6, 7, 0]);
    // Lanes are reused left to right as the roots arrive.
    assert_eq!(
        rows[9..]
            .iter()
            .map(|row| row.node_lane)
            .collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 5, 6, 7, 8]
    );
    assert_widths(&rows);
}

#[test]
fn duplicate_parents_draw_one_edge() {
    let rows = layout(&[(10, &[11, 11]), (11, &[])]);

    let bottoms: Vec<&Segment> = rows[0]
        .segments
        .iter()
        .filter(|segment| segment.half == Half::Bottom)
        .collect();
    assert_eq!(bottoms.len(), 1);
    assert_eq!(rows[0].width, 1);
}

#[test]
#[ignore = "200k commits; run with `cargo test --release -- --ignored`"]
fn perf_200k_commits() {
    const N: u32 = 200_000;

    // Ids are built up front so the measurement only covers the layout itself.
    let ids: Vec<Oid> = (0..N).map(oid).collect();
    let mut layout = GraphLayout::new();
    let mut rows = GraphRows::new();
    let mut parents: Vec<Oid> = Vec::with_capacity(2);

    let start = Instant::now();
    for (index, id) in ids.iter().copied().enumerate() {
        let index = index as u32;
        parents.clear();
        if index + 1 < N {
            parents.push(ids[(index + 1) as usize]);
        }
        // Every seventh commit merges a short side branch back in.
        if index % 7 == 3 && index + 4 < N {
            parents.push(ids[(index + 4) as usize]);
        }
        layout.push(id, &parents, &mut rows);
    }
    let elapsed = start.elapsed();

    assert_eq!(rows.len(), N as usize);
    println!("laid out {N} commits in {elapsed:?}");
    assert!(
        elapsed.as_millis() < 1_000,
        "200k commits took {elapsed:?}, expected well under a second"
    );
}
