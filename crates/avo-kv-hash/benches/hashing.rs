//! Dynamo's hashing throughput against this crate's one-pass and incremental implementations.

use avo_kv_hash::{IncrementalHasher, hash_sequence};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use dynamo_kv_hashing::Request;
use std::hint::black_box;

fn dynamo_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("plh");
    for len in [1_024u32, 8_192, 32_768] {
        let tokens: Vec<u32> = (0..len).collect();
        let req = Request::builder()
            .tokens(tokens)
            .build()
            .expect("valid request");
        group.throughput(Throughput::Elements(u64::from(len)));
        group.bench_with_input(BenchmarkId::new("dynamo", len), &req, |b, r| {
            b.iter(|| black_box(r.positional_lineage_hashes(16).expect("hashing succeeds")));
        });
    }
    group.finish();
}

fn avo_one_pass(c: &mut Criterion) {
    let mut group = c.benchmark_group("plh");
    for len in [1_024u32, 8_192, 32_768] {
        let tokens: Vec<u32> = (0..len).collect();
        group.throughput(Throughput::Elements(u64::from(len)));
        group.bench_with_input(BenchmarkId::new("avo_one_pass", len), &tokens, |b, t| {
            b.iter(|| black_box(hash_sequence(black_box(t), 16, None)));
        });
    }
    group.finish();
}

fn avo_incremental(c: &mut Criterion) {
    let mut group = c.benchmark_group("plh");
    for len in [1_024u32, 8_192, 32_768] {
        let tokens: Vec<u32> = (0..len).collect();
        group.throughput(Throughput::Elements(u64::from(len)));
        group.bench_with_input(BenchmarkId::new("avo_incremental", len), &tokens, |b, t| {
            b.iter(|| {
                let mut h = IncrementalHasher::new(16, None);
                for &tok in black_box(t) {
                    h.push(tok);
                }
                black_box(h.hashes().len())
            });
        });
    }
    group.finish();
}

criterion_group!(benches, dynamo_baseline, avo_one_pass, avo_incremental);
criterion_main!(benches);
