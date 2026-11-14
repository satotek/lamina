//! Read-only changed-path Bloom filter lookup for commit-graph files.
//!
//! `gix-commitgraph` does not expose `BIDX`/`BDAT` chunks yet, so this module
//! reads them directly from the same commit-graph files when present. Any
//! missing, truncated (`0xff`), non-v1-ASCII, or unreadable layer conservatively
//! reports `true` ("may contain") so the caller falls back to tree comparison.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use gix::commitgraph::Position;

const CGPH_MAGIC: &[u8; 4] = b"CGPH";
const SEED0: u32 = 0x293a_e76f;
const SEED1: u32 = 0x7e64_6e2c;

#[derive(Clone, Copy)]
pub(crate) struct BloomKeys {
    h0: u32,
    h1: u32,
    has_non_ascii: bool,
}

impl BloomKeys {
    pub(crate) fn new(path: &[u8]) -> Self {
        Self {
            h0: murmur3_seeded(SEED0, path),
            h1: murmur3_seeded(SEED1, path),
            has_non_ascii: path.iter().any(|&b| b >= 0x80),
        }
    }
}

enum LayerData {
    Present {
        hash_version: u8,
        num_hashes: u32,
        bidx: Vec<u8>,
        bdat: Vec<u8>,
    },
    Absent,
}

struct Layer {
    start_pos: u32,
    end_pos: u32,
    data: LayerData,
}

pub(crate) struct CommitGraphBloom {
    layers: Vec<Layer>,
}

impl CommitGraphBloom {
    /// Load changed-path Bloom filters matching `graph`, or `None` if no
    /// usable `BIDX`/`BDAT` chunks exist.
    pub(crate) fn load(repo: &gix::Repository, graph: &gix::commitgraph::Graph) -> Option<Self> {
        let info_dir = repo.objects.store_ref().path().join("info");
        let paths = graph_file_paths(&info_dir)?;
        let hash_len = graph.object_hash().len_in_bytes();
        let total_commits = graph.num_commits();

        let mut layers = Vec::with_capacity(paths.len());
        let mut offset = 0u32;
        let mut any_present = false;

        for path in paths {
            let (layer, next_offset) = load_layer(&path, graph, offset, total_commits, hash_len)?;
            if matches!(layer.data, LayerData::Present { .. }) {
                any_present = true;
            }
            layers.push(layer);
            offset = next_offset;
        }

        if !any_present || offset != total_commits {
            return None;
        }
        Some(Self { layers })
    }

    /// Returns `false` only when the commit's changed-path Bloom filter proves
    /// that `keys`' path was not touched against the commit's first parent.
    pub(crate) fn may_contain(&self, graph_pos: u32, keys: &BloomKeys) -> bool {
        let Some(layer) = self
            .layers
            .iter()
            .find(|l| graph_pos >= l.start_pos && graph_pos < l.end_pos)
        else {
            return true;
        };
        let LayerData::Present {
            hash_version,
            num_hashes,
            bidx,
            bdat,
        } = &layer.data
        else {
            return true;
        };

        // Git's Bloom v1 had a signed-char bug on non-ASCII bytes; fall back to
        // tree comparison whenever v1 encounters bytes >= 0x80.
        if *hash_version == 1 && keys.has_non_ascii {
            return true;
        }

        let local = (graph_pos - layer.start_pos) as usize;
        let start = if local == 0 {
            0
        } else {
            let idx = (local - 1) * 4;
            u32::from_be_bytes(bidx[idx..idx + 4].try_into().expect("4-byte bidx")) as usize
        };
        let end_idx = local * 4;
        let end = u32::from_be_bytes(bidx[end_idx..end_idx + 4].try_into().expect("4-byte bidx"))
            as usize;

        if end <= start || end > bdat.len() {
            return true;
        }
        let filter = &bdat[start..end];
        if filter == [0xff] {
            return true;
        }
        if filter == [0x00] {
            return false;
        }

        let Some(nbits) = (filter.len() as u32).checked_mul(8) else {
            return true;
        };
        if nbits == 0 {
            return true;
        }

        for i in 0..*num_hashes {
            let bit_pos = keys.h0.wrapping_add(i.wrapping_mul(keys.h1)) % nbits;
            let byte = filter[(bit_pos >> 3) as usize];
            if (byte & (1u8 << (bit_pos & 7))) == 0 {
                return false;
            }
        }
        true
    }
}

fn graph_file_paths(info_dir: &Path) -> Option<Vec<PathBuf>> {
    let single = info_dir.join("commit-graph");
    if single.is_file() {
        return Some(vec![single]);
    }
    let graphs_dir = info_dir.join("commit-graphs");
    let chain = graphs_dir.join("commit-graph-chain");
    if !chain.is_file() {
        return None;
    }
    let file = File::open(chain).ok()?;
    let mut paths = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.ok()?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        paths.push(graphs_dir.join(format!("graph-{trimmed}.graph")));
    }
    (!paths.is_empty()).then_some(paths)
}

