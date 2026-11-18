//! Proving test for Milestone 4: Every block offloaded through the bounded queue restores
//! bit for bit from both tiers and both flush policies, under random offloads, flushes,
//! restores, and evictions with either backpressure rule; every coalesced write is
//! page-aligned. Unit tests pin slab boundaries and backpressure.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use avo_block_pool::BlockId;
use avo_kv_hash::hash_sequence;
use avo_kv_offload::{
    Backpressure, BlockKey, CoalescedSlabWriter, DiskBackend, ExtentId, FlushPolicy, HostBackend,
    OffloadBackend, OffloadError, OffloadManager, OffloadRequest, PAGE, Tier, UnalignedBlockWriter,
};
use proptest::prelude::*;

fn key(n: u32) -> BlockKey {
    hash_sequence(&[n], 1, None)[0]
}

fn payload(seed: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (seed as usize * 31 + i * 7) as u8)
        .collect()
}

/// Temporary backing file, removed on drop.
struct TempFile(PathBuf);

impl TempFile {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "avo-kv-offload-test-{}-{n}.bin",
            std::process::id()
        )))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Backend wrapper that records the length of every write.
struct Recording<B> {
    inner: B,
    writes: Rc<RefCell<Vec<usize>>>,
}

impl<B: OffloadBackend> OffloadBackend for Recording<B> {
    fn tier(&self) -> Tier {
        self.inner.tier()
    }
    fn extent_size(&self) -> usize {
        self.inner.extent_size()
    }
    fn write(&mut self, extent: ExtentId, data: &[u8]) -> Result<(), OffloadError> {
        self.writes.borrow_mut().push(data.len());
        self.inner.write(extent, data)
    }
    fn read(&self, extent: ExtentId, offset: usize, out: &mut [u8]) -> Result<(), OffloadError> {
        self.inner.read(extent, offset, out)
    }
    fn remove(&mut self, extent: ExtentId) -> Result<(), OffloadError> {
        self.inner.remove(extent)
    }
}

fn host_slabs(slab: usize) -> CoalescedSlabWriter<HostBackend> {
    CoalescedSlabWriter::new(HostBackend::new(slab).expect("backend")).expect("writer")
}

fn req(k: u32, data: &[u8]) -> OffloadRequest<'_> {
    OffloadRequest {
        key: key(k),
        source: BlockId(k),
        data,
    }
}

#[test]
fn round_trip_every_tier_and_policy() {
    let file_a = TempFile::new();
    let file_b = TempFile::new();
    let blocks: Vec<Vec<u8>> = (0..10).map(|i| payload(i, 3000)).collect();
    let mut policies: Vec<Box<dyn FlushPolicy>> = vec![
        Box::new(host_slabs(8192)),
        Box::new(
            CoalescedSlabWriter::new(DiskBackend::create(&file_a.0, 8192).expect("file"))
                .expect("writer"),
        ),
        Box::new(
            UnalignedBlockWriter::new(HostBackend::new(3000).expect("backend"), 3000)
                .expect("writer"),
        ),
        Box::new(
            UnalignedBlockWriter::new(DiskBackend::create(&file_b.0, 3000).expect("file"), 3000)
                .expect("writer"),
        ),
    ];
    for p in &mut policies {
        for (i, b) in blocks.iter().enumerate() {
            p.stage(key(i as u32), BlockId(i as u32), b).expect("stage");
        }
        let stats = p.flush().expect("flush");
        assert_eq!(stats.offloaded.len(), 10);
        for (i, b) in blocks.iter().enumerate() {
            assert_eq!(&p.restore(&key(i as u32)).expect("restore"), b);
        }
    }
}

#[test]
fn partial_slab_flush_pads_to_a_page() {
    let mut w = host_slabs(8192);
    for i in 0..3 {
        w.stage(key(i), BlockId(i), &payload(i, 1000))
            .expect("stage");
    }
    let stats = w.flush().expect("flush");
    assert_eq!(
        (stats.writes, stats.bytes_written),
        (1, PAGE),
        "3,000 bytes pad to 4,096"
    );
    for i in 0..3 {
        assert_eq!(w.restore(&key(i)).expect("restore"), payload(i, 1000));
    }
}

