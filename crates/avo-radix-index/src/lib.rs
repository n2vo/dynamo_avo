//! Milestone 3: Radix index.
//!
//! Tree over block-hash sequences supporting insert, longest-prefix match, and LRU leaf
//! eviction. Built single-threaded first, then made concurrent.
//!
//! Dynamo counterpart: `lib/kv-router/src/indexer` (`concurrent_radix_tree_compressed`).
//!
//! Proving test: random operation sequences return identical results from the tree and a
//! brute-force list.
