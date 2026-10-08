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

/// Hash of one block's tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockHash(u64);

/// Identity of the full prefix through this block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SeqHash(u64);

/// Hashes every complete block of `tokens`, chaining each block to its parent.
/// Returns one `SeqHash` per complete block; a trailing partial block is ignored.
pub fn hash_sequence(_tokens: &[u32], _block_size: usize, _salt: Option<&str>) -> Vec<SeqHash> {
    Vec::new()
}
