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
