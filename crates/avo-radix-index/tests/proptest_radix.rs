//! Proving test for Milestone 3: The tree's longest-prefix matches equal a brute-force map
//! of every stored prefix, under random inserts, matches, and evictions. Unit tests pin one
//! behavior each.

use std::collections::HashMap;

use avo_block_pool::BlockId;
use avo_kv_hash::hash_sequence;
use avo_radix_index::{BlockKey, BlockMeta, RadixError, RadixTree, Tier, WorkerId};
use proptest::prelude::*;

/// One block per token, so each token sequence maps to a chained hash sequence of the same
/// length, and two sequences share hashes exactly as far as they share tokens.
fn seq(tokens: &[u32]) -> Vec<BlockKey> {
    hash_sequence(tokens, 1, None)
}

fn meta(worker: u32, block: u32) -> BlockMeta {
    BlockMeta {
        worker: WorkerId(worker),
        block: BlockId(block),
        tier: Tier::Tier1Device,
    }
}

fn metas(worker: u32, len: usize) -> Vec<BlockMeta> {
    (0..len as u32).map(|b| meta(worker, b)).collect()
}

fn insert(tree: &mut RadixTree, tokens: &[u32]) -> Vec<BlockKey> {
    let s = seq(tokens);
    tree.insert(&s, &metas(0, s.len())).expect("insert");
    s
}

#[test]
fn exact_match() {
    let mut tree = RadixTree::new();
    let s = insert(&mut tree, &[1, 2, 3]);
    let m = tree.match_prefix(&s);
    assert_eq!(m.matched, 3);
    assert_eq!(m.blocks, metas(0, 3));
    assert!(m.remainder.is_empty());
}

#[test]
fn partial_prefix_match() {
    let mut tree = RadixTree::new();
    insert(&mut tree, &[1, 2, 3]);
    let q = seq(&[1, 2, 9, 9]);
    let m = tree.match_prefix(&q);
    assert_eq!(m.matched, 2);
    assert_eq!(m.remainder, &q[2..]);
}

#[test]
fn disjoint_insert() {
    let mut tree = RadixTree::new();
    let a = insert(&mut tree, &[1, 2]);
    let b = insert(&mut tree, &[7, 8]);
    let s = tree.stats();
    assert_eq!((s.nodes, s.leaves, s.blocks), (3, 2, 4));
    assert_eq!(tree.match_prefix(&a).matched, 2);
    assert_eq!(tree.match_prefix(&b).matched, 2);
}

#[test]
fn edge_split_at_k() {
    let mut tree = RadixTree::new();
    insert(&mut tree, &[1, 2, 3, 4]);
    let b = seq(&[1, 2, 5]);
    let out = tree.insert(&b, &metas(1, 3)).expect("insert");
    assert!(out.split, "the edge [1, 2, 3, 4] splits after 2 blocks");
    assert_eq!(out.new_blocks, 1);
    let s = tree.stats();
    assert_eq!((s.nodes, s.leaves, s.blocks), (4, 2, 5));
    assert_eq!(tree.match_prefix(&seq(&[1, 2, 3, 4])).matched, 4);
    let m = tree.match_prefix(&b);
    assert_eq!(m.matched, 3);
    assert_eq!(
        m.blocks,
        metas(1, 3),
        "the shared blocks take the newer metadata"
    );
}

#[test]
fn insert_inside_an_edge_does_not_split() {
    let mut tree = RadixTree::new();
    insert(&mut tree, &[1, 2, 3, 4]);
    let out = tree.insert(&seq(&[1, 2]), &metas(0, 2)).expect("insert");
    assert!(!out.split);
    assert_eq!(out.new_blocks, 0);
    assert_eq!(tree.stats().nodes, 2);
}

#[test]
fn leaf_only_eviction_takes_least_recently_used() {
    let mut tree = RadixTree::new();
    let a = insert(&mut tree, &[1, 2, 3]);
    let b = insert(&mut tree, &[1, 2, 4]);
    tree.match_prefix(&a);
    let ev = tree.evict().expect("evict");
    assert_eq!((ev.key, ev.depth), (b[2], 3), "b's leaf is older than a's");
    assert_eq!(tree.match_prefix(&b).matched, 2, "the shared prefix stays");
    assert_eq!(tree.match_prefix(&a).matched, 3);
}

#[test]
fn parent_compaction_after_eviction() {
    let mut tree = RadixTree::new();
    let a = insert(&mut tree, &[1, 2, 3]);
    insert(&mut tree, &[1, 2, 4]);
    tree.match_prefix(&a);
    tree.evict().expect("evict");
    let s = tree.stats();
    assert_eq!(
        (s.nodes, s.leaves, s.blocks),
        (2, 1, 3),
        "[1, 2] merged with [3] into one edge"
    );
    for depth in [3, 2, 1] {
        assert_eq!(tree.evict().expect("evict").depth, depth);
    }
    assert_eq!(tree.stats().nodes, 1, "only the root remains");
    assert_eq!(tree.evict(), Err(RadixError::Empty));
}

