//! Milestone 5: KV-aware router.
//!
//! Chooses an inference worker for a request by weighing how much of the request's prefix a
//! worker already caches (prefill it can skip) against how loaded the worker is. The router
//! keeps one radix index (Milestone 3) per worker, mirrored from that worker's KV cache
//! events, plus the load and health each worker reports.
//!
//! - [`WorkerTracker`]: worker state, health, load, and each worker's cached prefixes.
//! - [`KvRouter`]: worker selection under a [`RoutingPolicy`] and [`CostWeights`].
//!
//! Cost of sending a request of `total` blocks to a worker that caches its first `matched`
//! blocks: `prefill_weight * (total - matched) + load_weight * load_ratio`, where
//! `load_ratio` is the worker's allocated blocks over its capacity. The lowest cost wins;
//! ties go to the lower load ratio, then the lower worker id.
//!
//! Dynamo counterpart: `lib/router/kv-router/src/scheduling`, with the shared indexer in
//! `lib/router/kv-indexer` and the default cost in `lib/router/scheduler/src/selector`.
//!
//! Proving tests: Routing invariants under random worker states, and per-worker prefix
//! matches that equal a ground-truth model of every worker's cache under random event
//! streams.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use avo_block_pool::BlockId;
pub use avo_radix_index::{BlockKey, Tier, WorkerId};
use avo_radix_index::{BlockMeta, RadixError, RadixTree};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RouterError {
    #[error("no worker is healthy and below capacity")]
    NoAvailableWorkers,

    #[error("worker {0:?} is not registered")]
    UnknownWorker(WorkerId),

    #[error("worker {0:?} is already registered")]
    DuplicateWorker(WorkerId),

    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),

    #[error("{hashes} block hashes but {blocks} block ids")]
    LengthMismatch { hashes: usize, blocks: usize },

    #[error("radix index: {0}")]
    Index(#[from] RadixError),
}

/// Whether a worker may take new requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Healthy,
    /// Finishing its current requests; takes no new ones.
    Draining,
    Unhealthy,
}

/// What the router knows about one worker's load and health.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerState {
    pub worker_id: WorkerId,
    pub health: Health,
    pub active_sequences: u32,
    pub queued_tokens: u64,
    pub allocated_blocks: u64,
    pub capacity_blocks: u64,
}

impl WorkerState {
    /// Allocated blocks over capacity. Above 1.0 if a worker reports more than capacity.
    pub fn load_ratio(&self) -> f64 {
        self.allocated_blocks as f64 / self.capacity_blocks as f64
    }

    pub fn is_full(&self) -> bool {
        self.allocated_blocks >= self.capacity_blocks
    }

    /// Healthy and below capacity.
    pub fn is_eligible(&self) -> bool {
        self.health == Health::Healthy && !self.is_full()
    }
}

/// Load a worker reports. Replaces the previous report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadReport {
    pub active_sequences: u32,
    pub queued_tokens: u64,
    pub allocated_blocks: u64,
}

/// A change to one worker's KV cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvEvent {
    /// The worker caches every block of `seq`, a chained sequence from its first block;
    /// `blocks[i]` holds `seq[i]`.
    BlockStored {
        worker: WorkerId,
        seq: Vec<BlockKey>,
        blocks: Vec<BlockId>,
        tier: Tier,
    },
    /// The worker dropped the last block of `seq`. The router also drops every block that
    /// extends it, since no prefix match can pass the missing block.
    BlockRemoved {
        worker: WorkerId,
        seq: Vec<BlockKey>,
    },
    /// The worker dropped its whole cache.
    WorkerCleared { worker: WorkerId },
}

impl KvEvent {
    pub fn worker(&self) -> WorkerId {
        match self {
            Self::BlockStored { worker, .. }
            | Self::BlockRemoved { worker, .. }
            | Self::WorkerCleared { worker } => *worker,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::BlockStored { .. } => "block_stored",
            Self::BlockRemoved { .. } => "block_removed",
            Self::WorkerCleared { .. } => "worker_cleared",
        }
    }

