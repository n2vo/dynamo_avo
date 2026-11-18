//! Proving tests for Milestone 5: Routing invariants under random worker states and caches,
//! and per-worker prefix matches that equal a ground-truth model of every worker's cache
//! under random event streams. Unit tests pin one behavior each. The trace-field test is in
//! `trace_fields.rs`, its own test binary.

use avo_block_pool::BlockId;
use avo_kv_hash::hash_sequence;
use avo_kv_router::{
    BlockKey, CostWeights, Health, KvEvent, KvRouter, LoadReport, RouterError, RoutingPolicy, Tier,
    WorkerId,
};
use avo_radix_index::RadixError;
use proptest::prelude::*;

/// One block per token, so two sequences share block hashes exactly as far as they share
/// tokens.
fn seq(tokens: &[u32]) -> Vec<BlockKey> {
    hash_sequence(tokens, 1, None)
}

fn stored(worker: u32, s: &[BlockKey]) -> KvEvent {
    KvEvent::BlockStored {
        worker: WorkerId(worker),
        seq: s.to_vec(),
        blocks: (0..s.len() as u32).map(BlockId).collect(),
        tier: Tier::Tier1Device,
    }
}

fn removed(worker: u32, s: &[BlockKey]) -> KvEvent {
    KvEvent::BlockRemoved {
        worker: WorkerId(worker),
        seq: s.to_vec(),
    }
}

fn weights(prefill: f64, load: f64) -> CostWeights {
    CostWeights::new(prefill, load).expect("weights")
}

/// A router with `n` healthy, idle workers of 100 blocks each, ids 0 to n - 1.
fn router(policy: RoutingPolicy, w: CostWeights, n: u32) -> KvRouter {
    let mut r = KvRouter::new(policy, w);
    for id in 0..n {
        r.tracker_mut()
            .add_worker(WorkerId(id), 100)
            .expect("add worker");
    }
    r
}

fn load(r: &mut KvRouter, worker: u32, allocated: u64) {
    let report = LoadReport {
        active_sequences: 1,
        queued_tokens: 0,
        allocated_blocks: allocated,
    };
    r.tracker_mut()
        .update_load(WorkerId(worker), report)
        .expect("load");
}

fn apply(r: &mut KvRouter, e: &KvEvent) -> usize {
    r.tracker_mut().apply_event(e).expect("event")
}

#[test]
fn kv_aware_prefers_the_cached_prefix() {
    let mut r = router(RoutingPolicy::KvAware, weights(1.0, 1.0), 3);
    let q = seq(&[1, 2, 3, 4, 5, 6]);
    apply(&mut r, &stored(1, &q[..2]));
    apply(&mut r, &stored(2, &q[..4]));
    let d = r.select_worker(&q).expect("select");
    assert_eq!(d.target_worker, WorkerId(2));
    assert_eq!((d.matched_blocks, d.unmatched_blocks), (4, 2));
    assert_eq!(d.cost, 2.0, "2 unmatched blocks, idle worker");
}

#[test]
fn load_term_outweighs_a_small_overlap() {
    let mut r = router(RoutingPolicy::KvAware, weights(1.0, 10.0), 2);
    let q = seq(&[1, 2, 3, 4]);
    apply(&mut r, &stored(0, &q[..2]));
    load(&mut r, 0, 50);
    // Worker 0: 2 + 10 * 0.5 = 7. Worker 1: 4 + 0 = 4.
    let d = r.select_worker(&q).expect("select");
    assert_eq!(d.target_worker, WorkerId(1));
    assert_eq!(d.cost, 4.0);
}

#[test]
fn ties_go_to_lower_load_then_lower_id() {
    let mut r = router(RoutingPolicy::KvAware, weights(1.0, 0.0), 3);
    let q = seq(&[1, 2]);
    load(&mut r, 0, 30);
    load(&mut r, 1, 10);
    load(&mut r, 2, 10);
    assert_eq!(
        r.select_worker(&q).expect("select").target_worker,
        WorkerId(1)
    );
    load(&mut r, 1, 20);
    assert_eq!(
        r.select_worker(&q).expect("select").target_worker,
        WorkerId(2)
    );
}

#[test]
fn round_robin_cycles_over_eligible_workers() {
    let mut r = router(RoutingPolicy::RoundRobin, weights(1.0, 1.0), 4);
    r.tracker_mut()
        .set_health(WorkerId(2), Health::Draining)
        .expect("health");
    let q = seq(&[7]);
    let picks: Vec<u32> = (0..5)
        .map(|_| r.select_worker(&q).expect("select").target_worker.0)
        .collect();
    assert_eq!(picks, [0, 1, 3, 0, 1]);
}