#[test]
fn batch_spanning_two_slabs() {
    let writes = Rc::new(RefCell::new(Vec::new()));
    let backend = Recording {
        inner: HostBackend::new(8192).expect("backend"),
        writes: Rc::clone(&writes),
    };
    let mut w = CoalescedSlabWriter::new(backend).expect("writer");
    for i in 0..3 {
        w.stage(key(i), BlockId(i), &payload(i, 3000))
            .expect("stage");
    }
    let stats = w.flush().expect("flush");
    assert_eq!(
        *writes.borrow(),
        vec![8192, 4096],
        "full slab, then a padded partial one"
    );
    assert_eq!(stats.writes, 2);
    let m = w.manifest();
    assert_eq!(m[&key(0)].slab_id, m[&key(1)].slab_id);
    assert_ne!(
        m[&key(1)].slab_id,
        m[&key(2)].slab_id,
        "the third block starts a new slab"
    );
    assert_eq!(m[&key(2)].byte_offset, 0);
    for i in 0..3 {
        assert_eq!(w.restore(&key(i)).expect("restore"), payload(i, 3000));
    }
}

#[test]
fn evicting_every_block_frees_its_slab_for_reuse() {
    let mut w = host_slabs(8192);
    w.stage(key(0), BlockId(0), &payload(0, 100))
        .expect("stage");
    w.stage(key(1), BlockId(1), &payload(1, 100))
        .expect("stage");
    w.flush().expect("flush");
    let slab = w.manifest()[&key(0)].slab_id;
    w.evict(&key(0)).expect("evict");
    assert_eq!(w.restore(&key(1)).expect("restore"), payload(1, 100));
    w.evict(&key(1)).expect("evict");
    assert!(w.manifest().is_empty());
    w.stage(key(2), BlockId(2), &payload(2, 100))
        .expect("stage");
    assert_eq!(
        w.manifest()[&key(2)].slab_id,
        slab,
        "the freed slab id is reused"
    );
    assert!(matches!(w.evict(&key(0)), Err(OffloadError::NotFound)));
}

#[test]
fn evicted_staged_block_is_not_written_or_reported() {
    let mut w =
        UnalignedBlockWriter::new(HostBackend::new(64).expect("backend"), 64).expect("writer");
    w.stage(key(0), BlockId(0), &payload(0, 64)).expect("stage");
    w.evict(&key(0)).expect("evict");
    w.stage(key(1), BlockId(1), &payload(1, 64)).expect("stage");
    let stats = w.flush().expect("flush");
    assert_eq!(stats.writes, 1);
    assert_eq!(stats.offloaded, vec![BlockId(1)]);
}

#[test]
fn configuration_and_size_errors() {
    assert!(matches!(
        CoalescedSlabWriter::new(HostBackend::new(5000).expect("backend")),
        Err(OffloadError::InvalidConfig(_))
    ));
    let mut w = host_slabs(4096);
    assert!(matches!(
        w.stage(key(0), BlockId(0), &payload(0, 5000)),
        Err(OffloadError::BlockTooLarge {
            len: 5000,
            slab: 4096
        })
    ));
    let mut u =
        UnalignedBlockWriter::new(HostBackend::new(64).expect("backend"), 64).expect("writer");
    assert!(matches!(
        u.stage(key(0), BlockId(0), &payload(0, 63)),
        Err(OffloadError::PayloadSize {
            expected: 64,
            got: 63
        })
    ));
    assert!(matches!(
        OffloadManager::new(host_slabs(4096), 0, Backpressure::RejectWhenFull),
        Err(OffloadError::InvalidConfig(_))
    ));
}

#[test]
fn reject_when_full_refuses_the_whole_batch() {
    let mut m =
        OffloadManager::new(host_slabs(8192), 2, Backpressure::RejectWhenFull).expect("manager");
    let (a, b, c) = (payload(0, 10), payload(1, 10), payload(2, 10));
    assert!(matches!(
        m.offload_blocks(&[req(0, &a), req(1, &b), req(2, &c)]),
        Err(OffloadError::QueueFull {
            free: 2,
            requested: 3
        })
    ));
    assert_eq!(
        m.queue_len(),
        0,
        "nothing from the rejected batch is queued"
    );
    m.offload_blocks(&[req(0, &a), req(1, &b)]).expect("fits");
    assert!(matches!(
        m.offload_blocks(&[req(2, &c)]),
        Err(OffloadError::QueueFull {
            free: 0,
            requested: 1
        })
    ));
    m.flush().expect("flush");
    assert_eq!(m.policy().manifest().len(), 2);
    assert_eq!(m.restore_block(&key(1)).expect("restore"), b);
}

