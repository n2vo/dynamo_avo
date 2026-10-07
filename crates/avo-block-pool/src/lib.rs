//! Milestone 2: Block pool.
//!
//! Fixed-capacity allocator with reference-counted shared prefix blocks, LRU reuse of
//! finished blocks, and RAII handles that return blocks through `Drop`.
//!
//! Dynamo counterpart: `lib/kvbm-logical` (`pools`, `blocks`, `manager`, `tinylfu.rs`).
//!
//! Proving test: no block is leaked, and no block is freed twice.
