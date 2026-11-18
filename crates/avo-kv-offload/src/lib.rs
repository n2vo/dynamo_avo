//! Milestone 4: KV offload.
//!
//! Moves KV block payloads out of device memory to a host DRAM tier or a disk tier, and
//! restores them by block hash. Two flush policies share one storage interface so their I/O
//! cost can be compared directly:
//!
//! - [`UnalignedBlockWriter`]: One write per block, no padding (the naive baseline).
//! - [`CoalescedSlabWriter`]: Packs blocks into fixed slabs whose size is a multiple of
//!   4096 bytes, and writes each slab once, padded to a 4096-byte boundary.
//!
//! [`OffloadManager`] puts a bounded submission queue in front of either policy, with an
//! explicit backpressure rule, so a caller is never blocked by slow storage.
//!
//! Alignment here means storage offsets and write lengths are multiples of 4096 bytes, the
//! shape direct I/O requires. Buffer memory alignment is not controlled (that needs
//! `unsafe` allocation), and the disk tier writes through the page cache.
//!
//! Dynamo counterparts: `lib/kvbm-engine/src/offload` (pipeline, policy, batching) and
//! `lib/kvbm-physical` (physical layouts and transfers).

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

use avo_block_pool::BlockId;
use avo_kv_hash::SeqHash;

/// Key of one block: its chained sequence hash.
pub type BlockKey = SeqHash;

/// Storage alignment unit: one page.
pub const PAGE: usize = 4096;

/// Storage tier an offloaded block lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    HostDram,
    Disk,
}

impl Tier {
    fn as_str(self) -> &'static str {
        match self {
            Tier::HostDram => "host_dram",
            Tier::Disk => "disk",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OffloadError {
    #[error("storage I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("queue full: {free} free slots, {requested} requested")]
    QueueFull { free: usize, requested: usize },

    #[error("block not offloaded")]
    NotFound,

    #[error("block payload is {got} bytes, writer expects {expected}")]
    PayloadSize { expected: usize, got: usize },

    #[error("block of {len} bytes does not fit a {slab}-byte slab")]
    BlockTooLarge { len: usize, slab: usize },

    #[error("write of {len} bytes exceeds the {extent}-byte extent")]
    ExtentTooSmall { len: usize, extent: usize },

    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),
}

/// Fixed-size storage unit: one slab for the coalescing writer, one block for the baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExtentId(pub u64);

/// Where one offloaded block lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlabLocation {
    pub tier: Tier,
    pub slab_id: ExtentId,
    pub byte_offset: usize,
    pub byte_len: usize,
}

/// Block hash to location, for exact restore and for freeing slabs no live block uses.
pub type SlabManifest = HashMap<BlockKey, SlabLocation>;

/// Storage for fixed-size extents.
pub trait OffloadBackend {
    fn tier(&self) -> Tier;

    /// Size in bytes of every extent; the most one write may carry.
    fn extent_size(&self) -> usize;

    /// Writes `data` at the start of `extent`.
    fn write(&mut self, extent: ExtentId, data: &[u8]) -> Result<(), OffloadError>;

    /// Reads `out.len()` bytes from `extent`, starting `offset` bytes in.
    fn read(&self, extent: ExtentId, offset: usize, out: &mut [u8]) -> Result<(), OffloadError>;

    /// Releases an extent no live block uses.
    fn remove(&mut self, extent: ExtentId) -> Result<(), OffloadError>;
}

/// Host DRAM tier: One heap buffer per extent.
pub struct HostBackend {
    extent_size: usize,
    extents: HashMap<ExtentId, Vec<u8>>,
}

impl HostBackend {
    pub fn new(extent_size: usize) -> Result<Self, OffloadError> {
        if extent_size == 0 {
            return Err(OffloadError::InvalidConfig("extent size must not be zero"));
        }
        Ok(Self {
            extent_size,
            extents: HashMap::new(),
        })
    }
}

impl OffloadBackend for HostBackend {
    fn tier(&self) -> Tier {
        Tier::HostDram
    }

    fn extent_size(&self) -> usize {
        self.extent_size
    }

    fn write(&mut self, extent: ExtentId, data: &[u8]) -> Result<(), OffloadError> {
        check_len(data.len(), self.extent_size)?;
        self.extents.insert(extent, data.to_vec());
        Ok(())
    }

