//! Milestone 1: Chained block hashing.
//!
//! Splits a token sequence into fixed-size blocks and hashes each block together with its
//! parent's hash, so each hash identifies the full prefix through that block.
//!
//! Dynamo counterparts: `lib/tokens` (`blocks.rs`, `radix.rs`) and `lib/kv-hashing`
//! (`salt.rs`, `request.rs`). The reference test in `tests/dynamo_reference.rs` exercises
//! Dynamo's implementation for differential comparison.
//!
//! Proving test: hashing a sequence in one pass equals hashing it incrementally as tokens
//! are appended.

use xxhash_rust::xxh3::xxh3_64_with_seed;

/// Dynamo's `CHAIN_XXH3_SEED`, used for chain steps.
const CHAIN_XXH3_SEED: u64 = 1337;

/// Hash of one block's tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockHash(u64);

/// Identity of the full prefix through this block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SeqHash(u64);

impl SeqHash {
    /// The raw 64-bit value, for comparison with Dynamo's `SequenceHash` (`u64`).
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Encodes tokens as little-endian bytes, four per token, in order.
fn token_bytes(tokens: &[u32]) -> Vec<u8> {
    tokens.iter().flat_map(|t| t.to_le_bytes()).collect()
}

/// Dynamo's block-hash seed: the chain seed, plus an xxh3 mix of the salt when present.
/// An empty salt counts as no salt.
fn salt_seed(salt: Option<&str>) -> u64 {
    match salt.filter(|s| !s.is_empty()) {
        None => CHAIN_XXH3_SEED,
        Some(s) => CHAIN_XXH3_SEED.wrapping_add(xxh3_64_with_seed(s.as_bytes(), 1)),
    }
}

/// Dynamo's chain step: The first block's sequence hash is its block hash; every later
/// block hashes `[parent, block_hash]` as little-endian bytes with the chain seed.
fn chain_step(parent: Option<u64>, block_hash: u64) -> u64 {
    match parent {
        None => block_hash,
        Some(p) => {
            let mut buf = [0u8; 16];
            buf[..8].copy_from_slice(&p.to_le_bytes());
            buf[8..].copy_from_slice(&block_hash.to_le_bytes());
            xxh3_64_with_seed(&buf, CHAIN_XXH3_SEED)
        }
    }
}

/// Hashes every complete block of `tokens`, chaining each block to its parent.
/// Returns one `SeqHash` per complete block; a trailing partial block is ignored.
pub fn hash_sequence(tokens: &[u32], block_size: usize, salt: Option<&str>) -> Vec<SeqHash> {
    let mut out = Vec::with_capacity(tokens.len() / block_size);
    let mut parent: Option<u64> = None;

    let seed = salt_seed(salt);

    for block in tokens.chunks_exact(block_size) {
        let block_hash = xxh3_64_with_seed(&token_bytes(block), seed);
        let seq = chain_step(parent, block_hash);

        out.push(SeqHash(seq));
        parent = Some(seq);
    }
    out
}

/// Hashes a token stream as it arrives, emitting a `SeqHash` each time a block completes.
/// Produces the same hashes as `hash_sequence` over the same tokens, block size, and salt.
pub struct IncrementalHasher {
    block_size: usize,
    seed: u64,
    pending: Vec<u32>,
    parent: Option<u64>,
    hashes: Vec<SeqHash>,
}

impl IncrementalHasher {
    /// Creates a hasher for `block_size` tokens per block. Panics if `block_size` is zero,
    /// matching `hash_sequence`, whose `chunks_exact(0)` also panics.
    pub fn new(block_size: usize, salt: Option<&str>) -> Self {
        assert!(block_size > 0, "block_size must be non-zero");
        Self {
            block_size,
            seed: salt_seed(salt),
            pending: Vec::with_capacity(block_size),
            parent: None,
            hashes: Vec::new(),
        }
    }

    pub fn push(&mut self, token: u32) -> Option<SeqHash> {
        self.pending.push(token);
        if self.pending.len() < self.block_size {
            return None;
        }
        let block_hash = xxh3_64_with_seed(&token_bytes(&self.pending), self.seed);
        let seq = SeqHash(chain_step(self.parent, block_hash));
        self.pending.clear();
        self.parent = Some(seq.0);
        self.hashes.push(seq);
        Some(seq)
    }

    /// All sequence hashes for completed blocks so far, in order.
    pub fn hashes(&self) -> &[SeqHash] {
        &self.hashes
    }
}