    fn blocks(&self) -> usize {
        match self {
            Self::BlockStored { seq, .. } | Self::BlockRemoved { seq, .. } => seq.len(),
            Self::WorkerCleared { .. } => 0,
        }
    }
}

struct Worker {
    state: WorkerState,
    index: RadixTree,
}

/// Every registered worker's state and cached prefixes, ordered by worker id.
#[derive(Default)]
pub struct WorkerTracker {
    workers: BTreeMap<u32, Worker>,
}

impl WorkerTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn worker(&self, id: WorkerId) -> Result<&Worker, RouterError> {
        self.workers
            .get(&id.0)
            .ok_or(RouterError::UnknownWorker(id))
    }

    fn worker_mut(&mut self, id: WorkerId) -> Result<&mut Worker, RouterError> {
        self.workers
            .get_mut(&id.0)
            .ok_or(RouterError::UnknownWorker(id))
    }

    /// Registers a healthy, idle worker with an empty cache.
    pub fn add_worker(&mut self, id: WorkerId, capacity_blocks: u64) -> Result<(), RouterError> {
        if capacity_blocks == 0 {
            return Err(RouterError::InvalidConfig("capacity must be positive"));
        }
        if self.workers.contains_key(&id.0) {
            return Err(RouterError::DuplicateWorker(id));
        }
        let state = WorkerState {
            worker_id: id,
            health: Health::Healthy,
            active_sequences: 0,
            queued_tokens: 0,
            allocated_blocks: 0,
            capacity_blocks,
        };
        self.workers.insert(
            id.0,
            Worker {
                state,
                index: RadixTree::new(),
            },
        );
        Ok(())
    }

    /// Unregisters a worker and forgets its cache. Returns its last state.
    pub fn remove_worker(&mut self, id: WorkerId) -> Result<WorkerState, RouterError> {
        self.workers
            .remove(&id.0)
            .map(|w| w.state)
            .ok_or(RouterError::UnknownWorker(id))
    }

    pub fn set_health(&mut self, id: WorkerId, health: Health) -> Result<(), RouterError> {
        self.worker_mut(id)?.state.health = health;
        Ok(())
    }

    pub fn update_load(&mut self, id: WorkerId, report: LoadReport) -> Result<(), RouterError> {
        let state = &mut self.worker_mut(id)?.state;
        state.active_sequences = report.active_sequences;
        state.queued_tokens = report.queued_tokens;
        state.allocated_blocks = report.allocated_blocks;
        tracing::debug!(
            worker_id = id.0,
            active_sequences = report.active_sequences,
            queued_tokens = report.queued_tokens,
            blocks_in_use = report.allocated_blocks,
            blocks_total = state.capacity_blocks,
            "router_load"
        );
        Ok(())
    }

    /// Applies one cache event. Returns the blocks added to or removed from the router's
    /// view of that worker. Removing a block the router already dropped is not an error:
    /// It went with an earlier removal of a block it extends.
    pub fn apply_event(&mut self, event: &KvEvent) -> Result<usize, RouterError> {
        let id = event.worker();
        let _span = tracing::debug_span!(
            "router_kv_event",
            worker_id = id.0,
            kv_event = event.kind(),
            prompt_blocks = event.blocks()
        )
        .entered();
        let worker = self.worker_mut(id)?;
        match event {
            KvEvent::BlockStored {
                seq, blocks, tier, ..
            } => {
                if seq.len() != blocks.len() {
                    return Err(RouterError::LengthMismatch {
                        hashes: seq.len(),
                        blocks: blocks.len(),
                    });
                }
                let metas: Vec<BlockMeta> = blocks
                    .iter()
                    .map(|&block| BlockMeta {
                        worker: id,
                        block,
                        tier: *tier,
                    })
                    .collect();
                Ok(worker.index.insert(seq, &metas)?.new_blocks)
            }
            KvEvent::BlockRemoved { seq, .. } => match worker.index.remove(seq) {
                Ok(n) => Ok(n),
                Err(RadixError::NotFound) => Ok(0),
                Err(e) => Err(e.into()),
            },
            KvEvent::WorkerCleared { .. } => {
                let n = worker.index.stats().blocks;
                worker.index = RadixTree::new();
                Ok(n)
            }
        }
    }

    /// Leading blocks of `seq` that `id` caches, as far as the router knows.
    pub fn matched_blocks(&mut self, id: WorkerId, seq: &[BlockKey]) -> Result<usize, RouterError> {
        Ok(self.worker_mut(id)?.index.match_prefix(seq).matched)
    }

    /// Blocks in the router's view of `id`'s cache.
    pub fn cached_blocks(&self, id: WorkerId) -> Result<usize, RouterError> {
        Ok(self.worker(id)?.index.stats().blocks)
    }

    pub fn state(&self, id: WorkerId) -> Result<WorkerState, RouterError> {
        Ok(self.worker(id)?.state)
    }

    /// States of every registered worker, by worker id.
    pub fn states(&self) -> impl Iterator<Item = &WorkerState> {
        self.workers.values().map(|w| &w.state)
    }

    pub fn len(&self) -> usize {
        self.workers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }
}