    fn read(&self, extent: ExtentId, offset: usize, out: &mut [u8]) -> Result<(), OffloadError> {
        let buf = self.extents.get(&extent).ok_or(OffloadError::NotFound)?;
        let src = buf
            .get(offset..offset + out.len())
            .ok_or(OffloadError::NotFound)?;
        out.copy_from_slice(src);
        Ok(())
    }

    fn remove(&mut self, extent: ExtentId) -> Result<(), OffloadError> {
        self.extents.remove(&extent);
        Ok(())
    }
}

/// Disk tier: One file, extent `n` at byte offset `n * extent_size`. Removed extents are
/// reused by later writes; the file does not shrink.
pub struct DiskBackend {
    extent_size: usize,
    file: File,
}

impl DiskBackend {
    /// Opens or creates the backing file, truncating it.
    pub fn create(path: &Path, extent_size: usize) -> Result<Self, OffloadError> {
        if extent_size == 0 {
            return Err(OffloadError::InvalidConfig("extent size must not be zero"));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self { extent_size, file })
    }

    fn offset(&self, extent: ExtentId) -> u64 {
        extent.0 * self.extent_size as u64
    }
}

impl OffloadBackend for DiskBackend {
    fn tier(&self) -> Tier {
        Tier::Disk
    }

    fn extent_size(&self) -> usize {
        self.extent_size
    }

    fn write(&mut self, extent: ExtentId, data: &[u8]) -> Result<(), OffloadError> {
        check_len(data.len(), self.extent_size)?;
        self.file.write_all_at(data, self.offset(extent))?;
        Ok(())
    }

    fn read(&self, extent: ExtentId, offset: usize, out: &mut [u8]) -> Result<(), OffloadError> {
        if offset + out.len() > self.extent_size {
            return Err(OffloadError::NotFound);
        }
        self.file
            .read_exact_at(out, self.offset(extent) + offset as u64)?;
        Ok(())
    }

    fn remove(&mut self, _extent: ExtentId) -> Result<(), OffloadError> {
        Ok(())
    }
}

fn check_len(len: usize, extent: usize) -> Result<(), OffloadError> {
    if len > extent {
        return Err(OffloadError::ExtentTooSmall { len, extent });
    }
    Ok(())
}

/// What one flush wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlushStats {
    /// Storage write operations issued.
    pub writes: usize,
    /// Bytes written, padding included.
    pub bytes_written: usize,
    /// Source device blocks whose payloads are now in the tier and can be released.
    pub offloaded: Vec<BlockId>,
}

/// A flush policy: Stages blocks, writes them to its backend, restores and evicts them.
pub trait FlushPolicy {
    fn tier(&self) -> Tier;

    /// Accepts one block. It becomes restorable at once; `flush` makes it durable in the
    /// tier. Offloading a key again replaces the earlier copy.
    fn stage(&mut self, key: BlockKey, source: BlockId, data: &[u8]) -> Result<(), OffloadError>;

    /// Writes everything staged.
    fn flush(&mut self) -> Result<FlushStats, OffloadError>;

    /// Copies the block's payload into a new buffer.
    fn restore(&self, key: &BlockKey) -> Result<Vec<u8>, OffloadError>;

    /// Drops the block from the manifest, and frees its extent once no live block uses it.
    fn evict(&mut self, key: &BlockKey) -> Result<(), OffloadError>;

    fn manifest(&self) -> &SlabManifest;
}

/// Reuses freed extent ids before allocating new ones.
#[derive(Default)]
struct ExtentAllocator {
    next: u64,
    free: Vec<ExtentId>,
}

impl ExtentAllocator {
    fn alloc(&mut self) -> ExtentId {
        self.free.pop().unwrap_or_else(|| {
            self.next += 1;
            ExtentId(self.next - 1)
        })
    }

    fn release(&mut self, id: ExtentId) {
        self.free.push(id);
    }
}

/// Baseline policy: Every block is its own extent and its own write, unpadded.
pub struct UnalignedBlockWriter<B: OffloadBackend> {
    backend: B,
    block_len: usize,
    manifest: SlabManifest,
    pending: Vec<(ExtentId, BlockId)>,
    staged: HashMap<ExtentId, Vec<u8>>,
    extents: ExtentAllocator,
}