#[test]
fn drop_oldest_cold_drops_the_oldest_queued_block() {
    let mut m =
        OffloadManager::new(host_slabs(8192), 2, Backpressure::DropOldestCold).expect("manager");
    let (a, b, c) = (payload(0, 10), payload(1, 10), payload(2, 10));
    let report = m
        .offload_blocks(&[req(0, &a), req(1, &b), req(2, &c)])
        .expect("never rejects");
    assert_eq!(report.accepted, 3);
    assert_eq!(report.dropped, vec![key(0)]);
    assert!(matches!(
        m.restore_block(&key(0)),
        Err(OffloadError::NotFound)
    ));
    let stats = m.flush().expect("flush");
    assert_eq!(stats.offloaded, vec![BlockId(1), BlockId(2)]);
    let keys: HashSet<BlockKey> = m.policy().manifest().keys().copied().collect();
    assert_eq!(keys, HashSet::from([key(1), key(2)]));
}

#[test]
fn queued_blocks_restore_and_evict_before_flush() {
    let mut m =
        OffloadManager::new(host_slabs(8192), 4, Backpressure::RejectWhenFull).expect("manager");
    let a = payload(0, 10);
    m.offload_blocks(&[req(0, &a)]).expect("queue");
    assert_eq!(m.restore_block(&key(0)).expect("from the queue"), a);
    m.evict_offloaded(&key(0)).expect("evict");
    assert_eq!(m.queue_len(), 0);
    assert!(matches!(
        m.evict_offloaded(&key(0)),
        Err(OffloadError::NotFound)
    ));
}

#[test]
fn trace_fields_match_avo_obs() {
    use avo_obs::fields;
    assert_eq!(fields::TIER, "tier");
    assert_eq!(fields::SLAB_ID, "slab_id");
    assert_eq!(fields::OFFLOAD_BLOCKS, "offload_blocks");
    assert_eq!(fields::OFFLOAD_BYTES, "offload_bytes");
    assert_eq!(fields::QUEUE_DEPTH, "queue_depth");
}

#[derive(Clone, Debug)]
enum Op {
    Offload(Vec<(u32, u32, usize)>),
    Flush,
    Restore(u32),
    Evict(u32),
}

fn ops(fixed_len: Option<usize>) -> impl Strategy<Value = Vec<Op>> {
    let len = match fixed_len {
        Some(n) => Just(n).boxed(),
        None => (1usize..3000).boxed(),
    };
    let block = (0u32..12, any::<u32>(), len);
    let op = prop_oneof![
        3 => prop::collection::vec(block, 1..5).prop_map(Op::Offload),
        2 => Just(Op::Flush),
        3 => (0u32..12).prop_map(Op::Restore),
        1 => (0u32..12).prop_map(Op::Evict),
    ];
    prop::collection::vec(op, 0..40)
}

