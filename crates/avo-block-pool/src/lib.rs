//! Milestone 2: Block pool.
//!
//! Fixed-capacity allocator with reference-counted shared prefix blocks, LRU reuse of
//! finished blocks, and RAII handles that return blocks through `Drop`.
//!
//! Dynamo counterpart: `lib/kvbm-logical` (`pools`, `blocks`, `manager`, `tinylfu.rs`).
//!
//! Proving test: no block is leaked, and no block is freed twice.
//!
//! Block lifecycle:
//!
//! ```text
//! Free ──allocate──▶ Active (uncommitted) ──commit(hash)──▶ Active (committed)
//!  ▲                      │                                       │
//!  │        last handle dropped                     last handle dropped
//!  │                      │                                       ▼
//!  └──────────────────────┘                              Inactive (cached by hash)
//!  ▲                                                              │
//!  └──────────── evicted (LRU) when the free list is empty ────────┘
//!                                                        lookup(hash) ──▶ Active (committed)
//! ```
//!
//! Every block is in exactly one of Free, Active, or Inactive at all times, so
//! `active + inactive + free == capacity` is the leak invariant, and a block id appearing in
//! more than one set, or twice in one set, is the double-free invariant.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Index of a block in the pool, `0..capacity`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PoolError {
    /// No free block and no inactive block to evict: Every block is held by a live handle.
    #[error("pool exhausted: no free or evictable block")]
    Exhausted,

    #[error("block already committed")]
    AlreadyCommitted,
}

/// Block counts. `active + inactive + free == capacity` always.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStats {
    pub capacity: usize,

    /// Held by at least one live handle.
    pub active: usize,

    /// Commited, no live handle, cached for `lookup`, evictable.
    pub inactive: usize,

    /// Never committed or evicted, available to `allocate`.
    pub free: usize,
}

/// Every block Id by state, for invariant checks in tests. Order within a vector is not
/// part of the contract.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Audit {
    pub active: Vec<BlockId>,
    pub inactive: Vec<BlockId>,
    pub free: Vec<BlockId>,
}

/// Where one block is in the lifecycle. Every block is in exactly one state; the
/// transitions are the diagram at the beginning of this file.
enum BlockState {
    /// On the free list. Never committed or evicted.
    Free,

    /// Held by `refs` live handles. `hash` is `None` until `commit`.
    Active { refs: usize, hash: Option<u64> },

    /// Committed, no live handle, cached for `lookup`. `last_use` is the pool tick at
    /// the last drop or lookup; eviction takes the smallest.
    Inactive { hash: u64, last_use: u64 },
}

/// Internal pool state behind the lock
struct PoolState {
    // Per-id state: Free | Active { refs, hash } | Inactive { hash, last_use }
    blocks: Vec<BlockState>,

    // Free list
    free: Vec<BlockId>,

    // Committed blocks, active or inactive, for lookup
    by_hash: HashMap<u64, BlockId>,

    // monotonic counter for LRU ordering
    tick: u64,
}

impl PoolState {
    fn idx(id: BlockId) -> usize {
        id.0 as usize
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Evicts the least recently used inactive block and returns its id, now Free but not
    /// on the free list (the caller takes it directly). `None` if nothing is inactive.
    fn evict_lru(&mut self) -> Option<BlockId> {
        let (i, hash) = self
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| match b {
                BlockState::Inactive { hash, last_use } => Some((i, *hash, *last_use)),
                _ => None,
            })
            .min_by_key(|&(_, _, last_use)| last_use)
            .map(|(i, hash, _)| (i, hash))?;
        self.blocks[i] = BlockState::Free;
        self.by_hash.remove(&hash);
        Some(BlockId(i as u32))
    }

    /// One handle released. The last release moves the block to Inactive if committed or
    /// back to Free if not. Panics if the block is not Active, which is a double free.
    fn release(&mut self, id: BlockId) {
        let i = Self::idx(id);
        let BlockState::Active { refs, hash } = &mut self.blocks[i] else {
            panic!("double free: block {id:?} released while not active");
        };
        *refs -= 1;
        if *refs > 0 {
            return;
        }
        match *hash {
            Some(hash) => {
                let last_use = self.next_tick();
                self.blocks[i] = BlockState::Inactive { hash, last_use };
            }
            None => {
                self.blocks[i] = BlockState::Free;
                self.free.push(id);
            }
        }
    }
}

/// Locks the state, recovering from a poisoned lock (a panic while another holder had it)
/// rather than propagating it, so `Drop` never panics on the lock itself.
fn lock(inner: &Mutex<PoolState>) -> MutexGuard<'_, PoolState> {
    inner.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Fixed-capacity block pool. Cheap to clone; clones share the same blocks.
#[derive(Clone)]
pub struct BlockPool {
    inner: Arc<Mutex<PoolState>>,
}

