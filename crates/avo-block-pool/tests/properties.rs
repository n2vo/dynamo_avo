//! Proving test for Milestone 2: No block is leaked and no block is freed twice.
//!
//! The property test drives the pool with random operation sequences and checks the
//! invariants after every step. The unit tests pin one lifecycle transition each.

use std::collections::HashSet;

use avo_block_pool::{Audit, BlockHandle, BlockId, BlockPool, PoolError};
use proptest::prelude::*;

const CAPACITY: usize = 8;

/// Every block id `0..capacity` appears exactly once across active, inactive, and free.
/// A missing id is a leak; a repeated id is a double free.
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

fn assert_active_matches_handles(audit: &Audit, handles: &[BlockHandle]) {
    let held: HashSet<BlockId> = handles.iter().map(BlockHandle::id).collect();
    let active: HashSet<BlockId> = audit.active.iter().copied().collect();
    assert_eq!(
        active, held,
        "active set differs from blocks held by live handles"
    );
}

#[derive(Clone, Debug)]
enum Op {
    Allocate,
    /// Commit then handle at this inde (module live handles) under this hash.
    Commit(usize, u64),
    /// Look up this hash.
    Lookup(u64),
    /// Clone the handle at this index.
    Clone(usize),
    /// Drop the handle at this index.
    Drop(usize),
}

fn op() -> impl Strategy<Value = Op> {
    // A small hash space so lookups hit and duplicate commits happen.
    prop_oneof![
        3 => Just(Op::Allocate),
        2 => (any::<usize>(), 0u64..6).prop_map(|(i, h)| Op::Commit(i, h)),
        2 => (0u64..6).prop_map(Op::Lookup),
        1 => any::<usize>().prop_map(Op::Clone),
        3 => any::<usize>().prop_map(Op::Drop),
    ]
}

proptest! {
    #[test]
    fn no_block_leaked_or_freed_twice(ops in prop::collection::vec(op(), 0..200)) {
        let pool = BlockPool::new(CAPACITY);
        let mut handles: Vec<BlockHandle> = Vec::new();

        for op in ops {
            match op {
                Op::Allocate => match pool.allocate() {
                    Ok(h) => handles.push(h),
                    // Exhausted is correct only when every block is held by a handle.
                    Err(PoolError::Exhausted) => {
                        prop_assert_eq!(pool.stats().active, CAPACITY);
                    }
                    Err(e) => prop_assert!(false, "unexpected error {e:?}"),
                },
                Op::Commit(i, hash) => {
                    if !handles.is_empty() {
                        let h = &handles[i % handles.len()];
                        let before = h.hash();
                        let result = h.commit(hash);
                        match before {
                            None => prop_assert!(result.is_ok()),
                            Some(_) => prop_assert_eq!(result, Err(PoolError::AlreadyCommitted)),
                        }
                    }
                }
                Op::Lookup(hash) => {
                    if let Some(h) = pool.lookup(hash) {
                        prop_assert_eq!(h.hash(), Some(hash));
                        handles.push(h);
                    }
                }
                Op::Clone(i) => {
                    if !handles.is_empty() {
                        let h = handles[i % handles.len()].clone();
                        handles.push(h);
                    }
                }
                Op::Drop(i) => {
                    if !handles.is_empty() {
                        let n = handles.len();
                        drop(handles.swap_remove(i % n));
                    }
                }
            }

            let audit = pool.audit();
            assert_partition(&audit, CAPACITY);
            assert_active_matches_handles(&audit, &handles);
            let stats = pool.stats();
            prop_assert_eq!(stats.active + stats.inactive + stats.free, CAPACITY);
        }

        // Dropping every handle leaves nothing active.
        handles.clear();
        let audit = pool.audit();
        assert_partition(&audit, CAPACITY);
        prop_assert!(audit.active.is_empty());
    }
}

#[test]
fn uncommitted_block_returns_to_free() {
    let pool = BlockPool::new(2);
    let h = pool.allocate().expect("free block");
    assert_eq!(pool.stats().active, 1);
    drop(h);
    assert_eq!(pool.stats().free, 2);
}

#[test]
fn committed_block_goes_inactive_and_lookup_revives_it() {
    let pool = BlockPool::new(2);
    let h = pool.allocate().expect("free block");
    let id = h.id();
    h.commit(42).expect("first commit");
    drop(h);
    assert_eq!(pool.stats().inactive, 1);

    let hit = pool.lookup(42).expect("cache hit");
    assert_eq!(hit.id(), id);
    assert_eq!(pool.stats().active, 1);
    assert_eq!(pool.stats().inactive, 0);
}

#[test]
fn second_commit_is_rejected() {
    let pool = BlockPool::new(1);
    let h = pool.allocate().expect("free block");
    h.commit(1).expect("first commit");
    assert_eq!(h.commit(2), Err(PoolError::AlreadyCommitted));
}

#[test]
fn clone_shares_the_block_until_the_last_drop() {
    let pool = BlockPool::new(1);
    let a = pool.allocate().expect("free block");
    let b = a.clone();
    drop(a);
    assert_eq!(pool.stats().active, 1, "one handle still holds the block");
    drop(b);
    assert_eq!(pool.stats().free, 1);
}

#[test]
fn eviction_takes_least_recently_used() {
    let pool = BlockPool::new(2);
    let a = pool.allocate().expect("free block");
    a.commit(10).expect("commit");
    let b = pool.allocate().expect("free block");
    b.commit(20).expect("commit");
    drop(a); // inactive first: older
    drop(b); // inactive second: newer

    let _c = pool.allocate().expect("evicts one");
    assert!(
        pool.lookup(10).is_none(),
        "oldest inactive block was evicted"
    );
    assert!(pool.lookup(20).is_some(), "newer inactive block survives");
}

#[test]
fn exhausted_when_every_block_is_held() {
    let pool = BlockPool::new(1);
    let _h = pool.allocate().expect("free block");
    assert_eq!(pool.allocate().err(), Some(PoolError::Exhausted));
}
