//! Milestone 4 benchmark: Per-block writes against coalesced slab writes, on the host DRAM
//! tier and the disk tier.
//!
//! - `offload`: Stage and flush 64 MiB or 256 MiB of 32 KiB blocks. `unaligned` issues one
//!   32 KiB write per block; `slab_1m`, `slab_2m`, `slab_4m` pack blocks into 1, 2, or
//!   4 MiB slabs and issue one page-aligned write per slab. Throughput in bytes.
//! - `flush_one`: Time to make one write unit durable in the tier: one 32 KiB block for
//!   `unaligned`, one full slab for the slab writers. The latency side of the trade-off.
//! - `restore_hit`: Restore one 32 KiB block from a populated 64 MiB tier, round robin.
//!
//! The disk tier is a file in the system temporary directory, written through the page
//! cache; nothing here forces data to the device.

use std::hint::black_box;
use std::path::PathBuf;

use avo_block_pool::BlockId;
use avo_kv_hash::hash_sequence;
use avo_kv_offload::{
    BlockKey, CoalescedSlabWriter, DiskBackend, FlushPolicy, HostBackend, UnalignedBlockWriter,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const BLOCK: usize = 32 * KIB;
const DATA_MIB: [usize; 2] = [64, 256];
const SLABS: [(&str, usize); 3] = [("slab_1m", MIB), ("slab_2m", 2 * MIB), ("slab_4m", 4 * MIB)];

#[derive(Clone, Copy)]
enum TierKind {
    Host,
    Disk,
}

impl TierKind {
    fn name(self) -> &'static str {
        match self {
            TierKind::Host => "host",
            TierKind::Disk => "disk",
        }
    }
}

const TIERS: [TierKind; 2] = [TierKind::Host, TierKind::Disk];

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "avo-kv-offload-bench-{}-{name}.bin",
        std::process::id()
    ))
}

/// A policy over either tier, boxed so every configuration shares one loop.
fn policy(tier: TierKind, slab: Option<usize>, file: &str) -> Box<dyn FlushPolicy> {
    let extent = slab.unwrap_or(BLOCK);
    match (tier, slab) {
        (TierKind::Host, Some(_)) => Box::new(
            CoalescedSlabWriter::new(HostBackend::new(extent).expect("backend")).expect("writer"),
        ),
        (TierKind::Host, None) => Box::new(
            UnalignedBlockWriter::new(HostBackend::new(extent).expect("backend"), BLOCK)
                .expect("writer"),
        ),
        (TierKind::Disk, Some(_)) => {
            Box::new(CoalescedSlabWriter::new(disk(extent, file)).expect("writer"))
        }
        (TierKind::Disk, None) => {
            Box::new(UnalignedBlockWriter::new(disk(extent, file), BLOCK).expect("writer"))
        }
    }
}

fn disk(extent: usize, file: &str) -> DiskBackend {
    DiskBackend::create(&temp_path(file), extent).expect("file")
}

fn keys(n: usize) -> Vec<BlockKey> {
    hash_sequence(&(0..n as u32).collect::<Vec<_>>(), 1, None)
}

fn configs() -> Vec<(&'static str, Option<usize>)> {
    let mut c = vec![("unaligned", None)];
    c.extend(SLABS.iter().map(|&(n, s)| (n, Some(s))));
    c
}

fn offload(c: &mut Criterion) {
    let block = vec![0xA5u8; BLOCK];
    let mut group = c.benchmark_group("offload");
    group.sample_size(10);
    for tier in TIERS {
        for mib in DATA_MIB {
            let n = mib * MIB / BLOCK;
            let ks = keys(n);
            group.throughput(Throughput::Bytes((mib * MIB) as u64));
            for (name, slab) in configs() {
                let id = format!("{}_{name}_{mib}mib", tier.name());
                group.bench_function(BenchmarkId::from_parameter(&id), |b| {
                    b.iter_batched(
                        || policy(tier, slab, &id),
                        |mut p| {
                            for (i, k) in ks.iter().enumerate() {
                                p.stage(*k, BlockId(i as u32), &block).expect("stage");
                            }
                            black_box(p.flush().expect("flush"))
                        },
                        BatchSize::PerIteration,
                    )
                });
                let _ = std::fs::remove_file(temp_path(&id));
            }
        }
    }
    group.finish();
}

fn flush_one(c: &mut Criterion) {
    let block = vec![0xA5u8; BLOCK];
    let mut group = c.benchmark_group("flush_one");
    for tier in TIERS {
        for (name, slab) in configs() {
            let unit = slab.unwrap_or(BLOCK);
            let n = unit / BLOCK;
            let ks = keys(n);
            group.throughput(Throughput::Bytes(unit as u64));
            let id = format!("{}_{name}", tier.name());
            let mut p = policy(tier, slab, &id);
            group.bench_function(BenchmarkId::from_parameter(&id), |b| {
                b.iter(|| {
                    for (i, k) in ks.iter().enumerate() {
                        p.stage(*k, BlockId(i as u32), &block).expect("stage");
                    }
                    black_box(p.flush().expect("flush"))
                })
            });
            drop(p);
            let _ = std::fs::remove_file(temp_path(&id));
        }
    }
    group.finish();
}

fn restore_hit(c: &mut Criterion) {
    let block = vec![0xA5u8; BLOCK];
    let n = 64 * MIB / BLOCK;
    let ks = keys(n);
    let mut group = c.benchmark_group("restore_hit");
    group.throughput(Throughput::Bytes(BLOCK as u64));
    for tier in TIERS {
        for (name, slab) in [("unaligned", None), ("slab_2m", Some(2 * MIB))] {
            let id = format!("{}_{name}", tier.name());
            let mut p = policy(tier, slab, &id);
            for (i, k) in ks.iter().enumerate() {
                p.stage(*k, BlockId(i as u32), &block).expect("stage");
            }
            p.flush().expect("flush");
            let mut i = 0;
            group.bench_function(BenchmarkId::from_parameter(&id), |b| {
                b.iter(|| {
                    i = (i + 1) % n;
                    black_box(p.restore(&ks[i]).expect("restore"))
                })
            });
            drop(p);
            let _ = std::fs::remove_file(temp_path(&id));
        }
    }
    group.finish();
}

criterion_group!(benches, offload, flush_one, restore_hit);
criterion_main!(benches);
