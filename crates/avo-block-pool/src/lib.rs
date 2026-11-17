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

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};

/// Index of a block in the pool, `0..capacity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PoolError {
    /// No free block and no inactive block to evict: Every block is held by a live handle.
    #[error("pool exhausted: no free or evictable block")]
    Exhausted,

    #[error("block already committed")]
    AlreadyCommitted,

    /// A batch named a handle from a different pool.
    #[error("handle belongs to a different pool")]
    ForeignHandle,
}

/// Block counts. `active + inactive + free == capacity` always.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStats {
    pub capacity: usize,

    /// Held by at least one live handle.
    pub active: usize,

    /// Committed, no live handle, cached for `lookup`, evictable.
    pub inactive: usize,

    /// Never committed, or evicted; available to `allocate`.
    pub free: usize,
}

/// Every block id by state, for invariant checks in tests. Order within a vector is not
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
    /// On the free list. Never committed, or evicted.
    Free,

    /// Held by `refs` live handles. `hash` is `None` until `commit`.
    Active { refs: usize, hash: Option<u64> },

    /// Committed, no live handle, cached for `lookup`, and linked into the LRU list.
    Inactive { hash: u64 },
}

/// Hasher for the hash index. Keys are block hashes, hashed again with one multiply by
/// 2^64 / golden ratio (Fibonacci hashing). The multiply spreads sequential keys into the
/// top bits, which `hashbrown` uses as each slot's tag. SipHash, the default, runs under the
/// pool lock on every lookup, eviction, and commit.
#[derive(Default)]
struct BlockHashHasher(u64);

impl Hasher for BlockHashHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    /// `u64` keys hash through `write_u64`, so this byte path is never reached.
    fn write(&mut self, _: &[u8]) {
        unreachable!("the hash index is keyed by u64, which hashes through write_u64")
    }
}

/// Block hash to block id, hashed with `BlockHashHasher`.
type HashIndex = HashMap<u64, BlockId, BuildHasherDefault<BlockHashHasher>>;

/// Internal pool state behind the lock
struct PoolState {
    // Per-id state: Free | Active { refs, hash } | Inactive { hash }
    blocks: Vec<BlockState>,

    // Free list
    free: Vec<BlockId>,

    // Committed blocks, active or inactive, for lookup
    by_hash: HashIndex,

    // LRU list: Previous inactive block, NIL at the head
    prev: Vec<u32>,

    // LRU list: Next inactive block, NIL at the tail
    next: Vec<u32>,

    // Least recently released inactive block, or NIL
    head: u32,

    // Most recently released inactive block, or NIL
    tail: u32,

    // Number of blocks linked into the LRU list, so a batch can check capacity up front
    inactive_len: usize,
}

// End-of-list marker for the LRU links
const NIL: u32 = u32::MAX;

impl PoolState {
    fn idx(id: BlockId) -> usize {
        id.0 as usize
    }

    fn lru_push_back(&mut self, id: BlockId) {
        let id = id.0;
        self.prev[id as usize] = self.tail;
        self.next[id as usize] = NIL;
        if self.tail == NIL {
            self.head = id;
        } else {
            self.next[self.tail as usize] = id;
        }
        self.tail = id;
        self.inactive_len += 1;
    }

    // Removes a block from anywhere in the LRU list.
    fn lru_unlink(&mut self, id: BlockId) {
        let i = Self::idx(id);
        let (p, n) = (self.prev[i], self.next[i]);
        if p == NIL {
            self.head = n;
        } else {
            self.next[p as usize] = n;
        }
        if n == NIL {
            self.tail = p;
        } else {
            self.prev[n as usize] = p;
        }
        self.prev[i] = NIL;
        self.next[i] = NIL;
        self.inactive_len -= 1;
    }

    /// Evicts the least recently used inactive block and returns its id, now Free but not
    /// on the free list (the caller takes it directly). `None` if nothing is inactive.
    fn evict_lru(&mut self) -> Option<BlockId> {
        if self.head == NIL {
            return None;
        }
        let id = BlockId(self.head);
        let i = Self::idx(id);
        let BlockState::Inactive { hash } = self.blocks[i] else {
            unreachable!("only inactive blocks are linked into the LRU list");
        };
        self.lru_unlink(id);
        self.blocks[i] = BlockState::Free;

        // Remove the index entry only if it still points at this block. A later commit
        // under the same hash may have re-pointed it at a newer block.
        if self.by_hash.get(&hash) == Some(&id) {
            self.by_hash.remove(&hash);
        }
        Some(id)
    }

    /// Takes a block from the free list, or evicts the LRU inactive block when the free
    /// list is empty, and makes it Active and uncommitted. `None` if neither exists.
    fn take(&mut self) -> Option<BlockId> {
        let id = match self.free.pop() {
            Some(id) => id,
            None => self.evict_lru()?,
        };
        self.blocks[Self::idx(id)] = BlockState::Active {
            refs: 1,
            hash: None,
        };
        Some(id)
    }