impl<B: OffloadBackend> UnalignedBlockWriter<B> {
    /// `block_len` is the payload size of every block; the backend's extents must hold it.
    pub fn new(backend: B, block_len: usize) -> Result<Self, OffloadError> {
        if block_len == 0 {
            return Err(OffloadError::InvalidConfig("block length must not be zero"));
        }
        check_len(block_len, backend.extent_size())?;
        Ok(Self {
            backend,
            block_len,
            manifest: HashMap::new(),
            pending: Vec::new(),
            staged: HashMap::new(),
            extents: ExtentAllocator::default(),
        })
    }
}

impl<B: OffloadBackend> FlushPolicy for UnalignedBlockWriter<B> {
    fn tier(&self) -> Tier {
        self.backend.tier()
    }

    fn stage(&mut self, key: BlockKey, source: BlockId, data: &[u8]) -> Result<(), OffloadError> {
        if data.len() != self.block_len {
            return Err(OffloadError::PayloadSize {
                expected: self.block_len,
                got: data.len(),
            });
        }
        if self.manifest.contains_key(&key) {
            self.evict(&key)?;
        }
        let id = self.extents.alloc();
        self.staged.insert(id, data.to_vec());
        self.pending.push((id, source));
        self.manifest.insert(
            key,
            SlabLocation {
                tier: self.backend.tier(),
                slab_id: id,
                byte_offset: 0,
                byte_len: data.len(),
            },
        );
        Ok(())
    }

    fn flush(&mut self) -> Result<FlushStats, OffloadError> {
        let tier = self.backend.tier().as_str();
        let _span =
            tracing::debug_span!("offload_flush", tier, offload_blocks = self.pending.len())
                .entered();
        let mut stats = FlushStats::default();
        for (id, source) in std::mem::take(&mut self.pending) {
            let Some(data) = self.staged.remove(&id) else {
                continue;
            };
            self.backend.write(id, &data)?;
            stats.writes += 1;
            stats.bytes_written += data.len();
            stats.offloaded.push(source);
        }
        tracing::debug!(offload_bytes = stats.bytes_written, "offload_flushed");
        Ok(stats)
    }

    fn restore(&self, key: &BlockKey) -> Result<Vec<u8>, OffloadError> {
        let loc = self.manifest.get(key).ok_or(OffloadError::NotFound)?;
        let _span = tracing::trace_span!("offload_restore", tier = loc.tier.as_str()).entered();
        if let Some(data) = self.staged.get(&loc.slab_id) {
            return Ok(data.clone());
        }
        let mut out = vec![0; loc.byte_len];
        self.backend.read(loc.slab_id, 0, &mut out)?;
        Ok(out)
    }

    fn evict(&mut self, key: &BlockKey) -> Result<(), OffloadError> {
        let loc = self.manifest.remove(key).ok_or(OffloadError::NotFound)?;
        if self.staged.remove(&loc.slab_id).is_some() {
            self.pending.retain(|(id, _)| *id != loc.slab_id);
        } else {
            self.backend.remove(loc.slab_id)?;
        }
        self.extents.release(loc.slab_id);
        Ok(())
    }

    fn manifest(&self) -> &SlabManifest {
        &self.manifest
    }
}

/// Slab being filled.
struct OpenSlab {
    id: ExtentId,
    buf: Vec<u8>,
    sources: Vec<(BlockKey, BlockId)>,
}

/// Optimized policy: Packs blocks into slabs and writes each slab once, padded to a page
/// boundary. A block never spans two slabs; a block that does not fit seals the open slab.
pub struct CoalescedSlabWriter<B: OffloadBackend> {
    backend: B,
    slab_size: usize,
    manifest: SlabManifest,
    open: Option<OpenSlab>,
    /// Live blocks per slab, open or sealed. A sealed slab at zero is freed.
    live: HashMap<ExtentId, usize>,
    extents: ExtentAllocator,
    /// Writes and bytes from slabs sealed by `stage`, reported at the next `flush`.
    sealed: FlushStats,
}

impl<B: OffloadBackend> CoalescedSlabWriter<B> {
    /// The slab size is the backend's extent size and must be a positive multiple of 4096.
    pub fn new(backend: B) -> Result<Self, OffloadError> {
        let slab_size = backend.extent_size();
        if slab_size == 0 || !slab_size.is_multiple_of(PAGE) {
            return Err(OffloadError::InvalidConfig(
                "slab size must be a positive multiple of 4096",
            ));
        }
        Ok(Self {
            backend,
            slab_size,
            manifest: HashMap::new(),
            open: None,
            live: HashMap::new(),
            extents: ExtentAllocator::default(),
            sealed: FlushStats::default(),
        })
    }

