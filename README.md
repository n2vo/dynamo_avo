### dynamo_avo

A clean-room Rust reimplementation of the CPU-side key-value (KV) cache management and scheduling core from NVIDIA Dynamo, built one component at a time with Dynamo’s own crates as the reference oracle. The goal is depth, not replacement: Rebuilding each component from its observable behavior and measuring where the design choices are load-bearing. Scope is the control plane: Block pool allocation, content-hashed prefix indexing, eviction, host and disk tiering, and KV-aware routing. Tensor residency and attention kernels remain in the inference engine. Each milestone builds a component, pins its behavior with property tests, compares it bit for bit against the Dynamo reference crates where a reference exists, benchmarks it against the Dynamo baseline, and records the result in `docs/FINDINGS.md`.

#### Workspace

| Crate | Milestone | Dynamo Counterpart | Status |
|---|---|---|---|
| `avo-kv-hash` | 1: Chained block hashing and prefix matching | `lib/tokens`, `lib/kv-hashing` | Complete |
| `avo-block-pool` | 2: Reference-counted block pool with LRU reuse and RAII handles | `lib/kvbm-logical` | Complete |
| `avo-radix-index` | 3: Radix index over block-hash sequences | `lib/kv-router/src/indexer` | Stub |
| `avo-obs` | Observability track: Tracing setup and per-request TTFT, ITL, and TPOT | `lib/llm/src/request_trace` | In progress |

TTFT is time to first token, ITL is inter-token latency, and TPOT is time per output token.

#### Verification

- Differential tests compare `avo-kv-hash` against `dynamo-kv-hashing` and `dynamo-tokens` 1.5.1, pinned in `Cargo.toml` and source-identical to Dynamo `main` at `6e4221b` for the two libraries in question.
- Property tests (`proptest`) pin block counts, prefix stability, salt behavior, and the equivalence of one-pass and incremental hashing.
- Criterion benchmarks run each implementation alongside the Dynamo baseline. Results and the bottleneck analysis for each milestone are in `docs/FINDINGS.md`.
- Workspace lints deny `unsafe_code` and `unused_must_use`; Clippy runs with `unwrap_used`, `dbg_macro`, and `todo` as warnings and `-D warnings` in CI.

#### Build

Toolchain is pinned in `rust-toolchain.toml` (Rust 1.96.1 with `rustfmt` and `clippy`).

```
cargo build
cargo test --workspace
cargo bench --workspace
```

CI (`.github/workflows/ci.yml`) runs format check, Clippy, the test suite, and a compile-only pass of the benchmarks on every push to `main` and every pull request.

#### Layout

```
crates/avo-kv-hash/      Milestone 1 implementation, differential and property tests, benchmarks
crates/avo-block-pool/   Milestone 2 (stub)
crates/avo-radix-index/  Milestone 3 (stub)
crates/avo-obs/          Observability shared by every milestone
docs/FINDINGS.md         Per-milestone benchmark, bottleneck, evidence, and Dynamo comparison
docs/dynamo_request_path.svg
```

#### License

Apache-2.0.
