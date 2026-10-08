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

/// Hashes every complete block of `tokens`, chaining each block to its parent.
/// Returns one `SeqHash` per complete block; a trailing partial block is ignored.
pub fn hash_sequence(tokens: &[u32], block_size: usize, salt: Option<&str>) -> Vec<SeqHash> {
    let mut out = Vec::with_capacity(tokens.len() / block_size);
    let mut parent: Option<u64> = None;

    let seed = salt_seed(salt);

    for block in tokens.chunks_exact(block_size) {
        let block_hash = xxh3_64_with_seed(&token_bytes(block), seed);
        let seq = match parent {
            None => block_hash,
            Some(p) => {
                let mut buf = [0u8; 16];
                buf[..8].copy_from_slice(&p.to_le_bytes());
                buf[8..].copy_from_slice(&block_hash.to_le_bytes());
                xxh3_64_with_seed(&buf, CHAIN_XXH3_SEED)
            }
        };
        out.push(SeqHash(seq));
        parent = Some(seq);
    }
    out
}
