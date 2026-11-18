//! Milestone 5 benchmarks.
//!
//! - `router`: `select_worker` latency. `kv_aware` over 8, 32, and 128 workers, 64 and 256
//!   block requests, and 0, 50, and 90 percent of the request already cached on every
//!   worker; `round_robin` and `least_loaded` at 256 blocks and 50 percent.
//! - `router_workload`: One 1,000-request run per policy against simulated workers with
//!   bounded caches and leaf-only LRU eviction. Before timing, each policy's prefill blocks
//!   (unmatched blocks summed over the run) are printed.

use std::collections::{HashMap, VecDeque};

use avo_block_pool::BlockId;
use avo_kv_hash::hash_sequence;
use avo_kv_router::{
    BlockKey, CostWeights, KvEvent, KvRouter, LoadReport, RouterError, RoutingPolicy, Tier,
    WorkerId,
};
use avo_radix_index::{RadixError, RadixTree};
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

const BLOCK_TOKENS: usize = 16;
const CACHED_PER_WORKER: u32 = 4;
const CAPACITY_BLOCKS: u64 = 4096;

fn hashes(tokens: &[u32]) -> Vec<BlockKey> {
    hash_sequence(tokens, BLOCK_TOKENS, None)
}

/// `blocks` blocks of tokens starting at `base`, distinct from any other `base`.
fn run(base: u32, blocks: usize) -> Vec<u32> {
    (0..(blocks * BLOCK_TOKENS) as u32)
        .map(|t| base + t)
        .collect()
}

fn store(r: &mut KvRouter, worker: u32, seq: &[BlockKey]) {
    let event = KvEvent::BlockStored {
        worker: WorkerId(worker),
        seq: seq.to_vec(),
        blocks: (0..seq.len() as u32).map(BlockId).collect(),
        tier: Tier::Tier1Device,
    };
    r.tracker_mut().apply_event(&event).expect("store");
}

fn report(r: &mut KvRouter, worker: u32, active: u32, allocated: u64) {
    let load = LoadReport {
        active_sequences: active,
        queued_tokens: 0,
        allocated_blocks: allocated,
    };
    r.tracker_mut()
        .update_load(WorkerId(worker), load)
        .expect("load");
}

/// A router whose every worker caches `CACHED_PER_WORKER` requests that share the query's
/// first `overlap` percent of blocks and then diverge, at loads spread across workers.
fn populated(
    policy: RoutingPolicy,
    workers: u32,
    blocks: usize,
    overlap: usize,
) -> (KvRouter, Vec<BlockKey>) {
    let shared = blocks * overlap / 100;
    let weights = CostWeights::new(1.0, 64.0).expect("weights");
    let mut r = KvRouter::new(policy, weights);
    let prefix = run(0, shared);
    for w in 0..workers {
        r.tracker_mut()
            .add_worker(WorkerId(w), CAPACITY_BLOCKS)
            .expect("add");
        for s in 0..CACHED_PER_WORKER {
            let base = 100_000_000 + (w * CACHED_PER_WORKER + s) * 100_000;
            let tokens: Vec<u32> = prefix
                .iter()
                .copied()
                .chain(run(base, blocks - shared))
                .collect();
            store(&mut r, w, &hashes(&tokens));
        }
        report(&mut r, w, 1, u64::from(w * 37) % (CAPACITY_BLOCKS / 2));
    }
    let query: Vec<u32> = prefix
        .into_iter()
        .chain(run(50_000_000, blocks - shared))
        .collect();
    (r, hashes(&query))
}

fn select(c: &mut Criterion) {
    let mut group = c.benchmark_group("router");
    for workers in [8, 32, 128] {
        for blocks in [64, 256] {
            for overlap in [0, 50, 90] {
                let (mut r, q) = populated(RoutingPolicy::KvAware, workers, blocks, overlap);
                let id = format!("w{workers}_b{blocks}_o{overlap}");
                group.bench_function(BenchmarkId::new("kv_aware", &id), |b| {
                    b.iter(|| black_box(r.select_worker(black_box(&q)).expect("select")))
                });
            }
        }
        for policy in [RoutingPolicy::RoundRobin, RoutingPolicy::LeastLoaded] {
            let (mut r, q) = populated(policy, workers, 256, 50);
            let id = format!("w{workers}_b256_o50");
            group.bench_function(BenchmarkId::new(policy.as_str(), &id), |b| {
                b.iter(|| black_box(r.select_worker(black_box(&q)).expect("select")))
            });
        }
    }
    group.finish();
}

/// Deterministic xorshift64*, so every policy sees the same request stream.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}

/// Workload: `WORKERS` workers; each request is one of `GROUPS` shared prompts of
/// `PREFIX_BLOCKS` blocks plus `TAIL_BLOCKS` blocks of its own. Each worker's cache holds
/// `CACHE_BLOCKS`, evicting least recently used unpinned leaves; a request's blocks stay
/// pinned and allocated until `ACTIVE` later requests have arrived.
const WORKERS: u32 = 8;
const GROUPS: u64 = 32;
const PREFIX_BLOCKS: usize = 48;
const TAIL_BLOCKS: usize = 16;
const CACHE_BLOCKS: usize = 1024;
const ACTIVE: usize = 32;
const REQUESTS: u32 = 1000;

