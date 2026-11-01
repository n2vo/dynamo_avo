//! Concurrency stress test: The Milestone 2 invariants hold when several threads share one
//! pool.
//!
//! Each worker thread runs its own random sequence of allocate, commit, lookup, clone, and
//! drop against a shared pool, while a checker thread repeatedly audits the pool. `audit`
//! takes the pool lock, so every audit is a consistent snapshot and must show each block
//! id exactly once. After the workers finish and drop every handle, nothing is active.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use avo_block_pool::{Audit, BlockHandle, BlockId, BlockPool, PoolError};

const CAPACITY: usize = 64;
const THREADS: usize = 8;
const OPS_PER_THREAD: usize = 20_000;
/// Small shared hash space, so threads hit each other's blocks and commit duplicate hashes.
const HASHES: u64 = 32;

/// xorshift64: A small deterministic generator, so a failing run can be reproduced from its
/// thread seed without adding a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Every block id `0..capacity` appears exactly once across active, inactive, and free.
fn assert_partition(audit: &Audit, capacity: usize) {
    let mut seen = HashSet::new();
    for id in audit
        .active
        .iter()
        .chain(&audit.inactive)
        .chain(&audit.free)
    {
        assert!(
            seen.insert(*id),
            "block {id:?} appears in more than one place"
        );
    }
    let expected: HashSet<BlockId> = (0..capacity as u32).map(BlockId).collect();
    assert_eq!(seen, expected, "a block is missing from every set (leaked)");
}

fn worker(pool: &BlockPool, seed: u64) {
    let mut rng = Rng(seed);
    let mut handles: Vec<BlockHandle> = Vec::new();

    for _ in 0..OPS_PER_THREAD {
        match rng.below(10) {
            // Allocate. Exhausted is legal under contention: Other threads may hold every
            // block at this instant.
            0..=2 => match pool.allocate() {
                Ok(h) => handles.push(h),
                Err(PoolError::Exhausted) => {}
                Err(e) => panic!("unexpected error {e:?}"),
            },
            // Commit one of this thread's handles under a shared hash.
            3 | 4 => {
                if !handles.is_empty() {
                    let i = rng.below(handles.len() as u64) as usize;
                    let hash = rng.below(HASHES);
                    match handles[i].commit(hash) {
                        Ok(()) | Err(PoolError::AlreadyCommitted) => {}
                        Err(e) => panic!("unexpected error {e:?}"),
                    }
                }
            }
            // Look up a shared hash: A hit must carry that hash.
            5 | 6 => {
                let hash = rng.below(HASHES);
                if let Some(h) = pool.lookup(hash) {
                    assert_eq!(h.hash(), Some(hash), "lookup returned the wrong block");
                    handles.push(h);
                }
            }
            // Clone one of this thread's handles.
            7 => {
                if !handles.is_empty() {
                    let i = rng.below(handles.len() as u64) as usize;
                    let h = handles[i].clone();
                    handles.push(h);
                }
            }
            // Drop one of this thread's handles.
            _ => {
                if !handles.is_empty() {
                    let i = rng.below(handles.len() as u64) as usize;
                    drop(handles.swap_remove(i));
                }
            }
        }
        // Cap the handles a thread holds so threads keep returning blocks to the pool.
        if handles.len() > CAPACITY / THREADS {
            handles.truncate(CAPACITY / THREADS / 2);
        }
    }
}

#[test]
fn invariants_hold_under_concurrent_use() {
    let pool = BlockPool::new(CAPACITY);
    let done = AtomicBool::new(false);

    std::thread::scope(|s| {
        let checker = s.spawn(|| {
            let mut audits = 0u64;
            while !done.load(Ordering::Relaxed) {
                assert_partition(&pool.audit(), CAPACITY);
                audits += 1;
            }
            audits
        });

        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let pool = &pool;
                // 0x9E37_7989_7F4A_7C15 -> 64-bit hex number, known constant in hashing & RNG
                // d0x9E37_7989_7F4A_7C15 <- from 2^64 / 1.618, aka golden ratio.
                s.spawn(move || worker(pool, 0x9E37_79B9_7F4A_7C15 ^ (t as u64 + 1)))
            })
            .collect();

        // Join every worker before stopping the checker, so a panicking worker fails the test
        // instead of leaving the checker looping forever
        let results: Vec<_> = workers.into_iter().map(|w| w.join()).collect();
        done.store(true, Ordering::Relaxed);
        let audits = checker.join().expect("checker thread panicked");
        for r in results {
            if let Err(panic) = r {
                std::panic::resume_unwind(panic);
            }
        }
        assert!(audits > 0, "the checker never ran");
    });

    // Every worker dropped its handle on exit, so nothing is active.
    let audit = pool.audit();
    assert_partition(&audit, CAPACITY);
    assert!(
        audit.active.is_empty(),
        "block still active after every handle is dropped"
    );
}
