//! Contention Benchmark: Pool throughput as threads are added, with every operation using
//! the pool's single `Mutex`.
//!
//! - `churn`: Each thread repeatedly allocates (evicting once the pool fills), commits
//!   under a thread-unique hash, and drops. Every step acquires the lock.
//! - `lookup`: Each thread repeatedly looks up one of 1,024 shared committed blocks and
//!   drops the handle. Read-mostly in intent but each lookup and drop still acquires the lock.
//! - `churn_batch8`, `churn_batch32`: `churn` in batches of 8 or 32 blocks. One
//!   `allocate_many` and one `commit_many` per batch; each handle drop still takes the lock.
//! - `lookup_batch8`, `lookup_batch32`: `lookup` in batches. One `lookup_prefix` over
//!   consecutive shared hashes per batch; each handle drop still takes the lock.
//!
//! One element is one block cycled by one thread, so batch and single-block workloads report
//! comparable blocks per second. Ideal scaling doubles elements per second when
//! threads double. Time is measured from a barrier release to the last thread joining.

use std::sync::Barrier;
use std::time::{Duration, Instant};

use avo_block_pool::BlockPool;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

const THREADS: [usize; 4] = [1, 2, 4, 8];
const CAPACITY: usize = 4_096;
const SHARED_HASHES: u64 = 1_024;
const BATCHES: [usize; 2] = [8, 32];

/// Runs `cycle(t, i)` for `iters` cycles on each of the `threads` threads, all released
/// together, and returns the wall time until the last thread finishes.
fn run_threads<F>(threads: usize, iters: u64, cycle: F) -> Duration
where
    F: Fn(usize, u64) + Sync,
{
    let barrier = Barrier::new(threads + 1);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (barrier, cycle) = (&barrier, &cycle);
                s.spawn(move || {
                    barrier.wait();
                    for i in 0..iters {
                        cycle(t, i);
                    }
                })
            })
            .collect();
        barrier.wait();
        let start = Instant::now();
        for h in handles {
            h.join().expect("bench thread panicked");
        }
        start.elapsed()
    })
}

/// Unique hashes matter because:
/// - If two threads committed under the same hash, the second would replace the first in
///   the pool’s hash index. That’s legal, but then churn would measure a mix of normal
///   commits and index replacements.
/// - Unique hashes keep every cycle identical: One allocation, which evicts the oldest
///   block once the pool is full; one index insert; and one release to the inactive list.
///
/// The benchmark then measures the same path on every thread, so throughput changes as
/// threads are added come from lock contention and nothing else.
fn churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention");
    for threads in THREADS {
        group.throughput(Throughput::Elements(threads as u64));
        group.bench_function(BenchmarkId::new("churn", threads), |b| {
            b.iter_custom(|iters| {
                let pool = BlockPool::new(CAPACITY);

                // Each thread runs the allocate, commit, drop cycle many times.
                // t is thread index from 0 to 7
                // i is cycle number within that thread
                run_threads(threads, iters, |t, i| {
                    let h = pool.allocate().expect("free or evictable block");

                    // (t as u64) << 48 converts thread index to u64 & shifts left by 48 bits
                    // moving it into top 16 bits of 64-bit number. Lower 48 bits become zero.
                    // | i is bitwise OR, filling the zeroed low 48 bits with cycle number.
                    // Thread index sits in the high bits, cycle number in the low bits; the
                    // two never overlap, so OR works like addition.
                    //
                    // Result: Thread ID in the high bits, keeping hash unique across threads
                    h.commit(((t as u64) << 48) | i).expect("commit");
                    drop(black_box(h));
                })
            });
        });
    }
    group.finish();
}

fn lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention");
    for threads in THREADS {
        group.throughput(Throughput::Elements(threads as u64));
        group.bench_function(BenchmarkId::new("lookup", threads), |b| {
            b.iter_custom(|iters| {
                let pool = BlockPool::new(CAPACITY);
                let held: Vec<_> = (0..SHARED_HASHES)
                    .map(|hash| {
                        let h = pool.allocate().expect("free block");
                        h.commit(hash).expect("commit");
                        h
                    })
                    .collect();
                drop(held);
                run_threads(threads, iters, |t, i| {
                    let hash = (i.wrapping_mul(2_654_435_761) ^ t as u64) % SHARED_HASHES;
                    drop(black_box(
                        pool.lookup(hash).expect("shared block is cached"),
                    ));
                })
            });
        });
    }
    group.finish();
}

/// `churn` with one `allocate_many` and one `commit_many` per batch. Hashes stay unique
/// across threads and cycles, as in `churn`: Thread index in the high 16 bits, and the
/// cycle number times the batch size plus the position in the batch in the low 48.
fn churn_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention");
    for batch in BATCHES {
        for threads in THREADS {
            group.throughput(Throughput::Elements((threads * batch) as u64));
            let id = BenchmarkId::new(format!("churn_batch{batch}"), threads);
            group.bench_function(id, |b| {
                b.iter_custom(|iters| {
                    let pool = BlockPool::new(CAPACITY);
                    run_threads(threads, iters, |t, i| {
                        let handles = pool.allocate_many(batch).expect("free or evictable blocks");
                        let base = ((t as u64) << 48) | (i * batch as u64);
                        let items: Vec<_> = handles.iter().zip(base..).collect();
                        pool.commit_many(&items).expect("commit");
                        drop(items);
                        drop(black_box(handles));
                    })
                });
            });
        }
    }
    group.finish();
}

/// `lookup` with one `lookup_prefix` per batch over `batch` consecutive shared hashes, so
/// every probe hits.
fn lookup_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention");
    let hashes: Vec<u64> = (0..SHARED_HASHES).collect();
    for batch in BATCHES {
        for threads in THREADS {
            group.throughput(Throughput::Elements((threads * batch) as u64));
            let id = BenchmarkId::new(format!("lookup_batch{batch}"), threads);
            group.bench_function(id, |b| {
                b.iter_custom(|iters| {
                    let pool = BlockPool::new(CAPACITY);
                    let held: Vec<_> = hashes
                        .iter()
                        .map(|&hash| {
                            let h = pool.allocate().expect("free block");
                            h.commit(hash).expect("commit");
                            h
                        })
                        .collect();
                    drop(held);
                    let starts = SHARED_HASHES - batch as u64 + 1;
                    run_threads(threads, iters, |t, i| {
                        let start = ((i.wrapping_mul(2_654_435_761) ^ t as u64) % starts) as usize;
                        let hits = pool.lookup_prefix(&hashes[start..start + batch]);
                        assert_eq!(hits.len(), batch, "shared blocks are cached");
                        drop(black_box(hits));
                    })
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, churn, lookup, churn_batch, lookup_batch);
criterion_main!(benches);