#[test]
fn evict_when_every_leaf_is_pinned() {
    let mut tree = RadixTree::new();
    let a = insert(&mut tree, &[1, 2, 3]);
    let b = insert(&mut tree, &[5, 6]);
    tree.pin(&a[..2]).expect("pin");
    tree.pin(&b).expect("pin");
    let ev = tree.evict().expect("a's unpinned tail");
    assert_eq!(ev.key, a[2]);
    assert_eq!(tree.evict(), Err(RadixError::AllPinned));
    assert_eq!(tree.stats().blocks, 4);
    tree.unpin(&b).expect("unpin");
    assert_eq!(tree.evict().expect("evict").key, b[1]);
}

#[test]
fn errors() {
    let mut tree = RadixTree::new();
    let s = seq(&[1, 2]);
    assert_eq!(tree.insert(&[], &[]), Err(RadixError::EmptySequence));
    assert_eq!(
        tree.insert(&s, &metas(0, 1)),
        Err(RadixError::LengthMismatch {
            hashes: 2,
            metas: 1
        })
    );
    assert_eq!(tree.pin(&s), Err(RadixError::NotFound));
    tree.insert(&s, &metas(0, 2)).expect("insert");
    assert_eq!(tree.unpin(&s), Err(RadixError::NotPinned));
    assert_eq!(tree.pin(&seq(&[1, 2, 3])), Err(RadixError::NotFound));
}

#[test]
fn trace_fields_match_avo_obs() {
    assert_eq!(avo_obs::fields::PROMPT_BLOCKS, "prompt_blocks");
    assert_eq!(avo_obs::fields::CACHE_HIT_BLOCKS, "cache_hit_blocks");
}

#[derive(Clone, Debug)]
enum Op {
    Insert(Vec<u32>, u32),
    Match(Vec<u32>),
    Evict,
}

fn tokens() -> impl Strategy<Value = Vec<u32>> {
    // A four-token alphabet, so sequences share prefixes and force edge splits.
    prop::collection::vec(0u32..4, 1..8)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (tokens(), 0u32..3).prop_map(|(t, w)| Op::Insert(t, w)),
        3 => tokens().prop_map(Op::Match),
        2 => Just(Op::Evict),
    ]
}

/// Longest prefix of `q` stored in `model`. Stored prefixes are prefix-closed, so the scan
/// stops at the first miss.
fn model_match(model: &HashMap<Vec<BlockKey>, BlockMeta>, q: &[BlockKey]) -> Vec<BlockMeta> {
    let mut out = Vec::new();
    for k in 1..=q.len() {
        match model.get(&q[..k]) {
            Some(m) => out.push(*m),
            None => break,
        }
    }
    out
}

proptest! {
    #[test]
    fn matches_equal_a_brute_force_map(ops in prop::collection::vec(op(), 0..120)) {
        let mut tree = RadixTree::new();
        let mut model: HashMap<Vec<BlockKey>, BlockMeta> = HashMap::new();

        for op in ops {
            match op {
                Op::Insert(t, w) => {
                    let s = seq(&t);
                    let m = metas(w, s.len());
                    tree.insert(&s, &m).expect("insert");
                    for k in 1..=s.len() {
                        model.insert(s[..k].to_vec(), m[k - 1]);
                    }
                }
                Op::Match(t) => {
                    let q = seq(&t);
                    let got = tree.match_prefix(&q);
                    let want = model_match(&model, &q);
                    prop_assert_eq!(got.matched, want.len());
                    prop_assert_eq!(&got.blocks, &want);
                    prop_assert_eq!(got.remainder.len(), q.len() - want.len());
                }
                Op::Evict => match tree.evict() {
                    Ok(ev) => {
                        let victims: Vec<Vec<BlockKey>> = model
                            .keys()
                            .filter(|k| k.last() == Some(&ev.key))
                            .cloned()
                            .collect();
                        prop_assert_eq!(victims.len(), 1, "evicted block is stored once");
                        let victim = &victims[0];
                        prop_assert_eq!(victim.len(), ev.depth);
                        let is_leaf = !model
                            .keys()
                            .any(|k| k.len() == victim.len() + 1 && k.starts_with(victim));
                        prop_assert!(is_leaf, "evicted a block another prefix passes through");
                        prop_assert_eq!(model.remove(victim), Some(ev.meta));
                    }
                    Err(e) => {
                        prop_assert_eq!(e, RadixError::Empty);
                        prop_assert!(model.is_empty());
                    }
                },
            }

            let s = tree.stats();
            prop_assert_eq!(s.blocks, model.len());
            // Every non-root node holds a block, and compaction leaves no single-child
            // internal node, so nodes stay bounded by blocks and by leaves.
            prop_assert!(s.nodes <= s.blocks + 1);
            prop_assert!(s.nodes <= (2 * s.leaves).max(1));
        }

        // Evicting everything returns the tree to its root.
        while tree.evict().is_ok() {}
        prop_assert_eq!(tree.stats().nodes, 1);
    }
}