#[test]
fn round_robin_reports_the_target_cache() {
    let mut r = router(RoutingPolicy::RoundRobin, weights(1.0, 0.0), 2);
    let q = seq(&[1, 2, 3]);
    apply(&mut r, &stored(1, &q));
    assert_eq!(r.select_worker(&q).expect("select").matched_blocks, 0);
    assert_eq!(r.select_worker(&q).expect("select").matched_blocks, 3);
}

#[test]
fn least_loaded_ignores_the_cache() {
    let mut r = router(RoutingPolicy::LeastLoaded, weights(1.0, 0.0), 3);
    let q = seq(&[1, 2, 3]);
    apply(&mut r, &stored(0, &q));
    load(&mut r, 0, 10);
    load(&mut r, 1, 40);
    load(&mut r, 2, 5);
    let d = r.select_worker(&q).expect("select");
    assert_eq!((d.target_worker, d.matched_blocks), (WorkerId(2), 0));
}

#[test]
fn ineligible_workers_are_never_selected() {
    for policy in [
        RoutingPolicy::KvAware,
        RoutingPolicy::RoundRobin,
        RoutingPolicy::LeastLoaded,
    ] {
        let mut r = router(policy, weights(1.0, 0.0), 4);
        let q = seq(&[1, 2, 3]);
        for id in 0..3 {
            apply(&mut r, &stored(id, &q));
        }
        let t = r.tracker_mut();
        t.set_health(WorkerId(0), Health::Draining).expect("health");
        t.set_health(WorkerId(1), Health::Unhealthy)
            .expect("health");
        load(&mut r, 2, 100);
        for _ in 0..3 {
            let d = r.select_worker(&q).expect("select");
            assert_eq!(d.target_worker, WorkerId(3), "{policy:?}");
        }
        load(&mut r, 3, 120);
        assert_eq!(
            r.select_worker(&q),
            Err(RouterError::NoAvailableWorkers),
            "{policy:?}"
        );
    }
    let mut empty = KvRouter::new(RoutingPolicy::KvAware, weights(1.0, 1.0));
    assert_eq!(
        empty.select_worker(&seq(&[1])),
        Err(RouterError::NoAvailableWorkers)
    );
}

#[test]
fn removing_a_block_drops_the_blocks_that_extend_it() {
    let mut r = router(RoutingPolicy::KvAware, weights(1.0, 0.0), 1);
    let a = seq(&[1, 2, 3, 4]);
    let b = seq(&[1, 2, 9]);
    apply(&mut r, &stored(0, &a));
    apply(&mut r, &stored(0, &b));
    assert_eq!(
        apply(&mut r, &removed(0, &a[..2])),
        4,
        "blocks 2, 3, 4, and 9"
    );
    let t = r.tracker_mut();
    assert_eq!(t.matched_blocks(WorkerId(0), &a).expect("match"), 1);
    assert_eq!(t.matched_blocks(WorkerId(0), &b).expect("match"), 1);
    assert_eq!(
        apply(&mut r, &removed(0, &a)),
        0,
        "a block already dropped is not an error"
    );
    let cleared = KvEvent::WorkerCleared {
        worker: WorkerId(0),
    };
    assert_eq!(apply(&mut r, &cleared), 1);
    assert_eq!(r.tracker().cached_blocks(WorkerId(0)), Ok(0));
}

#[test]
fn errors() {
    let mut r = router(RoutingPolicy::KvAware, weights(1.0, 1.0), 1);
    let t = r.tracker_mut();
    assert_eq!(
        t.add_worker(WorkerId(0), 10),
        Err(RouterError::DuplicateWorker(WorkerId(0)))
    );
    assert_eq!(
        t.add_worker(WorkerId(5), 0),
        Err(RouterError::InvalidConfig("capacity must be positive"))
    );
    assert_eq!(
        t.set_health(WorkerId(9), Health::Healthy),
        Err(RouterError::UnknownWorker(WorkerId(9)))
    );
    assert_eq!(
        t.apply_event(&stored(9, &seq(&[1]))),
        Err(RouterError::UnknownWorker(WorkerId(9)))
    );
    let mismatched = KvEvent::BlockStored {
        worker: WorkerId(0),
        seq: seq(&[1, 2]),
        blocks: vec![BlockId(0)],
        tier: Tier::Tier1Device,
    };
    assert_eq!(
        t.apply_event(&mismatched),
        Err(RouterError::LengthMismatch {
            hashes: 2,
            blocks: 1
        })
    );
    assert_eq!(
        t.apply_event(&stored(0, &[])),
        Err(RouterError::Index(RadixError::EmptySequence))
    );
    assert_eq!(
        t.remove_worker(WorkerId(0)).map(|s| s.capacity_blocks),
        Ok(100)
    );
    for (p, l) in [(-1.0, 0.0), (0.0, f64::NAN), (f64::INFINITY, 1.0)] {
        assert!(matches!(
            CostWeights::new(p, l),
            Err(RouterError::InvalidConfig(_))
        ));
    }
}

