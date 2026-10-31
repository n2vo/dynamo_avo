//! Milestone 2 benchmark: The three paths through the block pool, at three capacities.
//!
//! - `alloc_drop`: Allocate and drop with no commit. Free-list pop and push: Expected flat.
//! - `lookup_hit`: Look up a committed inactive block and drop it. Hash index hit; expected flat.
//! - `evict`: Allocate on a pool whose blocks are all inactive, so every allocation evicts the
//!   LRU block, then commit and drop it. Eviction pops the head of the LRU list, so this is
//!   expected flat.

use avo_block_pool::BlockPool;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

const CAPACITIES: [usize; 3] = [1_024, 8_192, 32_768];

fn alloc_drop(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool");
    group.throughput(Throughput::Elements(1));
    for cap in CAPACITIES {
        let pool = BlockPool::new(cap);
        group.bench_with_input(BenchmarkId::new("alloc_drop", cap), &pool, |b, pool| {
            b.iter(|| drop(black_box(pool.allocate().expect("free block"))));
        });
    }
    group.finish();
}

fn lookup_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool");
    group.throughput(Throughput::Elements(1));
    for cap in CAPACITIES {
        let pool = BlockPool::new(cap);
        let h = pool.allocate().expect("free block");
        h.commit(1).expect("commit");
        drop(h);
        group.bench_with_input(BenchmarkId::new("lookup_hit", cap), &pool, |b, pool| {
            b.iter(|| drop(black_box(pool.lookup(black_box(1)).expect("cache hit"))));
        });
    }
    group.finish();
}

fn evict(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool");
    group.throughput(Throughput::Elements(1));
    for cap in CAPACITIES {
        // Fill the pool: Every block committed under a distinct hash, then released,
        // so all blocks are inactive and the free list is empty.
        let pool = BlockPool::new(cap);
        let handles: Vec<_> = (0..cap)
            .map(|i| {
                let h = pool.allocate().expect("free block");
                h.commit(i as u64).expect("commit");
                h
            })
            .collect();
        drop(handles);

        let mut next_hash = cap as u64;
        group.bench_with_input(BenchmarkId::new("evict", cap), &pool, |b, pool| {
            b.iter(|| {
                let h = pool.allocate().expect("evicts the LRU block");
                h.commit(next_hash).expect("commit");
                next_hash += 1;
                drop(black_box(h));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, alloc_drop, lookup_hit, evict);
criterion_main!(benches);