/// How the router picks a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingPolicy {
    /// Lowest cost under [`CostWeights`]; matches the request against every eligible worker.
    KvAware,
    /// Next eligible worker after the last pick, by worker id.
    RoundRobin,
    /// Lowest load ratio, then lowest worker id.
    LeastLoaded,
}

impl RoutingPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KvAware => "kv_aware",
            Self::RoundRobin => "round_robin",
            Self::LeastLoaded => "least_loaded",
        }
    }
}

/// Weights of the two cost terms. Both finite and non-negative.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CostWeights {
    prefill_weight: f64,
    load_weight: f64,
}

impl CostWeights {
    pub fn new(prefill_weight: f64, load_weight: f64) -> Result<Self, RouterError> {
        let valid = |w: f64| w.is_finite() && w >= 0.0;
        if !valid(prefill_weight) || !valid(load_weight) {
            return Err(RouterError::InvalidConfig(
                "weights must be finite and non-negative",
            ));
        }
        Ok(Self {
            prefill_weight,
            load_weight,
        })
    }

    pub fn prefill_weight(&self) -> f64 {
        self.prefill_weight
    }

    pub fn load_weight(&self) -> f64 {
        self.load_weight
    }

    /// `prefill_weight * unmatched_blocks + load_weight * load_ratio`.
    pub fn cost(&self, unmatched_blocks: usize, load_ratio: f64) -> f64 {
        self.prefill_weight * unmatched_blocks as f64 + self.load_weight * load_ratio
    }
}

/// Where a request goes and how much prefill it needs there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoutingDecision {
    pub target_worker: WorkerId,
    /// Leading request blocks the target already caches.
    pub matched_blocks: usize,
    /// Blocks the target must prefill.
    pub unmatched_blocks: usize,
    /// Cost of the target under the router's weights, whatever the policy.
    pub cost: f64,
    pub policy: RoutingPolicy,
}

/// Worker selection over a [`WorkerTracker`].
pub struct KvRouter {
    tracker: WorkerTracker,
    policy: RoutingPolicy,
    weights: CostWeights,
    /// Worker id of the last round-robin pick.
    last_pick: Option<u32>,
}

impl KvRouter {
    pub fn new(policy: RoutingPolicy, weights: CostWeights) -> Self {
        Self {
            tracker: WorkerTracker::new(),
            policy,
            weights,
            last_pick: None,
        }
    }

    pub fn tracker(&self) -> &WorkerTracker {
        &self.tracker
    }

    pub fn tracker_mut(&mut self) -> &mut WorkerTracker {
        &mut self.tracker
    }

    pub fn policy(&self) -> RoutingPolicy {
        self.policy
    }