    /// Writes the open slab, padded to the next page boundary.
    fn seal(&mut self) -> Result<(), OffloadError> {
        let Some(mut slab) = self.open.take() else {
            return Ok(());
        };
        let padded = slab.buf.len().div_ceil(PAGE) * PAGE;
        slab.buf.resize(padded, 0);
        let _span = tracing::debug_span!(
            "offload_coalesce",
            tier = self.backend.tier().as_str(),
            slab_id = slab.id.0,
            offload_blocks = slab.sources.len(),
            offload_bytes = padded
        )
        .entered();
        self.backend.write(slab.id, &slab.buf)?;
        self.sealed.writes += 1;
        self.sealed.bytes_written += padded;
        self.sealed
            .offloaded
            .extend(slab.sources.iter().map(|&(_, source)| source));
        if self.live.get(&slab.id) == Some(&0) {
            self.free_slab(slab.id)?;
        }
        Ok(())
    }

    fn free_slab(&mut self, id: ExtentId) -> Result<(), OffloadError> {
        self.live.remove(&id);
        self.backend.remove(id)?;
        self.extents.release(id);
        Ok(())
    }
}

impl<B: OffloadBackend> FlushPolicy for CoalescedSlabWriter<B> {
    fn tier(&self) -> Tier {
        self.backend.tier()
    }

    fn stage(&mut self, key: BlockKey, source: BlockId, data: &[u8]) -> Result<(), OffloadError> {
        if data.len() > self.slab_size {
            return Err(OffloadError::BlockTooLarge {
                len: data.len(),
                slab: self.slab_size,
            });
        }
        if self.manifest.contains_key(&key) {
            self.evict(&key)?;
        }
        let fits = self
            .open
            .as_ref()
            .is_some_and(|s| s.buf.len() + data.len() <= self.slab_size);
        if !fits {
            self.seal()?;
        }
        let slab = match &mut self.open {
            Some(s) => s,
            None => {
                let id = self.extents.alloc();
                self.open.insert(OpenSlab {
                    id,
                    buf: Vec::with_capacity(self.slab_size),
                    sources: Vec::new(),
                })
            }
        };
        let byte_offset = slab.buf.len();
        slab.buf.extend_from_slice(data);
        slab.sources.push((key, source));
        let slab_id = slab.id;
        *self.live.entry(slab_id).or_insert(0) += 1;
        self.manifest.insert(
            key,
            SlabLocation {
                tier: self.backend.tier(),
                slab_id,
                byte_offset,
                byte_len: data.len(),
            },
        );
        Ok(())
    }

    fn flush(&mut self) -> Result<FlushStats, OffloadError> {
        let _span =
            tracing::debug_span!("offload_flush", tier = self.backend.tier().as_str()).entered();
        self.seal()?;
        let stats = std::mem::take(&mut self.sealed);
        tracing::debug!(offload_bytes = stats.bytes_written, "offload_flushed");
        Ok(stats)
    }

    fn restore(&self, key: &BlockKey) -> Result<Vec<u8>, OffloadError> {
        let loc = self.manifest.get(key).ok_or(OffloadError::NotFound)?;
        let _span = tracing::trace_span!(
            "offload_restore",
            tier = loc.tier.as_str(),
            slab_id = loc.slab_id.0
        )
        .entered();
        if let Some(open) = self.open.as_ref().filter(|s| s.id == loc.slab_id) {
            let src = open
                .buf
                .get(loc.byte_offset..loc.byte_offset + loc.byte_len)
                .ok_or(OffloadError::NotFound)?;
            return Ok(src.to_vec());
        }
        let mut out = vec![0; loc.byte_len];
        self.backend.read(loc.slab_id, loc.byte_offset, &mut out)?;
        Ok(out)
    }