// Routing invariants.

#[derive(Clone, Debug)]
struct WorkerSpec {
    capacity: u64,
    allocated: u64,
    health: Health,
    cached: Vec<Vec<u32>>,
}

fn health() -> impl Strategy<Value = Health> {
    prop_oneof![
        6 => Just(Health::Healthy),
        1 => Just(Health::Draining),
        1 => Just(Health::Unhealthy),
    ]
}

fn tokens() -> impl Strategy<Value = Vec<u32>> {
    // A three-token alphabet, so workers and queries share prefixes of varied lengths.
    prop::collection::vec(0u32..3, 1..7)
}

fn worker_spec() -> impl Strategy<Value = WorkerSpec> {
    (
        1u64..20,
        0u64..24,
        health(),
        prop::collection::vec(tokens(), 0..4),
    )
        .prop_map(|(capacity, allocated, health, cached)| WorkerSpec {
            capacity,
            allocated,
            health,
            cached,
        })
}

fn build(specs: &[WorkerSpec], policy: RoutingPolicy, w: CostWeights) -> KvRouter {
    let mut r = KvRouter::new(policy, w);
    for (id, s) in specs.iter().enumerate() {
        let id = id as u32;
        let t = r.tracker_mut();
        t.add_worker(WorkerId(id), s.capacity).expect("add");
        t.set_health(WorkerId(id), s.health).expect("health");
        for c in &s.cached {
            t.apply_event(&stored(id, &seq(c))).expect("store");
        }
        load(&mut r, id, s.allocated);
    }
    r
}

/// Per eligible worker: (id, matched blocks, load ratio).
fn eligible(r: &mut KvRouter, q: &[BlockKey]) -> Vec<(u32, usize, f64)> {
    let states: Vec<_> = r.tracker().states().copied().collect();
    states
        .iter()
        .filter(|s| s.is_eligible())
        .map(|s| {
            let m = r
                .tracker_mut()
                .matched_blocks(s.worker_id, q)
                .expect("match");
            (s.worker_id.0, m, s.load_ratio())
        })
        .collect()
}

proptest! {
    #[test]
    fn routing_invariants(
        specs in prop::collection::vec(worker_spec(), 0..8),
        q in tokens(),
        prefill in 0.0f64..4.0,
        load_w in 0.0f64..4.0,
    ) {
        let q = seq(&q);
        for (p, l) in [(prefill, 0.0), (0.0, load_w), (prefill, load_w)] {
            for policy in [
                RoutingPolicy::KvAware,
                RoutingPolicy::RoundRobin,
                RoutingPolicy::LeastLoaded,
            ] {
                let mut r = build(&specs, policy, weights(p, l));
                let cands = eligible(&mut r, &q);
                let d = match r.select_worker(&q) {
                    Ok(d) => d,
                    Err(e) => {
                        prop_assert_eq!(e, RouterError::NoAvailableWorkers);
                        prop_assert!(cands.is_empty(), "rejected with an eligible worker");
                        continue;
                    }
                };
                let picked = cands.iter().find(|c| c.0 == d.target_worker.0);
                prop_assert!(picked.is_some(), "picked an ineligible worker");
                let (_, matched, ratio) = *picked.expect("checked");
                prop_assert_eq!(d.matched_blocks, matched);
                prop_assert_eq!(d.unmatched_blocks, q.len() - matched);
                prop_assert_eq!(d.cost, r.weights().cost(q.len() - matched, ratio));
                let min_ratio = cands.iter().map(|c| c.2).fold(f64::INFINITY, f64::min);
                if policy == RoutingPolicy::LeastLoaded {
                    prop_assert_eq!(ratio, min_ratio);
                }
                if policy != RoutingPolicy::KvAware {
                    continue;
                }
                let costs = cands.iter().map(|c| r.weights().cost(q.len() - c.1, c.2));
                prop_assert_eq!(d.cost, costs.fold(f64::INFINITY, f64::min));
                if l == 0.0 && p > 0.0 {
                    let max = cands.iter().map(|c| c.1).max().unwrap_or(0);
                    prop_assert_eq!(matched, max, "load_weight 0 must maximize overlap");
                }
                if p == 0.0 {
                    prop_assert_eq!(ratio, min_ratio, "prefill_weight 0 must minimize load");
                    let lowest = cands
                        .iter()
                        .filter(|c| c.2 == min_ratio)
                        .map(|c| c.0)
                        .min();
                    prop_assert_eq!(Some(d.target_worker.0), lowest, "ties go to the lower id");
                }
            }
        }
    }
}