fn load_layer(
    path: &Path,
    graph: &gix::commitgraph::Graph,
    start_pos: u32,
    total_commits: u32,
    hash_len: usize,
) -> Option<(Layer, u32)> {
    let mut file = File::open(path).ok()?;
    let mut hdr = [0u8; 8];
    file.read_exact(&mut hdr).ok()?;
    if &hdr[0..4] != CGPH_MAGIC {
        return None;
    }
    let file_hash_len = match hdr[4] {
        1 => 20usize,
        2 => 32usize,
        _ => return None,
    };
    // hdr[5] = format version (1)
    if file_hash_len != hash_len || hdr[5] != 1 {
        return None;
    }
    let num_chunks = hdr[6] as usize;
    if num_chunks == 0 {
        return None;
    }

    let mut toc = vec![0u8; (num_chunks + 1) * 12];
    file.read_exact(&mut toc).ok()?;

    let mut oidl = None;
    let mut bidx = None;
    let mut bdat = None;
    for i in 0..num_chunks {
        let entry = &toc[i * 12..(i + 1) * 12];
        let next = &toc[(i + 1) * 12..(i + 2) * 12];
        let chunk_id: [u8; 4] = entry[0..4].try_into().ok()?;
        let start = u64::from_be_bytes(entry[4..12].try_into().ok()?);
        let end = u64::from_be_bytes(next[4..12].try_into().ok()?);
        if end < start {
            return None;
        }
        let len = end - start;
        match &chunk_id {
            b"OIDL" => oidl = Some((start, len)),
            b"BIDX" => bidx = Some((start, len)),
            b"BDAT" => bdat = Some((start, len)),
            _ => {}
        }
    }

    let (oidl_off, oidl_len) = oidl?;
    if oidl_len % (hash_len as u64) != 0 {
        return None;
    }
    let num_commits = u32::try_from(oidl_len / (hash_len as u64)).ok()?;
    let end_pos = start_pos.checked_add(num_commits)?;
    if end_pos > total_commits {
        return None;
    }

    // Verify first and last OID in this layer match the graph loaded by gix.
    if num_commits > 0 {
        let mut oid_buf = vec![0u8; hash_len];
        file.seek(SeekFrom::Start(oidl_off)).ok()?;
        file.read_exact(&mut oid_buf).ok()?;
        if oid_buf.as_slice() != graph.id_at(Position(start_pos)).as_bytes() {
            return None;
        }
        if num_commits > 1 {
            let last_off = oidl_off + u64::from(num_commits - 1) * (hash_len as u64);
            file.seek(SeekFrom::Start(last_off)).ok()?;
            file.read_exact(&mut oid_buf).ok()?;
            if oid_buf.as_slice() != graph.id_at(Position(end_pos - 1)).as_bytes() {
                return None;
            }
        }
    }

    let data = if let (Some((bidx_off, bidx_len)), Some((bdat_off, bdat_len))) = (bidx, bdat)
        && bidx_len == u64::from(num_commits) * 4
        && bdat_len >= 12
    {
        read_bloom_chunks(
            &mut file,
            bidx_off,
            bidx_len as usize,
            bdat_off,
            bdat_len as usize,
        )
        .unwrap_or(LayerData::Absent)
    } else {
        LayerData::Absent
    };

    Some((
        Layer {
            start_pos,
            end_pos,
            data,
        },
        end_pos,
    ))
}

fn read_bloom_chunks(
    file: &mut File,
    bidx_off: u64,
    bidx_len: usize,
    bdat_off: u64,
    bdat_len: usize,
) -> Option<LayerData> {
    let mut bdat_hdr = [0u8; 12];
    file.seek(SeekFrom::Start(bdat_off)).ok()?;
    file.read_exact(&mut bdat_hdr).ok()?;
    let hash_version = u32::from_be_bytes(bdat_hdr[0..4].try_into().ok()?);
    let num_hashes = u32::from_be_bytes(bdat_hdr[4..8].try_into().ok()?);
    let bits_per_entry = u32::from_be_bytes(bdat_hdr[8..12].try_into().ok()?);
    if !matches!(hash_version, 1 | 2) || num_hashes == 0 || bits_per_entry == 0 {
        return None;
    }

    let mut bidx = vec![0u8; bidx_len];
    file.seek(SeekFrom::Start(bidx_off)).ok()?;
    file.read_exact(&mut bidx).ok()?;

    let mut bdat = vec![0u8; bdat_len - 12];
    file.seek(SeekFrom::Start(bdat_off + 12)).ok()?;
    file.read_exact(&mut bdat).ok()?;

    Some(LayerData::Present {
        hash_version: hash_version as u8,
        num_hashes,
        bidx,
        bdat,
    })
}

