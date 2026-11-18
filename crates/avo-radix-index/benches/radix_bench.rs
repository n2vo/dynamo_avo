//! Milestone 3 benchmark: The radix tree against a flat hash index, over workloads where
//! every sequence starts with one shared system prompt.
//!
//! - Workloads: `n` sequences of `len` blocks, the first `shared` percent of each identical
//!   across sequences (0, 50, 90), the rest unique. One block per token, so the chained
//!   hashes share exactly the shared prefix.
//! - `build_radix`, `build_flat`: Insert all `n` sequences into an empty index. One element
//!   is one block inserted.
//! - `match_radix`, `match_flat`: Longest-prefix match of one stored sequence, round robin.
//!   One element is one query.
//! - `evict_radix`: Evict every block from a built tree. One element is one block evicted.
//!   The flat index has no structure to evict leaves safely, so it has no counterpart.
//!
//! Each workload also prints index sizes once: blocks and entries for both indexes, plus
//! the tree's node count.

use std::collections::HashMap;
use std::hint::black_box;

use avo_block_pool::BlockId;
use avo_kv_hash::hash_sequence;
use avo_radix_index::{BlockKey, BlockMeta, RadixTree, Tier, WorkerId};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

/// Flat comparison index: One map entry per block, keyed by chained hash. A chained hash
/// names its whole prefix, so a prefix match probes block by block until the first miss.
#[derive(Default)]
struct FlatHashIndex {
    map: HashMap<BlockKey, BlockMeta>,
}

impl FlatHashIndex {
    fn insert(&mut self, seq: &[BlockKey], metas: &[BlockMeta]) {
        for (k, m) in seq.iter().zip(metas) {
            self.map.insert(*k, *m);
        }
    }

    fn match_prefix(&self, query: &[BlockKey]) -> usize {
        query
            .iter()
            .take_while(|k| self.map.contains_key(k))
            .count()
    }
}

/// `(n, len)` pairs: 1,000 sequences of 64 blocks, and 10,000 of 256.
const SIZES: [(usize, usize); 2] = [(1_000, 64), (10_000, 256)];
const SHARED_PERCENT: [usize; 3] = [0, 50, 90];

struct Workload {
    seqs: Vec<Vec<BlockKey>>,
    metas: Vec<Vec<BlockMeta>>,
    blocks: usize,
}

fn workload(n: usize, len: usize, shared_percent: usize) -> Workload {
    let shared = len * shared_percent / 100;
    let mut seqs = Vec::with_capacity(n);
    let mut metas = Vec::with_capacity(n);
    for i in 0..n {
        // Shared tokens are identical; unique tokens start with the sequence index, so
        // sequences diverge at the first unique block.
        let tokens: Vec<u32> = (0..len)
            .map(|j| {
                if j < shared {
                    j as u32
                } else {
                    (i as u32) << 12 | j as u32
                }
            })
            .collect();
        seqs.push(hash_sequence(&tokens, 1, None));
        metas.push(
            (0..len as u32)
                .map(|b| BlockMeta {
                    worker: WorkerId(0),
                    block: BlockId(b),
                    tier: Tier::Tier1Device,
                })
                .collect(),
        );
    }
    // Distinct blocks: the shared prefix once, plus every unique suffix.
    let blocks = shared + n * (len - shared);
    Workload {
        seqs,
        metas,
        blocks,
    }
}

fn build_radix(w: &Workload) -> RadixTree {
    let mut tree = RadixTree::new();
    for (s, m) in w.seqs.iter().zip(&w.metas) {
        tree.insert(s, m).expect("insert");
    }
    tree
}

fn build_flat(w: &Workload) -> FlatHashIndex {
    let mut flat = FlatHashIndex::default();
    for (s, m) in w.seqs.iter().zip(&w.metas) {
        flat.insert(s, m);
    }
    flat
}

fn label(n: usize, len: usize, shared: usize) -> String {
    format!("n{n}_len{len}_shared{shared}")
}

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("radix");
    group.sample_size(10);
    for (n, len) in SIZES {
        for shared in SHARED_PERCENT {
            let w = workload(n, len, shared);
            let id = label(n, len, shared);

            let tree = build_radix(&w);
            let flat = build_flat(&w);
            let s = tree.stats();
            eprintln!(
                "{id}: blocks {} (expected {}), radix nodes {} entries {}, flat entries {}",
                s.blocks,
                w.blocks,
                s.nodes,
                s.blocks + s.nodes - 1,
                flat.map.len()
            );

            group.throughput(Throughput::Elements((n * len) as u64));
            group.bench_function(BenchmarkId::new("build_radix", &id), |b| {
                b.iter(|| black_box(build_radix(&w)))
            });
            group.bench_function(BenchmarkId::new("build_flat", &id), |b| {
                b.iter(|| black_box(build_flat(&w)))
            });

            group.throughput(Throughput::Elements(1));
            let mut tree = tree;
            let mut q = 0;
            group.bench_function(BenchmarkId::new("match_radix", &id), |b| {
                b.iter(|| {
                    q = (q + 1) % n;
                    black_box(tree.match_prefix(&w.seqs[q]).matched)
                })
            });
            let mut q = 0;
            group.bench_function(BenchmarkId::new("match_flat", &id), |b| {
                b.iter(|| {
                    q = (q + 1) % n;
                    black_box(flat.match_prefix(&w.seqs[q]))
                })
            });

            group.throughput(Throughput::Elements(s.blocks as u64));
            group.bench_function(BenchmarkId::new("evict_radix", &id), |b| {
                b.iter_batched(
                    || build_radix(&w),
                    |mut t| {
                        while t.evict().is_ok() {}
                        t
                    },
                    BatchSize::LargeInput,
                )
            });
        }
    }
    group.finish();
}

criterion_group!(radix, benches);
criterion_main!(radix);