    /// Cache hit on `hash`: Shares an active block or revives an inactive one out of
    /// the LRU list. `None` if no block is committed under `hash`.
    fn acquire(&mut self, hash: u64) -> Option<BlockId> {
        let id = *self.by_hash.get(&hash)?;
        let i = Self::idx(id);
        match &mut self.blocks[i] {
            BlockState::Active { refs, .. } => *refs += 1,
            BlockState::Inactive { .. } => {
                self.lru_unlink(id);
                self.blocks[i] = BlockState::Active {
                    refs: 1,
                    hash: Some(hash),
                };
            }
            BlockState::Free => unreachable!("by_hash holds only committed blocks"),
        }
        Some(id)
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
                self.blocks[i] = BlockState::Inactive { hash };
                self.lru_push_back(id);
            }
            None => {
                self.blocks[i] = BlockState::Free;
                self.free.push(id);
            }
        }
    }
}

/// Locks the state. `parking_lot::Mutex` has no lock poisoning so a panic while another
/// holder had the lock does not make `Drop` panic on the lock itself.
fn lock(inner: &Mutex<PoolState>) -> MutexGuard<'_, PoolState> {
    inner.lock()
}

/// Fixed-capacity block pool. Cheap to clone; clones share the same blocks.
#[derive(Clone)]
pub struct BlockPool {
    inner: Arc<Mutex<PoolState>>,
}

impl BlockPool {
    /// Creates a pool of `capacity` free blocks. Panics if `capacity` is zero.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must not be zero");
        assert!(
            capacity < NIL as usize,
            "capacity must fit block ids below NIL"
        );

        let blocks = (0..capacity).map(|_| BlockState::Free).collect();
        let free = (0..capacity).map(|i| BlockId(i as u32)).collect();
        let state = PoolState {
            blocks,
            free,
            by_hash: HashIndex::default(),
            prev: vec![NIL; capacity],
            next: vec![NIL; capacity],
            head: NIL,
            tail: NIL,
            inactive_len: 0,
        };
        Self {
            inner: Arc::new(Mutex::new(state)),
        }
    }

    /// Takes a block from the free list or evicts the least recently used inactive block
    /// when the free list is empty. Returns an uncommitted handle.
    pub fn allocate(&self) -> Result<BlockHandle, PoolError> {
        let id = lock(&self.inner).take().ok_or(PoolError::Exhausted)?;
        Ok(self.handle(id))
    }

    /// Allocates `n` uncommitted blocks under one lock acquisition, free list first, then
    /// LRU eviction. All or nothing: If fewer than `n` blocks are free or inactive, returns
    /// `Exhausted` and changes nothing.
    pub fn allocate_many(&self, n: usize) -> Result<Vec<BlockHandle>, PoolError> {
        let ids: Vec<BlockId> = {
            let mut st = lock(&self.inner);
            if st.free.len() + st.inactive_len < n {
                return Err(PoolError::Exhausted);
            }
            (0..n)
                .map(|_| st.take().expect("free plus inactive covers n"))
                .collect()
        };
        // Handles are built after the lock is released: A handle dropped while the lock
        // is held would deadlock in `Drop`.
        Ok(ids.into_iter().map(|id| self.handle(id)).collect())
    }

    /// Cache hit. If a block committed under `hash` exists (active or inactive), returns a
    /// handle sharing it. An inactive block becomes active and leaves the LRU list.
    pub fn lookup(&self, hash: u64) -> Option<BlockHandle> {
        let id = lock(&self.inner).acquire(hash)?;
        Some(self.handle(id))
    }

    /// Prefix match under one lock acquisition: Looks up `hashes` in order and stops at the
    /// first miss. Returns one handle per matched hash, in order.
    pub fn lookup_prefix(&self, hashes: &[u64]) -> Vec<BlockHandle> {
        let ids: Vec<BlockId> = {
            let mut st = lock(&self.inner);
            hashes.iter().map_while(|&hash| st.acquire(hash)).collect()
        };
        ids.into_iter().map(|id| self.handle(id)).collect()
    }

    /// Commits each handle under its hash, under one lock acquisition. Validates the batch
    /// before changing anything, so it applies entirely or not at all. Fails with
    /// `ForeignHandle` if a handle belongs to another pool, and with `AlreadyCommitted` if
    /// a block is already committed or appears twice in the batch (clones share a block).
    /// Commits apply in order, so a repeated hash leaves the last block in the index, as
    /// sequential `commit` calls would.
    pub fn commit_many(&self, items: &[(&BlockHandle, u64)]) -> Result<(), PoolError> {
        if items
            .iter()
            .any(|(h, _)| !Arc::ptr_eq(&h.pool, &self.inner))
        {
            return Err(PoolError::ForeignHandle);
        }
        let mut st = lock(&self.inner);
        let mut seen = HashSet::with_capacity(items.len());
        for (h, _) in items {
            if !seen.insert(h.id) {
                return Err(PoolError::AlreadyCommitted);
            }
            match st.blocks[PoolState::idx(h.id)] {
                BlockState::Active { hash: Some(_), .. } => {
                    return Err(PoolError::AlreadyCommitted);
                }
                BlockState::Active { hash: None, .. } => {}
                _ => unreachable!("a live handle implies an active block"),
            }
        }
        for (h, hash) in items {
            if let BlockState::Active { hash: slot, .. } = &mut st.blocks[PoolState::idx(h.id)] {
                *slot = Some(*hash);
            }
            st.by_hash.insert(*hash, h.id);
        }
        Ok(())
    }

    fn handle(&self, id: BlockId) -> BlockHandle {
        BlockHandle {
            id,
            pool: Arc::clone(&self.inner),
        }
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

/// RAII handle on an active block. `Clone` shares the block, reference count plus one;
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
