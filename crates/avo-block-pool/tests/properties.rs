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
    /// Commit the handle at this index (modulo live handles) under this hash.
    Commit(usize, u64),
    /// Look up this hash.
    Lookup(u64),
    /// Clone the handle at this index.
    Clone(usize),
    /// Drop the handle at this index.
    Drop(usize),
    /// Allocate this many blocks in one call.
    AllocateMany(usize),
    /// Prefix-match these hashes in one call.
    LookupPrefix(Vec<u64>),
    /// Commit the handles at these indices under these hashes in one call.
    CommitMany(Vec<(usize, u64)>),
}

fn op() -> impl Strategy<Value = Op> {
    // A small hash space so lookups hit and duplicate commits happen.
    prop_oneof![
        3 => Just(Op::Allocate),
        2 => (any::<usize>(), 0u64..6).prop_map(|(i, h)| Op::Commit(i, h)),
        2 => (0u64..6).prop_map(Op::Lookup),
        1 => any::<usize>().prop_map(Op::Clone),
        3 => any::<usize>().prop_map(Op::Drop),
        1 => (0usize..=4).prop_map(Op::AllocateMany),
        1 => prop::collection::vec(0u64..6, 0..4).prop_map(Op::LookupPrefix),
        1 => prop::collection::vec((any::<usize>(), 0u64..6), 0..3).prop_map(Op::CommitMany),
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
                Op::AllocateMany(n) => {
                    let before = pool.audit();
                    match pool.allocate_many(n) {
                        Ok(batch) => {
                            prop_assert_eq!(batch.len(), n);
                            for h in &batch {
                                prop_assert_eq!(h.hash(), None);
                            }
                            handles.extend(batch);
                        }
                        // All or nothing: Exhausted only when fewer than n blocks are
                        // free or inactive, and then nothing changed.
                        Err(PoolError::Exhausted) => {
                            prop_assert!(before.free.len() + before.inactive.len() < n);
                            prop_assert_eq!(pool.audit(), before);
                        }
                        Err(e) => prop_assert!(false, "unexpected error {e:?}"),
                    }
                }
                Op::LookupPrefix(hashes) => {
                    let hits = pool.lookup_prefix(&hashes);
                    prop_assert!(hits.len() <= hashes.len());
                    for (h, hash) in hits.iter().zip(&hashes) {
                        prop_assert_eq!(h.hash(), Some(*hash));
                    }
                    // A short result means the next hash is a miss. A miss changes nothing,
                    // so checking it with `lookup` is safe.
                    if let Some(&next) = hashes.get(hits.len()) {
                        prop_assert!(pool.lookup(next).is_none(), "stopped before a miss");
                    }
                    handles.extend(hits);
                }
                Op::CommitMany(picks) => {
                    if !handles.is_empty() {
                        let items: Vec<(&BlockHandle, u64)> = picks
                            .iter()
                            .map(|&(i, hash)| (&handles[i % handles.len()], hash))
                            .collect();
                        let before: Vec<Option<u64>> = items.iter().map(|(h, _)| h.hash()).collect();
                        let ids: HashSet<BlockId> = items.iter().map(|(h, _)| h.id()).collect();
                        let reject = ids.len() < items.len() || before.iter().any(Option::is_some);
                        let result = pool.commit_many(&items);
                        if reject {
                            prop_assert_eq!(result, Err(PoolError::AlreadyCommitted));
                            let after: Vec<Option<u64>> = items.iter().map(|(h, _)| h.hash()).collect();
                            prop_assert_eq!(after, before, "a rejected batch changed a block");
                        } else {
                            prop_assert!(result.is_ok());
                            for (h, hash) in &items {
                                prop_assert_eq!(h.hash(), Some(*hash));
                            }
                        }
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

#[test]
fn evicting_a_replaced_block_keeps_the_newer_index_entry() {
    let pool = BlockPool::new(2);
    let a = pool.allocate().expect("free block");
    a.commit(7).expect("commit");
    let b = pool.allocate().expect("free block");
    b.commit(7).expect("same hash; b replaces a in the index");
    let b_id = b.id();
    drop(a); // a goes inactive first, so it is the eviction victim
    drop(b);

    let _c = pool.allocate().expect("evicts a");
    let hit = pool.lookup(7).expect("b is still cached under hash 7");
    assert_eq!(hit.id(), b_id);
}

#[test]
fn allocate_many_exhausted_changes_nothing() {
    let pool = BlockPool::new(4);
    let held = pool.allocate().expect("free block");
    let cached = pool.allocate().expect("free block");
    cached.commit(9).expect("commit");
    drop(cached); // 2 free, 1 inactive, 1 active
    let before = pool.audit();
    assert_eq!(pool.allocate_many(4).err(), Some(PoolError::Exhausted));
    assert_eq!(
        pool.audit(),
        before,
        "a failed batch must not take or evict blocks"
    );
    assert!(
        pool.lookup(9).is_some(),
        "the inactive block was not evicted"
    );
    drop(held);
}

#[test]
fn allocate_many_takes_free_then_evicts_least_recently_used() {
    let pool = BlockPool::new(3);
    let a = pool.allocate().expect("free block");
    a.commit(1).expect("commit");
    let b = pool.allocate().expect("free block");
    b.commit(2).expect("commit");
    drop(a); // older inactive
    drop(b); // newer inactive; one block still free

    let batch = pool.allocate_many(2).expect("one free plus one evicted");
    assert_eq!(batch.len(), 2);
    assert!(
        pool.lookup(1).is_none(),
        "oldest inactive block was evicted"
    );
    assert!(pool.lookup(2).is_some(), "newer inactive block survives");
}

#[test]
fn allocate_many_eviction_keeps_the_newer_index_entry() {
    let pool = BlockPool::new(2);
    let a = pool.allocate().expect("free block");
    a.commit(7).expect("commit");
    let b = pool.allocate().expect("free block");
    b.commit(7).expect("same hash; b replaces a in the index");
    let b_id = b.id();
    drop(a); // a goes inactive first, so it is the eviction victim
    drop(b);

    let _batch = pool.allocate_many(1).expect("evicts a");
    let hit = pool.lookup(7).expect("b is still cached under hash 7");
    assert_eq!(hit.id(), b_id);
}

#[test]
fn lookup_prefix_stops_at_the_first_miss() {
    let pool = BlockPool::new(4);
    for hash in [10, 11, 13] {
        let h = pool.allocate().expect("free block");
        h.commit(hash).expect("commit");
    }
    let hits = pool.lookup_prefix(&[10, 11, 12, 13]);
    let got: Vec<Option<u64>> = hits.iter().map(BlockHandle::hash).collect();
    assert_eq!(
        got,
        vec![Some(10), Some(11)],
        "13 is cached but after the miss on 12"
    );
    assert_eq!(pool.stats().active, 2, "13 was not revived");
    assert!(pool.lookup_prefix(&[]).is_empty());
}

#[test]
fn commit_many_rejects_without_partial_apply() {
    let pool = BlockPool::new(4);
    let a = pool.allocate().expect("free block");
    let b = pool.allocate().expect("free block");
    let c = pool.allocate().expect("free block");
    c.commit(30).expect("commit");

    // c is already committed: Nothing in the batch is applied, including a and b.
    assert_eq!(
        pool.commit_many(&[(&a, 10), (&b, 20), (&c, 31)]),
        Err(PoolError::AlreadyCommitted)
    );
    assert_eq!((a.hash(), b.hash()), (None, None));
    assert!(pool.lookup(10).is_none());

    // The same block twice, through a clone: Rejected, nothing applied.
    let a2 = a.clone();
    assert_eq!(
        pool.commit_many(&[(&a, 10), (&a2, 11)]),
        Err(PoolError::AlreadyCommitted)
    );
    assert_eq!(a.hash(), None);

    pool.commit_many(&[(&a, 10), (&b, 20)])
        .expect("valid batch");
    assert_eq!((a.hash(), b.hash()), (Some(10), Some(20)));
}

#[test]
fn commit_many_rejects_a_handle_from_another_pool() {
    let pool = BlockPool::new(2);
    let other = BlockPool::new(2);
    let a = pool.allocate().expect("free block");
    let x = other.allocate().expect("free block");
    assert_eq!(
        pool.commit_many(&[(&a, 1), (&x, 2)]),
        Err(PoolError::ForeignHandle)
    );
    assert_eq!((a.hash(), x.hash()), (None, None));
}
