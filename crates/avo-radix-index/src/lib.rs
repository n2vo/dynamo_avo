//! Milestone 3: Radix index.
//!
//! Compressed radix tree over chained block-hash sequences. Each edge holds a run of
//! blocks, so a long shared prefix (a system prompt) is one hop. Supports insert with edge
//! splitting, longest-prefix match, per-block pinning, and leaf-safe LRU eviction that
//! compacts the tree as it goes. Single-threaded; wrap it in a lock to share it.
//!
//! Dynamo counterpart: `lib/kv-router/src/indexer` (`concurrent_radix_tree_compressed`) for
//! the tree, and `lib/kvbm-logical/src/pools/inactive/backends/lineage` for leaf-only
//! eviction.
//!
//! Proving test: random operation sequences give the same longest-prefix matches from the
//! tree as from a brute-force map of every stored prefix.

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap};

use avo_block_pool::BlockId;
use avo_kv_hash::SeqHash;

/// Key of one block: its chained sequence hash, which identifies the full prefix through
/// that block.
pub type BlockKey = SeqHash;

/// Worker that holds a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkerId(pub u32);

/// Where a block's KV data lives, for the Milestone 4 spill policies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    Tier1Device,
    Tier2PinnedHost,
    Tier3DiskOrRemote,
}

/// Location of one cached block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockMeta {
    pub worker: WorkerId,
    pub block: BlockId,
    pub tier: Tier,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RadixError {
    #[error("sequence is empty")]
    EmptySequence,

    #[error("{hashes} hashes but {metas} block metas")]
    LengthMismatch { hashes: usize, metas: usize },

    #[error("sequence is not fully cached")]
    NotFound,

    #[error("sequence is not pinned")]
    NotPinned,

    #[error("tree is empty")]
    Empty,

    #[error("every leaf is pinned")]
    AllPinned,
}

/// Result of a longest-prefix match.
#[derive(Debug, PartialEq, Eq)]
pub struct PrefixMatch<'q> {
    /// Number of leading query blocks found in the tree.
    pub matched: usize,
    /// Metadata of the matched blocks, in query order.
    pub blocks: Vec<BlockMeta>,
    /// Query blocks after the matched prefix.
    pub remainder: &'q [BlockKey],
}

/// What `insert` changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InsertOutcome {
    /// Blocks added to the tree; blocks already present only have their metadata updated.
    pub new_blocks: usize,
    /// Whether an existing edge was split.
    pub split: bool,
}

/// The block removed by `evict`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvictedBlock {
    pub key: BlockKey,
    pub meta: BlockMeta,
    /// Position of the block in its sequence, counted from 1.
    pub depth: usize,
}

/// Tree size, for invariant checks and benchmarks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeStats {
    /// Live nodes, including the root.
    pub nodes: usize,
    pub blocks: usize,
    pub leaves: usize,
    /// Leaves whose last block is not pinned.
    pub evictable: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct NodeId(u32);

const ROOT: NodeId = NodeId(0);

#[derive(Clone, Copy)]
struct Entry {
    key: BlockKey,
    meta: BlockMeta,
    /// Pins ending at this block. A pinned block and its ancestors are never evicted.
    pins: u32,
}

#[derive(Default)]
struct Node {
    /// Blocks on the edge into this node. Empty only for the root and for freed slots.
    edge: Vec<Entry>,
    /// Children keyed by the first block of their edge.
    children: HashMap<BlockKey, NodeId>,
    parent: NodeId,
    /// Clock value of the last insert or match that touched this node.
    last_access: u64,
}

/// Compressed radix tree from chained block-hash sequences to block locations.
pub struct RadixTree {
    nodes: Vec<Node>,
    free: Vec<NodeId>,
    /// Evictable leaves ordered by last access, oldest first.
    lru: BTreeSet<(u64, NodeId)>,
    clock: u64,
    blocks: usize,
}

impl Default for RadixTree {
    fn default() -> Self {
        Self::new()
    }
}

