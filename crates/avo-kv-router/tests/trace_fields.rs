//! Trace-field test for Milestone 5, alone in its own test binary. Run in parallel with the
//! other router tests in one process, it intermittently missed the debug-level fields; run
//! alone, it passed every time. The cause inside `tracing` was not isolated.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use avo_block_pool::BlockId;
use avo_kv_hash::hash_sequence;
use avo_kv_router::{
    BlockKey, CostWeights, KvEvent, KvRouter, LoadReport, RoutingPolicy, Tier, WorkerId,
};

fn seq(tokens: &[u32]) -> Vec<BlockKey> {
    hash_sequence(tokens, 1, None)
}

/// Records the name of every span and event field the router emits.
#[derive(Clone, Default)]
struct FieldNames(Arc<Mutex<BTreeSet<String>>>);

impl FieldNames {
    fn add(&self, meta: &tracing::Metadata<'_>) {
        if !meta.target().starts_with("avo_kv_router") {
            return;
        }
        if let Ok(mut names) = self.0.lock() {
            names.extend(
                meta.fields()
                    .iter()
                    .map(|f| f.name().to_string())
                    .filter(|n| n != "message"),
            );
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for FieldNames {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.add(attrs.metadata());
    }

    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        self.add(event.metadata());
    }
}

#[test]
fn trace_fields_match_avo_obs() {
    use avo_obs::fields;
    use tracing_subscriber::layer::SubscriberExt;

    let names = FieldNames::default();
    let subscriber = tracing_subscriber::registry().with(names.clone());
    tracing::subscriber::with_default(subscriber, || {
        let weights = CostWeights::new(1.0, 1.0).expect("weights");
        let mut r = KvRouter::new(RoutingPolicy::KvAware, weights);
        let t = r.tracker_mut();
        for id in 0..2 {
            t.add_worker(WorkerId(id), 100).expect("add");
        }
        let q = seq(&[1, 2, 3]);
        let stored = KvEvent::BlockStored {
            worker: WorkerId(0),
            seq: q.clone(),
            blocks: (0..3).map(BlockId).collect(),
            tier: Tier::Tier1Device,
        };
        let removed = KvEvent::BlockRemoved {
            worker: WorkerId(0),
            seq: q[..2].to_vec(),
        };
        t.apply_event(&stored).expect("store");
        t.apply_event(&removed).expect("remove");
        let report = LoadReport {
            active_sequences: 1,
            queued_tokens: 64,
            allocated_blocks: 10,
        };
        t.update_load(WorkerId(1), report).expect("load");
        r.select_worker(&q).expect("select");
    });
    let emitted = names.0.lock().expect("lock").clone();
    let canonical: BTreeSet<String> = [
        fields::WORKER_ID,
        fields::PROMPT_BLOCKS,
        fields::CACHE_HIT_BLOCKS,
        fields::ROUTE_COST,
        fields::ROUTING_POLICY,
        fields::KV_EVENT,
        fields::ACTIVE_SEQUENCES,
        fields::QUEUED_TOKENS,
        fields::BLOCKS_IN_USE,
        fields::BLOCKS_TOTAL,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        emitted, canonical,
        "every field the router emits is an avo-obs name, and each name is emitted"
    );
}