impl BlockPool {
    /// Creates a pool of `capacity` from blocks. Panics if `capacity` is zero.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must not be zero");
        let blocks = (0..capacity).map(|_| BlockState::Free).collect();
        let free = (0..capacity).map(|i| BlockId(i as u32)).collect();
        let state = PoolState {
            blocks,
            free,
            by_hash: HashMap::new(),
            tick: 0,
        };
        Self {
            inner: Arc::new(Mutex::new(state)),
        }
    }

    /// Takes a block from the free list or evicts the least recently used inactive block
    /// when the free list is empty. Returns an uncommitted handle.
    pub fn allocate(&self) -> Result<BlockHandle, PoolError> {
        let mut st = lock(&self.inner);
        let id = match st.free.pop() {
            Some(id) => id,
            None => st.evict_lru().ok_or(PoolError::Exhausted)?,
        };
        st.blocks[PoolState::idx(id)] = BlockState::Active {
            refs: 1,
            hash: None,
        };
        Ok(BlockHandle {
            id,
            pool: Arc::clone(&self.inner),
        })
    }

    /// Cache hit. If a block committed under `hash` exists (active or inactive), returns a
    /// handle sharing it and marks it as used now. An inactive block becomes active.
    pub fn lookup(&self, hash: u64) -> Option<BlockHandle> {
        let mut st = lock(&self.inner);
        let id = *st.by_hash.get(&hash)?;
        let i = PoolState::idx(id);
        match &mut st.blocks[i] {
            BlockState::Active { refs, .. } => *refs += 1,
            BlockState::Inactive { .. } => {
                st.blocks[i] = BlockState::Active {
                    refs: 1,
                    hash: Some(hash),
                };
            }
            BlockState::Free => unreachable!("by_hash holds only committed blocks"),
        }
        Some(BlockHandle {
            id,
            pool: Arc::clone(&self.inner),
        })
    }

    pub fn stats(&self) -> PoolStats {
        let st = lock(&self.inner);
        let mut stats = PoolStats {
            capacity: st.blocks.len(),
            active: 0,
            inactive: 0,
            free: 0,
        };
        for b in &st.blocks {
            match b {
                BlockState::Free => stats.free += 1,
                BlockState::Active { .. } => stats.active += 1,
                BlockState::Inactive { .. } => stats.inactive += 1,
            }
        }
        stats
    }

    /// Every block id by state. For tests.
    pub fn audit(&self) -> Audit {
        let st = lock(&self.inner);
        let mut audit = Audit::default();
        for (i, b) in st.blocks.iter().enumerate() {
            let id = BlockId(i as u32);
            match b {
                BlockState::Free => audit.free.push(id),
                BlockState::Active { .. } => audit.active.push(id),
                BlockState::Inactive { .. } => audit.inactive.push(id),
            }
        }
        audit
    }
}

/// RAII handle on active block. `Clone` shares the block, reference count plus one;
/// `Drop` releases it. The last release moves the block into Inactive if committed or
/// back to Free if not.
pub struct BlockHandle {
    id: BlockId,
    pool: Arc<Mutex<PoolState>>,
}

impl BlockHandle {
    pub fn id(&self) -> BlockId {
        self.id
    }

    /// The hash used to commit this block, `None` if uncommitted.
    pub fn hash(&self) -> Option<u64> {
        let st = lock(&self.pool);
        match st.blocks[PoolState::idx(self.id)] {
            BlockState::Active { hash, .. } => hash,
            _ => unreachable!("a live handle implies an active block"),
        }
    }

    /// Seals the block under `hash`, making it reachable through `BlockPool::lookup`.
    /// Fails if the block is already committed. If another block is already committed
    /// under the same hash, this one replaces it in the lookup index; the older block
    /// stays valid for its handle and is evicted in due course.
    pub fn commit(&self, hash: u64) -> Result<(), PoolError> {
        let mut st = lock(&self.pool);
        let i = PoolState::idx(self.id);
        match &mut st.blocks[i] {
            BlockState::Active { hash: Some(_), .. } => Err(PoolError::AlreadyCommitted),
            BlockState::Active { hash: slot, .. } => {
                *slot = Some(hash);
                st.by_hash.insert(hash, self.id);
                Ok(())
            }
            _ => unreachable!("a live handle implies an active block"),
        }
    }
}

impl Clone for BlockHandle {
    fn clone(&self) -> Self {
        let mut st = lock(&self.pool);
        match &mut st.blocks[PoolState::idx(self.id)] {
            BlockState::Active { refs, .. } => *refs += 1,
            _ => unreachable!("a live handle implies an active block"),
        }
        Self {
            id: self.id,
            pool: Arc::clone(&self.pool),
        }
    }
}

impl Drop for BlockHandle {
    fn drop(&mut self) {
        lock(&self.pool).release(self.id);
    }
}