    pub fn set_policy(&mut self, policy: RoutingPolicy) {
        self.policy = policy;
    }

    pub fn weights(&self) -> CostWeights {
        self.weights
    }

    /// Picks a healthy worker below capacity for a request with block hashes `seq`.
    pub fn select_worker(&mut self, seq: &[BlockKey]) -> Result<RoutingDecision, RouterError> {
        let span = tracing::debug_span!(
            "router_select",
            routing_policy = self.policy.as_str(),
            prompt_blocks = seq.len(),
            worker_id = tracing::field::Empty,
            cache_hit_blocks = tracing::field::Empty,
            route_cost = tracing::field::Empty
        );
        let _guard = span.enter();
        let (id, matched) = match self.policy {
            RoutingPolicy::KvAware => self.pick_kv_aware(seq)?,
            RoutingPolicy::RoundRobin => {
                let id = self.pick_round_robin()?;
                (id, self.tracker.matched_blocks(WorkerId(id), seq)?)
            }
            RoutingPolicy::LeastLoaded => {
                let id = self.pick_least_loaded()?;
                (id, self.tracker.matched_blocks(WorkerId(id), seq)?)
            }
        };
        let ratio = self.tracker.state(WorkerId(id))?.load_ratio();
        let unmatched = seq.len() - matched;
        let cost = self.weights.cost(unmatched, ratio);
        span.record("worker_id", id);
        span.record("cache_hit_blocks", matched);
        span.record("route_cost", cost);
        Ok(RoutingDecision {
            target_worker: WorkerId(id),
            matched_blocks: matched,
            unmatched_blocks: unmatched,
            cost,
            policy: self.policy,
        })
    }

    /// Lowest (cost, load ratio, worker id) over eligible workers. Returns the id and its
    /// matched blocks.
    fn pick_kv_aware(&mut self, seq: &[BlockKey]) -> Result<(u32, usize), RouterError> {
        let weights = self.weights;
        let mut best: Option<(f64, f64, u32, usize)> = None;
        for (&id, worker) in self.tracker.workers.iter_mut() {
            if !worker.state.is_eligible() {
                continue;
            }
            let matched = worker.index.match_prefix(seq).matched;
            let ratio = worker.state.load_ratio();
            let cost = weights.cost(seq.len() - matched, ratio);
            tracing::trace!(
                worker_id = id,
                cache_hit_blocks = matched,
                route_cost = cost,
                "router_score"
            );
            let better = best.is_none_or(|(c, r, i, _)| {
                cost.total_cmp(&c)
                    .then(ratio.total_cmp(&r))
                    .then(id.cmp(&i))
                    .is_lt()
            });
            if better {
                best = Some((cost, ratio, id, matched));
            }
        }
        best.map(|(_, _, id, matched)| (id, matched))
            .ok_or(RouterError::NoAvailableWorkers)
    }

    fn pick_round_robin(&mut self) -> Result<u32, RouterError> {
        let eligible = |w: &&Worker| w.state.is_eligible();
        let workers = &self.tracker.workers;
        let next = self
            .last_pick
            .and_then(|id| id.checked_add(1))
            .and_then(|after| workers.range(after..).map(|(_, w)| w).find(eligible))
            .or_else(|| workers.values().find(eligible))
            .map(|w| w.state.worker_id.0)
            .ok_or(RouterError::NoAvailableWorkers)?;
        self.last_pick = Some(next);
        Ok(next)
    }

    fn pick_least_loaded(&self) -> Result<u32, RouterError> {
        self.tracker
            .workers
            .values()
            .filter(|w| w.state.is_eligible())
            .min_by(|a, b| {
                a.state
                    .load_ratio()
                    .total_cmp(&b.state.load_ratio())
                    .then(a.state.worker_id.0.cmp(&b.state.worker_id.0))
            })
            .map(|w| w.state.worker_id.0)
            .ok_or(RouterError::NoAvailableWorkers)
    }
}