    fn evict(&mut self, key: &BlockKey) -> Result<(), OffloadError> {
        let loc = self.manifest.remove(key).ok_or(OffloadError::NotFound)?;
        let count = self.live.entry(loc.slab_id).or_insert(0);
        *count = count.saturating_sub(1);
        let count = *count;
        let is_open = match &mut self.open {
            Some(s) if s.id == loc.slab_id => {
                // Its bytes stay as dead space in the slab, but it is no longer offloaded.
                s.sources.retain(|(k, _)| k != key);
                true
            }
            _ => false,
        };
        if count == 0 && !is_open {
            self.free_slab(loc.slab_id)?;
        }
        Ok(())
    }

    fn manifest(&self) -> &SlabManifest {
        &self.manifest
    }
}

/// What the queue does when a submission would overflow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backpressure {
    /// Refuse the whole batch; nothing is queued.
    RejectWhenFull,
    /// Drop the oldest queued blocks, coldest by submission order, to make room.
    DropOldestCold,
}

/// One block to offload.
#[derive(Clone, Copy, Debug)]
pub struct OffloadRequest<'a> {
    pub key: BlockKey,
    pub source: BlockId,
    pub data: &'a [u8],
}

/// What `offload_blocks` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubmitReport {
    pub accepted: usize,
    /// Blocks dropped from the queue under `DropOldestCold`; they were never offloaded.
    pub dropped: Vec<BlockKey>,
}

struct Queued {
    key: BlockKey,
    source: BlockId,
    data: Vec<u8>,
}

/// Bounded offload pipeline in front of one flush policy. Submission never waits on
/// storage: it queues, rejects, or drops, per the backpressure rule. `flush` drains the
/// queue into the policy and writes it out.
pub struct OffloadManager<P: FlushPolicy> {
    policy: P,
    queue: VecDeque<Queued>,
    capacity: usize,
    backpressure: Backpressure,
}

impl<P: FlushPolicy> OffloadManager<P> {
    pub fn new(
        policy: P,
        capacity: usize,
        backpressure: Backpressure,
    ) -> Result<Self, OffloadError> {
        if capacity == 0 {
            return Err(OffloadError::InvalidConfig(
                "queue capacity must not be zero",
            ));
        }
        Ok(Self {
            policy,
            queue: VecDeque::with_capacity(capacity),
            capacity,
            backpressure,
        })
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    pub fn policy(&self) -> &P {
        &self.policy
    }

    /// Queues blocks for offload.
    pub fn offload_blocks(
        &mut self,
        blocks: &[OffloadRequest<'_>],
    ) -> Result<SubmitReport, OffloadError> {
        let _span = tracing::debug_span!(
            "offload_enqueue",
            tier = self.policy.tier().as_str(),
            offload_blocks = blocks.len(),
            queue_depth = self.queue.len()
        )
        .entered();
        let free = self.capacity - self.queue.len();
        if self.backpressure == Backpressure::RejectWhenFull && blocks.len() > free {
            return Err(OffloadError::QueueFull {
                free,
                requested: blocks.len(),
            });
        }
        let mut report = SubmitReport::default();
        for b in blocks {
            if self.queue.len() == self.capacity
                && let Some(old) = self.queue.pop_front()
            {
                report.dropped.push(old.key);
            }
            self.queue.push_back(Queued {
                key: b.key,
                source: b.source,
                data: b.data.to_vec(),
            });
            report.accepted += 1;
        }
        if !report.dropped.is_empty() {
            tracing::debug!(dropped = report.dropped.len(), "offload_backpressure_drop");
        }
        Ok(report)
    }

    /// Stages every queued block in submission order and flushes the policy.
    pub fn flush(&mut self) -> Result<FlushStats, OffloadError> {
        while let Some(q) = self.queue.pop_front() {
            self.policy.stage(q.key, q.source, &q.data)?;
        }
        self.policy.flush()
    }

    /// The block's payload: the newest queued copy, else the offloaded one.
    pub fn restore_block(&self, key: &BlockKey) -> Result<Vec<u8>, OffloadError> {
        if let Some(q) = self.queue.iter().rev().find(|q| q.key == *key) {
            return Ok(q.data.clone());
        }
        self.policy.restore(key)
    }

    /// Removes the block from the queue and from the tier.
    pub fn evict_offloaded(&mut self, key: &BlockKey) -> Result<(), OffloadError> {
        let before = self.queue.len();
        self.queue.retain(|q| q.key != *key);
        let queued = self.queue.len() < before;
        match self.policy.evict(key) {
            Err(OffloadError::NotFound) if queued => Ok(()),
            other => other,
        }
    }
}