/// Runs `ops` against the manager and a model of its queue and its flushed blocks.
fn check<P: FlushPolicy>(
    mut m: OffloadManager<P>,
    ops: Vec<Op>,
    capacity: usize,
    rule: Backpressure,
    writes: &Rc<RefCell<Vec<usize>>>,
    aligned: bool,
    slab: usize,
) -> Result<(), TestCaseError> {
    let mut queue: VecDeque<(BlockKey, Vec<u8>)> = VecDeque::new();
    let mut flushed: HashMap<BlockKey, Vec<u8>> = HashMap::new();

    for op in ops {
        match op {
            Op::Offload(blocks) => {
                let data: Vec<Vec<u8>> = blocks.iter().map(|&(_, s, l)| payload(s, l)).collect();
                let reqs: Vec<OffloadRequest<'_>> = blocks
                    .iter()
                    .zip(&data)
                    .map(|(&(k, _, _), d)| req(k, d))
                    .collect();
                match m.offload_blocks(&reqs) {
                    Ok(report) => {
                        let mut dropped = Vec::new();
                        for (r, d) in reqs.iter().zip(&data) {
                            if queue.len() == capacity
                                && let Some((k, _)) = queue.pop_front()
                            {
                                dropped.push(k);
                            }
                            queue.push_back((r.key, d.clone()));
                        }
                        prop_assert_eq!(report.dropped, dropped);
                    }
                    Err(OffloadError::QueueFull { .. }) => {
                        prop_assert_eq!(rule, Backpressure::RejectWhenFull);
                        prop_assert!(queue.len() + reqs.len() > capacity);
                    }
                    Err(e) => prop_assert!(false, "unexpected error {e}"),
                }
            }
            Op::Flush => {
                m.flush().map_err(|e| TestCaseError::fail(e.to_string()))?;
                for (k, d) in queue.drain(..) {
                    flushed.insert(k, d);
                }
            }
            Op::Restore(k) => {
                let k = key(k);
                let want = queue
                    .iter()
                    .rev()
                    .find(|(q, _)| *q == k)
                    .map(|(_, d)| d.clone())
                    .or_else(|| flushed.get(&k).cloned());
                match (m.restore_block(&k), want) {
                    (Ok(got), Some(want)) => prop_assert_eq!(got, want),
                    (Err(OffloadError::NotFound), None) => {}
                    (got, want) => prop_assert!(false, "got {got:?}, want {want:?}"),
                }
            }
            Op::Evict(k) => {
                let k = key(k);
                let had = queue.iter().any(|(q, _)| *q == k) | flushed.remove(&k).is_some();
                queue.retain(|(q, _)| *q != k);
                match m.evict_offloaded(&k) {
                    Ok(()) => prop_assert!(had),
                    Err(OffloadError::NotFound) => prop_assert!(!had),
                    Err(e) => prop_assert!(false, "unexpected error {e}"),
                }
            }
        }

        prop_assert_eq!(m.queue_len(), queue.len());
        let manifest = m.policy().manifest();
        let keys: HashSet<BlockKey> = manifest.keys().copied().collect();
        let want: HashSet<BlockKey> = flushed.keys().copied().collect();
        prop_assert_eq!(keys, want, "manifest holds exactly the flushed blocks");
        for loc in manifest.values() {
            prop_assert!(loc.byte_offset + loc.byte_len <= slab);
        }
        if aligned {
            for len in writes.borrow().iter() {
                prop_assert!(len.is_multiple_of(PAGE), "unaligned slab write");
            }
        }
    }
    Ok(())
}

fn rule() -> impl Strategy<Value = Backpressure> {
    prop_oneof![
        Just(Backpressure::RejectWhenFull),
        Just(Backpressure::DropOldestCold)
    ]
}

const SLAB: usize = 8192;
const BLOCK: usize = 512;
const CAPACITY: usize = 6;

proptest! {
    #[test]
    fn coalesced_host_round_trips(ops in ops(None), rule in rule()) {
        let writes = Rc::new(RefCell::new(Vec::new()));
        let backend = Recording { inner: HostBackend::new(SLAB).expect("backend"), writes: Rc::clone(&writes) };
        let m = OffloadManager::new(CoalescedSlabWriter::new(backend).expect("writer"), CAPACITY, rule)
            .expect("manager");
        check(m, ops, CAPACITY, rule, &writes, true, SLAB)?;
    }

    #[test]
    fn coalesced_disk_round_trips(ops in ops(None), rule in rule()) {
        let file = TempFile::new();
        let writes = Rc::new(RefCell::new(Vec::new()));
        let backend = Recording { inner: DiskBackend::create(&file.0, SLAB).expect("file"), writes: Rc::clone(&writes) };
        let m = OffloadManager::new(CoalescedSlabWriter::new(backend).expect("writer"), CAPACITY, rule)
            .expect("manager");
        check(m, ops, CAPACITY, rule, &writes, true, SLAB)?;
    }

    #[test]
    fn unaligned_host_round_trips(ops in ops(Some(BLOCK)), rule in rule()) {
        let writes = Rc::new(RefCell::new(Vec::new()));
        let w = UnalignedBlockWriter::new(HostBackend::new(BLOCK).expect("backend"), BLOCK).expect("writer");
        let m = OffloadManager::new(w, CAPACITY, rule).expect("manager");
        check(m, ops, CAPACITY, rule, &writes, false, BLOCK)?;
    }

    #[test]
    fn unaligned_disk_round_trips(ops in ops(Some(BLOCK)), rule in rule()) {
        let file = TempFile::new();
        let writes = Rc::new(RefCell::new(Vec::new()));
        let w = UnalignedBlockWriter::new(DiskBackend::create(&file.0, BLOCK).expect("file"), BLOCK)
            .expect("writer");
        let m = OffloadManager::new(w, CAPACITY, rule).expect("manager");
        check(m, ops, CAPACITY, rule, &writes, false, BLOCK)?;
    }
}