// Event streams against a ground-truth model of each worker's cache.

const WORKERS: u32 = 3;

#[derive(Clone, Debug)]
enum Op {
    Store(u32, Vec<u32>),
    /// Removes the n-th removable block of a worker, chosen from the model.
    Remove(u32, usize),
    Clear(u32),
    Query(Vec<u32>),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..WORKERS, tokens()).prop_map(|(w, t)| Op::Store(w, t)),
        3 => (0..WORKERS, any::<usize>()).prop_map(|(w, n)| Op::Remove(w, n)),
        1 => (0..WORKERS).prop_map(Op::Clear),
        4 => tokens().prop_map(Op::Query),
    ]
}

/// Leading blocks of `q` in a worker's cache: The scan stops at the first missing block.
fn truth_match(cache: &[Vec<BlockKey>], q: &[BlockKey]) -> usize {
    (1..=q.len())
        .take_while(|&k| cache.iter().any(|p| p.as_slice() == &q[..k]))
        .count()
}

/// Runs `ops` against a router and a model holding each worker's cached blocks, each block
/// as its prefix. With `leaf_only`, a worker only removes blocks nothing else extends, as
/// leaf-only eviction does; then the router must match the model exactly. Otherwise a
/// worker may remove any block, and the router may only under-count.
fn check_events(ops: Vec<Op>, leaf_only: bool) -> Result<(), TestCaseError> {
    let mut r = router(RoutingPolicy::KvAware, weights(1.0, 0.0), WORKERS);
    let mut model: Vec<Vec<Vec<BlockKey>>> = vec![Vec::new(); WORKERS as usize];
    for op in ops {
        match op {
            Op::Store(w, t) => {
                let s = seq(&t);
                apply(&mut r, &stored(w, &s));
                let cache = &mut model[w as usize];
                for k in 1..=s.len() {
                    if !cache.iter().any(|p| p.as_slice() == &s[..k]) {
                        cache.push(s[..k].to_vec());
                    }
                }
            }
            Op::Remove(w, n) => {
                let cache = &mut model[w as usize];
                let removable: Vec<usize> = (0..cache.len())
                    .filter(|&i| {
                        !leaf_only
                            || !cache
                                .iter()
                                .any(|p| p.len() > cache[i].len() && p.starts_with(&cache[i]))
                    })
                    .collect();
                if removable.is_empty() {
                    continue;
                }
                let victim = cache.remove(removable[n % removable.len()]);
                apply(&mut r, &removed(w, &victim));
            }
            Op::Clear(w) => {
                apply(
                    &mut r,
                    &KvEvent::WorkerCleared {
                        worker: WorkerId(w),
                    },
                );
                model[w as usize].clear();
            }
            Op::Query(t) => {
                let q = seq(&t);
                let mut best = 0;
                for w in 0..WORKERS {
                    let got = r
                        .tracker_mut()
                        .matched_blocks(WorkerId(w), &q)
                        .expect("match");
                    let want = truth_match(&model[w as usize], &q);
                    if leaf_only {
                        prop_assert_eq!(got, want, "worker {}", w);
                    } else {
                        prop_assert!(got <= want, "router over-counts worker {}", w);
                    }
                    best = best.max(got);
                }
                let d = r.select_worker(&q).expect("select");
                prop_assert_eq!(d.matched_blocks, best);
                prop_assert!(
                    truth_match(&model[d.target_worker.0 as usize], &q) >= d.matched_blocks,
                    "the target holds every block the router counted"
                );
            }
        }
        if leaf_only {
            for w in 0..WORKERS {
                prop_assert_eq!(
                    r.tracker().cached_blocks(WorkerId(w)),
                    Ok(model[w as usize].len())
                );
            }
        }
    }
    Ok(())
}

proptest! {
    #[test]
    fn leaf_only_removals_track_ground_truth(ops in prop::collection::vec(op(), 0..80)) {
        check_events(ops, true)?;
    }

    #[test]
    fn arbitrary_removals_never_over_count(ops in prop::collection::vec(op(), 0..80)) {
        check_events(ops, false)?;
    }
}
