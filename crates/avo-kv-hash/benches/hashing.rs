//! Baseline: Dynamo's hashing throughput. Add this crate's implementation alongside it.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use dynamo_kv_hashing::Request;
use std::hint::black_box;

fn dynamo_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("plh");
    for len in [1_024u32, 8_192, 32_768] {
        let tokens: Vec<u32> = (0..len).collect();
        group.throughput(Throughput::Elements(u64::from(len)));
        group.bench_with_input(BenchmarkId::new("dynamo", len), &tokens, |b, t| {
            b.iter(|| {
                let req = Request::builder()
                    .tokens(black_box(t.clone()))
                    .build()
                    .expect("valid request");
                black_box(req.positional_lineage_hashes(16).expect("hashing succeeds"))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, dynamo_baseline);
criterion_main!(benches);