#[derive(Debug, Default)]
struct Outcome {
    prompt_blocks: usize,
    unmatched_blocks: usize,
    rejected: usize,
    peak_load_ratio: f64,
}

/// Runs the workload under `policy`. Each worker's real cache is a radix tree; every block
/// it stores or evicts reaches the router as an event.
fn workload(policy: RoutingPolicy, weights: CostWeights) -> Outcome {
    let mut r = KvRouter::new(policy, weights);
    let mut caches: Vec<RadixTree> = (0..WORKERS).map(|_| RadixTree::new()).collect();
    let mut allocated = vec![0u64; WORKERS as usize];
    let mut active_count = vec![0u32; WORKERS as usize];
    for w in 0..WORKERS {
        r.tracker_mut()
            .add_worker(WorkerId(w), CACHE_BLOCKS as u64)
            .expect("add");
    }
    let prefixes: Vec<Vec<u32>> = (0..GROUPS as u32)
        .map(|g| run(g * 1_000_000, PREFIX_BLOCKS))
        .collect();
    // Every block hash seen, with the sequence that reaches it, to name evicted blocks.
    let mut paths: HashMap<BlockKey, Vec<BlockKey>> = HashMap::new();
    let mut active: VecDeque<(u32, Vec<BlockKey>)> = VecDeque::new();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out = Outcome::default();

    for i in 0..REQUESTS {
        let g = rng.below(GROUPS) as usize;
        let tail = run(1_000_000_000 + i * 1_000, TAIL_BLOCKS);
        let seq = hashes(&[prefixes[g].as_slice(), tail.as_slice()].concat());
        for (k, key) in seq.iter().enumerate() {
            paths.entry(*key).or_insert_with(|| seq[..=k].to_vec());
        }
        out.prompt_blocks += seq.len();
        match r.select_worker(&seq) {
            Ok(d) => {
                out.unmatched_blocks += d.unmatched_blocks;
                let w = d.target_worker.0;
                let cache = &mut caches[w as usize];
                cache
                    .insert(&seq, &vec![meta(w); seq.len()])
                    .expect("insert");
                cache.pin(&seq).expect("pin");
                store(&mut r, w, &seq);
                allocated[w as usize] += seq.len() as u64;
                active_count[w as usize] += 1;
                report(&mut r, w, active_count[w as usize], allocated[w as usize]);
                active.push_back((w, seq));
                while cache.stats().blocks > CACHE_BLOCKS {
                    match cache.evict() {
                        Ok(ev) => {
                            let removed = KvEvent::BlockRemoved {
                                worker: WorkerId(w),
                                seq: paths[&ev.key].clone(),
                            };
                            r.tracker_mut().apply_event(&removed).expect("remove");
                        }
                        Err(RadixError::AllPinned | RadixError::Empty) => break,
                        Err(e) => panic!("evict: {e}"),
                    }
                }
            }
            Err(RouterError::NoAvailableWorkers) => out.rejected += 1,
            Err(e) => panic!("select: {e}"),
        }
        let peak = r
            .tracker()
            .states()
            .map(|s| s.load_ratio())
            .fold(0.0, f64::max);
        out.peak_load_ratio = out.peak_load_ratio.max(peak);
        if active.len() > ACTIVE
            && let Some((w, seq)) = active.pop_front()
        {
            caches[w as usize].unpin(&seq).expect("unpin");
            allocated[w as usize] -= seq.len() as u64;
            active_count[w as usize] -= 1;
            report(&mut r, w, active_count[w as usize], allocated[w as usize]);
        }
    }
    out
}

fn meta(worker: u32) -> avo_radix_index::BlockMeta {
    avo_radix_index::BlockMeta {
        worker: WorkerId(worker),
        block: BlockId(0),
        tier: Tier::Tier1Device,
    }
}

fn policies() -> [(RoutingPolicy, CostWeights); 4] {
    let w = |p, l| CostWeights::new(p, l).expect("weights");
    [
        (RoutingPolicy::KvAware, w(1.0, 64.0)),
        (RoutingPolicy::KvAware, w(1.0, 0.0)),
        (RoutingPolicy::RoundRobin, w(1.0, 64.0)),
        (RoutingPolicy::LeastLoaded, w(1.0, 64.0)),
    ]
}

fn name(policy: RoutingPolicy, weights: CostWeights) -> String {
    format!("{}_load{}", policy.as_str(), weights.load_weight())
}

fn simulated(c: &mut Criterion) {
    for (policy, weights) in policies() {
        let o = workload(policy, weights);
        println!(
            "workload {}: prefill_blocks {} of {} prompt blocks ({:.1}%), rejected {}, peak_load_ratio {:.3}",
            name(policy, weights),
            o.unmatched_blocks,
            o.prompt_blocks,
            100.0 * o.unmatched_blocks as f64 / o.prompt_blocks as f64,
            o.rejected,
            o.peak_load_ratio
        );
    }
    let mut group = c.benchmark_group("router_workload");
    group.sample_size(10);
    group.throughput(Throughput::Elements(u64::from(REQUESTS)));
    for (policy, weights) in policies() {
        group.bench_function(name(policy, weights), |b| {
            b.iter(|| black_box(workload(policy, weights)))
        });
    }
    group.finish();
}

criterion_group!(benches, select, simulated);
criterion_main!(benches);