fn murmur3_seeded(seed: u32, data: &[u8]) -> u32 {
    const C1: u32 = 0xcc9e_2d51;
    const C2: u32 = 0x1b87_3593;
    const R1: u32 = 15;
    const R2: u32 = 13;
    const M: u32 = 5;
    const N: u32 = 0xe654_6b64;

    let mut hash = seed;
    let (chunks, tail) = data.as_chunks::<4>();
    for &chunk in chunks {
        let mut k = u32::from_le_bytes(chunk);
        k = k.wrapping_mul(C1).rotate_left(R1).wrapping_mul(C2);
        hash ^= k;
        hash = hash.rotate_left(R2).wrapping_mul(M).wrapping_add(N);
    }

    if !tail.is_empty() {
        let mut k1 = 0u32;
        for (i, &byte) in tail.iter().enumerate() {
            k1 |= u32::from(byte) << (i * 8);
        }
        k1 = k1.wrapping_mul(C1).rotate_left(R1).wrapping_mul(C2);
        hash ^= k1;
    }

    hash ^= data.len() as u32;
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85eb_ca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2_ae35);
    hash ^= hash >> 16;
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;

    #[test]
    fn bloom_filters_distinguish_changed_empty_and_oversized_commits() {
        let test_repo = TestRepo::new();
        test_repo.write("a.txt", "a\n");
        let c1 = test_repo.commit("add a");
        test_repo.write("dir/sub/b.txt", "b\n");
        let c2 = test_repo.commit("add nested b");
        test_repo.git(&["commit", "--quiet", "--allow-empty", "-m", "empty"]);
        let c3 = test_repo.head();

        // > 512 changed files triggers the 0xff truncated marker.
        for i in 0..550 {
            test_repo.write(&format!("bulk/f{i}.txt"), "x\n");
        }
        let c4 = test_repo.commit("bulk add");

        test_repo.git(&["commit-graph", "write", "--reachable", "--changed-paths"]);

        let repo = test_repo.open();
        let local = repo.local();
        let graph = local
            .commit_graph_if_enabled()
            .expect("commit graph")
            .expect("present");
        let bloom = CommitGraphBloom::load(&local, &graph).expect("bloom loaded");

        let pos = |id| graph.lookup(id).expect("in graph").0;

        // c1 is root: Bloom filters aren't relied upon for roots in history.rs,
        // wait - test c2 against a.txt vs dir/sub/b.txt vs dir.
        assert!(!bloom.may_contain(pos(c2), &BloomKeys::new(b"a.txt")));
        assert!(bloom.may_contain(pos(c2), &BloomKeys::new(b"dir")));
        assert!(bloom.may_contain(pos(c2), &BloomKeys::new(b"dir/sub")));
        assert!(bloom.may_contain(pos(c2), &BloomKeys::new(b"dir/sub/b.txt")));
        assert!(!bloom.may_contain(pos(c2), &BloomKeys::new(b"other.txt")));

        // c3 is empty (0x00): must return false for everything.
        assert!(!bloom.may_contain(pos(c3), &BloomKeys::new(b"a.txt")));
        assert!(!bloom.may_contain(pos(c3), &BloomKeys::new(b"dir/sub/b.txt")));

        // c4 is oversized (0xff): must conservatively return true for any path.
        assert!(bloom.may_contain(pos(c4), &BloomKeys::new(b"unrelated/file.txt")));

        let _ = c1;
    }

    #[test]
    fn split_commit_graph_with_partial_bloom_filters_works() {
        let test_repo = TestRepo::new();
        test_repo.write("first.txt", "1\n");
        let c1 = test_repo.commit("first");
        // Base layer without changed-paths.
        test_repo.git(&["commit-graph", "write", "--reachable"]);

        test_repo.write("second.txt", "2\n");
        let c2 = test_repo.commit("second");
        test_repo.write("third.txt", "3\n");
        let c3 = test_repo.commit("third");
        // Split top layer with changed-paths without merging the base layer.
        test_repo.git(&[
            "commit-graph",
            "write",
            "--reachable",
            "--split=no-merge",
            "--changed-paths",
        ]);

        let repo = test_repo.open();
        let local = repo.local();
        let graph = local
            .commit_graph_if_enabled()
            .expect("commit graph")
            .expect("present");
        let bloom = CommitGraphBloom::load(&local, &graph).expect("bloom loaded");

        let pos = |id| graph.lookup(id).expect("in graph").0;
        // Base layer without BIDX/BDAT must conservatively say true.
        assert!(bloom.may_contain(pos(c1), &BloomKeys::new(b"unrelated.txt")));
        // Top layer has Bloom filters.
        assert!(bloom.may_contain(pos(c3), &BloomKeys::new(b"third.txt")));
        assert!(!bloom.may_contain(pos(c3), &BloomKeys::new(b"first.txt")));
        let _ = c2;
    }
}