impl RadixTree {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node::default()],
            free: Vec::new(),
            lru: BTreeSet::new(),
            clock: 0,
            blocks: 0,
        }
    }

    fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.0 as usize]
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn alloc(&mut self, node: Node) -> NodeId {
        match self.free.pop() {
            Some(id) => {
                *self.node_mut(id) = node;
                id
            }
            None => {
                self.nodes.push(node);
                NodeId((self.nodes.len() - 1) as u32)
            }
        }
    }

    fn release(&mut self, id: NodeId) {
        *self.node_mut(id) = Node::default();
        self.free.push(id);
    }

    /// A leaf whose last block is unpinned.
    fn is_evictable(&self, id: NodeId) -> bool {
        let n = self.node(id);
        id != ROOT && n.children.is_empty() && n.edge.last().is_some_and(|e| e.pins == 0)
    }

    /// Re-files `id` in the LRU set after its access time or shape changed.
    fn refresh(&mut self, id: NodeId, old_access: u64) {
        self.lru.remove(&(old_access, id));
        if self.is_evictable(id) {
            let at = self.node(id).last_access;
            self.lru.insert((at, id));
        }
    }

    fn touch(&mut self, id: NodeId, now: u64) {
        let old = self.node(id).last_access;
        self.node_mut(id).last_access = now;
        self.refresh(id, old);
    }

    /// Inserts a sequence with one metadata entry per block. Blocks already present keep
    /// their place and take the new metadata.
    pub fn insert(
        &mut self,
        seq: &[BlockKey],
        metas: &[BlockMeta],
    ) -> Result<InsertOutcome, RadixError> {
        if seq.is_empty() {
            return Err(RadixError::EmptySequence);
        }
        if seq.len() != metas.len() {
            return Err(RadixError::LengthMismatch {
                hashes: seq.len(),
                metas: metas.len(),
            });
        }
        let _span = tracing::trace_span!("radix_insert", prompt_blocks = seq.len()).entered();
        let now = self.tick();
        let mut outcome = InsertOutcome {
            new_blocks: 0,
            split: false,
        };
        let mut cur = ROOT;
        let mut i = 0;
        let mut path = Vec::new();
        while i < seq.len() {
            let Some(&child) = self.node(cur).children.get(&seq[i]) else {
                outcome.new_blocks = seq.len() - i;
                if cur != ROOT && self.node(cur).children.is_empty() {
                    // Extending a leaf: grow its edge rather than hang a single child.
                    let tail = seq[i..].iter().zip(&metas[i..]).map(|(&key, &meta)| Entry {
                        key,
                        meta,
                        pins: 0,
                    });
                    self.node_mut(cur).edge.extend(tail);
                } else {
                    let leaf = self.new_leaf(cur, &seq[i..], &metas[i..], now);
                    path.push(leaf);
                }
                break;
            };
            let k = {
                let edge = &mut self.node_mut(child).edge;
                let k = common_len(edge, &seq[i..]);
                for (e, m) in edge[..k].iter_mut().zip(&metas[i..i + k]) {
                    e.meta = *m;
                }
                k
            };
            path.push(child);
            if k == self.node(child).edge.len() {
                cur = child;
                i += k;
                continue;
            }
            if i + k == seq.len() {
                break;
            }
            let mid = self.split(child, k);
            tracing::trace!(at = k, "radix_edge_split");
            outcome.split = true;
            path.pop();
            path.push(mid);
            let leaf = self.new_leaf(mid, &seq[i + k..], &metas[i + k..], now);
            outcome.new_blocks = seq.len() - i - k;
            path.push(leaf);
            break;
        }
        self.blocks += outcome.new_blocks;
        for id in path {
            self.touch(id, now);
        }
        Ok(outcome)
    }

    fn new_leaf(
        &mut self,
        parent: NodeId,
        seq: &[BlockKey],
        metas: &[BlockMeta],
        now: u64,
    ) -> NodeId {
        let edge = seq
            .iter()
            .zip(metas)
            .map(|(&key, &meta)| Entry { key, meta, pins: 0 })
            .collect();
        let id = self.alloc(Node {
            edge,
            children: HashMap::new(),
            parent,
            last_access: now,
        });
        self.node_mut(parent).children.insert(seq[0], id);
        // The parent may have been an evictable leaf until now.
        let old = self.node(parent).last_access;
        self.refresh(parent, old);
        id
    }

    /// Splits `child`'s edge after `k` blocks. A new node takes the first `k`; `child`
    /// keeps the rest, its children, and its place in the LRU set.
    fn split(&mut self, child: NodeId, k: usize) -> NodeId {
        let parent = self.node(child).parent;
        let head: Vec<Entry> = self.node_mut(child).edge.drain(..k).collect();
        let first = head[0].key;
        let tail_first = self.node(child).edge[0].key;
        let access = self.node(child).last_access;
        let mid = self.alloc(Node {
            edge: head,
            children: HashMap::from([(tail_first, child)]),
            parent,
            last_access: access,
        });
        self.node_mut(child).parent = mid;
        self.node_mut(parent).children.insert(first, mid);
        mid
    }

    /// Longest prefix of `query` stored in the tree. Marks every node on the matched path
    /// as accessed.
    pub fn match_prefix<'q>(&mut self, query: &'q [BlockKey]) -> PrefixMatch<'q> {
        let _span = tracing::trace_span!("radix_match", prompt_blocks = query.len()).entered();
        let now = self.tick();
        let mut blocks = Vec::new();
        let mut cur = ROOT;
        let mut i = 0;
        while i < query.len() {
            let Some(&child) = self.node(cur).children.get(&query[i]) else {
                break;
            };
            let edge = &self.node(child).edge;
            let k = common_len(edge, &query[i..]);
            blocks.extend(edge[..k].iter().map(|e| e.meta));
            let full = k == edge.len();
            i += k;
            self.touch(child, now);
            if !full {
                break;
            }
            cur = child;
        }
        tracing::trace!(cache_hit_blocks = i, "radix_match_done");
        PrefixMatch {
            matched: i,
            blocks,
            remainder: &query[i..],
        }
    }

    /// Finds the node and edge offset holding the last block of `seq`.
    fn locate(&self, seq: &[BlockKey]) -> Result<(NodeId, usize), RadixError> {
        if seq.is_empty() {
            return Err(RadixError::EmptySequence);
        }
        let mut cur = ROOT;
        let mut i = 0;
        loop {
            let child = *self
                .node(cur)
                .children
                .get(&seq[i])
                .ok_or(RadixError::NotFound)?;
            let edge = &self.node(child).edge;
            let k = common_len(edge, &seq[i..]);
            if i + k == seq.len() {
                return Ok((child, k - 1));
            }
            if k < edge.len() {
                return Err(RadixError::NotFound);
            }
            i += k;
            cur = child;
        }
    }

    /// Protects `seq` from eviction until a matching `unpin`. Pins nest.
    pub fn pin(&mut self, seq: &[BlockKey]) -> Result<(), RadixError> {
        let (id, at) = self.locate(seq)?;
        self.node_mut(id).edge[at].pins += 1;
        let old = self.node(id).last_access;
        self.refresh(id, old);
        Ok(())
    }

    /// Releases one pin taken by `pin` on the same sequence.
    pub fn unpin(&mut self, seq: &[BlockKey]) -> Result<(), RadixError> {
        let (id, at) = self.locate(seq)?;
        let pins = &mut self.node_mut(id).edge[at].pins;
        if *pins == 0 {
            return Err(RadixError::NotPinned);
        }
        *pins -= 1;
        let old = self.node(id).last_access;
        self.refresh(id, old);
        Ok(())
    }

    /// Removes the last block of the least recently used unpinned leaf. Never removes a
    /// block that another stored sequence passes through. Frees an emptied leaf and merges
    /// a parent left with one child, so no dead or single-child internal node remains.
    pub fn evict(&mut self) -> Result<EvictedBlock, RadixError> {
        if self.blocks == 0 {
            return Err(RadixError::Empty);
        }
        let &(access, id) = self.lru.first().ok_or(RadixError::AllPinned)?;
        let first = self.node(id).edge[0].key;
        let depth = self.depth(id);
        let Some(entry) = self.node_mut(id).edge.pop() else {
            return Err(RadixError::Empty);
        };
        self.blocks -= 1;
        tracing::trace!(depth, "radix_evict");
        if self.node(id).edge.is_empty() {
            self.lru.remove(&(access, id));
            let parent = self.node(id).parent;
            self.node_mut(parent).children.remove(&first);
            self.release(id);
            self.compact(parent);
        } else {
            self.refresh(id, access);
        }
        Ok(EvictedBlock {
            key: entry.key,
            meta: entry.meta,
            depth,
        })
    }

    /// Number of blocks from the root through the last block of `id`'s edge.
    fn depth(&self, id: NodeId) -> usize {
        let mut d = 0;
        let mut cur = id;
        while cur != ROOT {
            d += self.node(cur).edge.len();
            cur = self.node(cur).parent;
        }
        d
    }

    /// After a child of `id` was removed: a childless `id` becomes a leaf, and an `id`
    /// with one child merges into that child.
    fn compact(&mut self, id: NodeId) {
        if id == ROOT {
            return;
        }
        match self.node(id).children.len() {
            0 => {
                let old = self.node(id).last_access;
                self.refresh(id, old);
            }
            1 => {
                let Some(&only) = self.node(id).children.values().next() else {
                    return;
                };
                let parent = self.node(id).parent;
                let first = self.node(id).edge[0].key;
                let mut edge = std::mem::take(&mut self.node_mut(id).edge);
                edge.append(&mut self.node_mut(only).edge);
                let old = self.node(only).last_access;
                let access = old.max(self.node(id).last_access);
                let n = self.node_mut(only);
                n.edge = edge;
                n.parent = parent;
                n.last_access = access;
                self.node_mut(parent).children.insert(first, only);
                self.release(id);
                self.refresh(only, old);
                tracing::trace!("radix_compact");
            }
            _ => {}
        }
    }

    pub fn stats(&self) -> TreeStats {
        let mut s = TreeStats {
            nodes: 1,
            blocks: self.blocks,
            leaves: 0,
            evictable: self.lru.len(),
        };
        for n in self.nodes.iter().skip(1) {
            if n.edge.is_empty() {
                continue;
            }
            s.nodes += 1;
            if n.children.is_empty() {
                s.leaves += 1;
            }
        }
        s
    }
}

/// Length of the common prefix of an edge and a query.
fn common_len(edge: &[Entry], query: &[BlockKey]) -> usize {
    edge.iter()
        .zip(query)
        .take_while(|(e, q)| e.key == **q)
        .count()
}
