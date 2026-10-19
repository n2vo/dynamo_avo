### Findings Log

Each milestone closes with one entry: The benchmark, the bottleneck that limits throughput or latency, the evidence, and the fix or the reason it was deferred, followed by the comparison against the Dynamo counterpart.

#### Milestone 1: Block Hashing

- **Benchmark:** `benches/hashing.rs`, Criterion, group `plh`, block size 16, unsalted, 1,024 / 8,192 / 32,768 tokens, on jeeves7 (M1 Max). Dynamo baseline times only `Request::positional_lineage_hashes(16)` on a prebuilt `Request`. Results in millions of tokens per second (Melem/s): Dynamo 260 to 275; `hash_sequence` 459 to 464; `IncrementalHasher` 252 to 268. Flat across lengths.
- **Bottleneck:** `IncrementalHasher::push` runs at about 0.57x of `hash_sequence` on the same tokens. Both share `chain_step` and `token_bytes`, so the difference is per-token entry cost: One bounds-checked `Vec::push`, one length compare, one branch, per token, versus one `chunks_exact` step per block. Hypothesis from code reading; not isolated by measurement.
- **Evidence:** Same outputs proven by the differential tests against `dynamo-kv-hashing` 1.5.1 and by the `incremental_matches_one_pass` proptest. Moving the token clone and `Request` build out of the Dynamo timed region changed its throughput by 2 to 4 percent, so the remaining gap is inside `into_blocks`.
- **Fix or Deferral:** Deferred. Decode produces one token per step, so the per-token path is the real one; batching through an `extend` would only help prefill. Candidate fixes: Replace the `Vec<u8>` allocation in `token_bytes` with a stack buffer sized by block, and let `push` write the token's bytes directly into that buffer instead of staging `u32` values. Revisit when a milestone exercises the decode path end to end.
- **Dynamo Comparison:** The xxh3 work is identical: Two hashes per block, same seeds, same little-endian layouts. The 1.7x throughput gap is structural, not algorithmic. `into_blocks` clones the full token vector, builds a `TokenBlockSequence` with one `TokenBlock` per block, each owning a copy of its tokens and both hashes, collects a `Vec<UniversalBlock>`, and then maps that into the returned hashes. Dynamo materializes block objects because its router, pool, and migration paths consume them; the hash is a by-product. `avo-kv-hash` computes only the hash. When later milestones require block objects, that cost returns.

#### Milestone 2: Block Pool

- **Benchmark:**
- **Bottleneck:**
- **Evidence:**
- **Fix or Deferral:**
- **Dynamo Comparison:**

#### Milestone 3: Radix Index

- **Benchmark:**
- **Bottleneck:**
- **Evidence:**
- **Fix or Deferral:**
- **Dynamo Comparison:**
