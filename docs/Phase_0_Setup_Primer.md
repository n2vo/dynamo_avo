### Phase 0 Setup Primer

#### Purpose

Phase 0 prepares the Rust key-value (KV) cache and scheduling core project for build work. It fixes the interview planning date, checks the upstream contribution lead, builds the NVIDIA Dynamo reference crates locally, and creates a signed, continuously tested workspace. Work was performed on October 7, 2026, against Dynamo `main` at commit `6e4221b`.

#### Status Summary

| Step | Outcome | Status |
|---|---|---|
| 1. Interview Date | Planning default set to October 27, 2026 | Complete; date to be confirmed |
| 2. Contribution Lead Triage | Original salt lead largely fixed upstream; new lead identified | Complete |
| 3. Dynamo Local Build | Four reference crates pass on Jeeves7 | Complete |
| 4. Workspace Scaffold | Private repository `n2vo/dynamo_avo` with passing continuous integration (CI) | Complete; Verified badge to be confirmed |

#### Step 1: Interview Date

No date for the Photon Intelligence coding interview was found in prior records. Planning proceeds against October 27, 2026, three weeks from the start of Phase 0. The language rule from the project primer stands: Rust if Milestones 1 through 3 are complete before the interview, otherwise Go.

#### Step 2: Contribution Lead Triage

##### Method

GitHub issue search was unavailable from the working environment, so the Dynamo commit history and source at `6e4221b` were searched for salt, tenant, and keyed hashing changes.

##### Findings

| Original Weakness | Current State | Reference |
|---|---|---|
| Salt dropped when request extensions are disabled | Fixed on August 22, 2026 | Pull request (PR) #13157 |
| Salt unsupported on SGLang | Fixed on September 22, 2026 | PR #14803 |
| Salt taken from the unverified `x-tenant-id` header | Documented as intended; the gateway must authenticate tenant identity | Request extensions documentation |
| Salt derivation uses no secret key | Partially addressed by experimental provider-keyed tracking hashes | PR #11548 |

PR #11548 adds `--router-tracking-hash keyed-xxh3-v1`. It requires a 32-byte provider key and a key identifier (key epoch), derives per-request seeds from a keyed BLAKE3 digest, and refuses to start on invalid key configuration. The keyed hashes apply only to router-internal tracking state; indexer hashes, engine hashes, and KV events remain public by design.

##### Revised Lead

- **Primary:** Mixed key epochs are not detected. No request, selection payload, tracker event, or replica message carries the algorithm or key identifier, so a partial key rotation passes silently. The gap is documented upstream and confined to the router.
- **Secondary:** `TODO(#11971)` in `lib/kv-router/src/tracking_hash.rs`, concerning derivation of the static tracking-domain identity.
- **Tertiary:** Test coverage for key rotation and mixed-epoch behavior, supporting promotion of keyed mode out of experimental status.

##### Contribution Requirements

Dynamo requires a Developer Certificate of Origin (DCO) sign-off on every commit (`git commit -s`). Fork PRs qualify for automatic CI approval only when every commit carries a cryptographic signature that GitHub reports as Verified. A sign-off and a signature are separate mechanisms.

#### Developer Environment

##### Commit Identity and Signing

| Setting | Value |
|---|---|
| `user.name` | Anh Thu Vo |
| `user.email` | avo@alum.mit.edu |
| `gpg.format` | ssh |
| `user.signingkey` | `~/.ssh/id_ed25519.pub` |
| `commit.gpgsign` | true |
| `gpg.ssh.allowedSignersFile` | `~/.ssh/allowed_signers` |

The Ed25519 key was already registered on GitHub as an authentication key by the GitHub command-line interface (CLI). It was registered a second time as a signing key titled "Jeeves7 Signing" through the web interface. A test commit produced a good signature and a `Signed-off-by` trailer.

##### Issues Resolved

- **Stale Token:** A `GITHUB_TOKEN` environment variable with revoked credentials overrides the stored GitHub CLI login and returns Hypertext Transfer Protocol (HTTP) status 401. The variable remains set in the shell profile and should be removed or regenerated.
- **Missing Git LFS:** The global Git configuration referenced Git Large File Storage (LFS), which was not installed, so the Dynamo clone aborted mid-checkout. Installing `git-lfs` and running `git reset --hard HEAD` recovered the working tree without a second download.
- **Finder Metadata:** `.DS_Store` was added to the global ignore file at `~/.config/git/ignore`.

#### Step 3: Dynamo Local Build

The reference checkout is located at `~/dev/avolab-projects/dynamos/dynamo`, pinned to `6e4221b`. A stale June 2025 clone with no local work was deleted.

| Crate | Tests Passed | Ignored |
|---|---|---|
| `dynamo-tokens` | 55 | 2 |
| `dynamo-kv-hashing` | 20 | 0 |
| `kvbm-logical` | 508 | 12 |
| `dynamo-kv-router` | 1,044 | 3 |

Results match a Linux x86 build exactly. No CUDA, NVIDIA Inference Xfer Library (NIXL), or other system library was required. `dynamo-tokens` and `dynamo-kv-hashing` have no internal Dynamo dependencies, which makes them suitable for the Milestone 1 differential test.

#### Dynamo Architecture Notes

##### Repository Layout

| Layer | Language | Location |
|---|---|---|
| Runtime, router, KV Block Manager (KVBM), frontend | Rust | `lib/`, approximately 760,000 lines across 40 workspace members |
| Engine integrations, planner, deployable components | Python | `components/src/dynamo/` |
| Kubernetes operator | Go | `deploy/operator/` |

Python reaches Rust through PyO3 bindings in `lib/bindings/python`.

##### Crate Dependency Order

| Crate | Depends On | Role |
|---|---|---|
| `dynamo-tokens` | None | Block formation, salt-seeded chain, positional lineage hash (PLH) |
| `dynamo-kv-hashing` | `dynamo-tokens` | Request-to-hash contract, including `salt.rs` |
| `kvbm-logical` | `dynamo-tokens` | Block lifecycle, pools, reference counts, eviction |
| `dynamo-kv-router` | `dynamo-kv-hashing`, `dynamo-runtime` | Indexer, scheduling, `tracking_hash.rs`, `conditional_disagg.rs` |
| `dynamo-mocker` | `dynamo-kv-router` | Simulated engine scheduler and KV manager |
| `dynamo-runtime` | None internal | Discovery, transports, pipeline, metrics |
| `dynamo-llm` | Router, runtime, mocker | Frontend, preprocessor, `migration.rs`, `request_trace` |

##### Engines and Connection Modes

The inference engine is vLLM, SGLang, or TensorRT-LLM. The engine owns model weights, kernels, the paged KV cache, the per-step scheduler, and sampling. Dynamo owns the frontend, routing, disaggregation, KVBM offload, the planner, and discovery across many engine instances.

| Aspect | Integrated Backend | Sidecar Backend (Experimental) |
|---|---|---|
| Process Model | One Python worker imports the engine as a library | Stock engine server runs beside a central processing unit (CPU)-only Rust sidecar |
| Request Path | In-process engine call | Request plane calls the engine's native gRPC API |
| Code | `components/src/dynamo/{vllm,sglang,trtllm}` | `lib/sidecar/` |

Under Dynamo, each engine's own HTTP frontend is bypassed. The Dynamo frontend tokenizes once and routes token identifiers, because KV-aware routing requires block hashes before worker selection. The KVBM entry for SGLang is marked work in progress; SGLang uses its native HiCache tiering instead.

##### Request Path

| Order | Stage | Code Location | Milestone |
|---|---|---|---|
| 1 | HTTP frontend | `lib/llm/src/http/service/openai.rs` | None |
| 2 | Preprocessor | `lib/llm/src/preprocessor.rs` | None |
| 3 | Migration | `lib/llm/src/migration.rs` | 9 |
| 4 | Token backend | `Backend::from_tokenizer` | None |
| 5 | Encoder router | `EncoderRouter` | None |
| 6 | Prefill router | `PrefillRouter` | 7 |
| 7 | KV router | `lib/llm/src/kv_router/routing_host.rs` | 1, 3, 5, 6 |
| 8 | Worker | `components/src/dynamo/vllm/handlers.py` | 2, 4 (engine internals) |

Each stage is an operator with a forward edge for the request and a backward edge for the response stream. KV events and discovery run beside the request path. The diagram `docs/dynamo_request_path.svg` records this flow.

##### Latency Metrics

Time to First Token (TTFT) measures arrival to first output token. Inter-Token Latency (ITL) measures each gap between consecutive output tokens. Time Per Output Token (TPOT) averages decode time per request. Mean ITL approximates mean TPOT, but only ITL tail percentiles expose prefill-induced decode stalls.

#### Cache Salt and Tenant Isolation

##### Mechanism

A salt is mixed into the seed of the first block hash. Because hashes chain, every later hash changes, so requests with different salts never share blocks while requests with the same salt still do. Every distributed component that keys on block hash (router indexer, KV events, KVBM tiers, disaggregated transfer, shared pools) inherits the isolation without a separate tenant check.

##### Limits

- **Universal Adoption Required:** One component hashing without the salt breaks isolation, as SGLang did before PR #14803.
- **Reuse Cost:** Tenants sharing an identical public prefix each pay for prefill and memory; salt granularity is a policy choice.
- **Load Skew:** Tenant prefixes attract requests to specific workers, which the router's overlap-versus-load scoring must correct.
- **Residual Channels:** The salt does not prevent capacity contention or eviction-based inference.
- **No Authentication:** The salt is whatever the request declares. Direct request-plane access bypasses the frontend entirely, and request-plane Transport Layer Security (TLS) is opt-in.

##### Identity Design

| Concern | Mechanism | Enforcement Point |
|---|---|---|
| Workload identity | Secure Production Identity Framework for Everyone (SPIFFE) Verifiable Identity Document (SVID) over mutual TLS (mTLS) | Worker, at connection |
| Tenant identity | JSON Web Token (JWT) from the tenant identity provider | Gateway or frontend, per request |
| Salt unguessability | Hash-based Message Authentication Code (HMAC) of the tenant identifier under a provider key | Frontend |

A per-request nonce is unnecessary under this design, since the tenant JWT terminates at the frontend and mTLS prevents request-plane replay. A salt cannot serve as a nonce: The salt must remain constant across a tenant's requests to enable cache reuse, while a nonce must change per request to establish freshness.

##### Relevance to Photon Intelligence

A clean-slate stack can enforce one hashing contract end to end, eliminating Dynamo's cross-engine translation problem. Remaining risks are drift from later features, hash reimplementation across language boundaries such as CUDA kernels, mixed versions during rolling upgrades, and external dependencies with their own keys. Mitigations are a type-level contract in which a `BlockHash` is constructible only from a verified `TenantScope`, a hash-scheme version handshake, and shared test vectors.

#### Step 4: Workspace Scaffold

##### Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Project name | `dynamo_avo` | Owner selection |
| Crate prefix | `avo-` | Avoids collision with upstream `dynamo-*` crates used as dev-dependencies |
| Visibility | Private | History can be cleaned before exposure |
| Toolchain | Rust 1.96.1, edition 2024 | Matches Dynamo |
| Reference crates | crates.io `dynamo-kv-hashing` and `dynamo-tokens` at `=1.5.1` | Source identical to `6e4221b` for both crates; avoids cloning Dynamo in CI |

##### Contents

| Path | Contents |
|---|---|
| `crates/avo-kv-hash` | Milestone 1 stub, three Dynamo reference tests, Dynamo throughput baseline benchmark |
| `crates/avo-block-pool` | Milestone 2 stub |
| `crates/avo-radix-index` | Milestone 3 stub |
| `crates/avo-obs` | Tracing setup, canonical field names, `RequestTimings` for TTFT, ITL, and TPOT, with four tests including a property test |
| `.github/workflows/ci.yml` | Format, Clippy with warnings denied, tests, benchmark compilation |
| `docs/FINDINGS.md` | Per-milestone bottleneck log template |

##### Repository State

- **Location:** `~/dev/avolab-projects/dynamos/dynamo_avo`, remote `git@github.com:n2vo/dynamo_avo.git`.
- **First Commit:** `1863a46`, 21 files, signed and signed off.
- **CI:** Passed on the first run.
- **Excluded From Version Control:** `downloads/`, `docs/ddil_architectural_blueprint*.md`, and `docs/Inference_Serving_Primer.md`, pending a decision on whether they belong in the repository.

#### Open Items

1. Confirm the interview date.
2. Confirm the Verified badge on commit `1863a46`.
3. Decide whether the three excluded documents belong in the repository.
4. Remove or regenerate the stale `GITHUB_TOKEN`.
5. Check GitHub for existing issues on key-epoch detection, PR #11548, and TODO #11971.
6. Update the project primer: Rewrite the Contribution Lead section and retarget the Milestone 6 comparison from `salt.rs` to `lib/kv-router/src/tracking_hash.rs`.

#### Next Step

Phase 1, Milestone 1: Block hashing in `avo-kv-hash`. Property tests are written first, beginning with equality between one-pass and incremental hashing, followed by a timed drill using the artificial intelligence (AI) coding tool permitted in the interview.

#### Sources

- NVIDIA Dynamo Repository: [https://github.com/ai-dynamo/dynamo](https://github.com/ai-dynamo/dynamo)
- PR #11548, Provider-Keyed Tracking Hashes: [https://github.com/ai-dynamo/dynamo/pull/11548](https://github.com/ai-dynamo/dynamo/pull/11548)
- PR #13157, Preserve Cache Salting When Request Extensions Are Disabled: [https://github.com/ai-dynamo/dynamo/pull/13157](https://github.com/ai-dynamo/dynamo/pull/13157)
- PR #14803, SGLang Cache Salt Routing: [https://github.com/ai-dynamo/dynamo/pull/14803](https://github.com/ai-dynamo/dynamo/pull/14803)
- Dynamo Contributing Guide: [https://github.com/ai-dynamo/dynamo/blob/main/CONTRIBUTING.md](https://github.com/ai-dynamo/dynamo/blob/main/CONTRIBUTING.md)
- Dynamo Request Extensions and Cache Salt: [https://docs.nvidia.com/dynamo/v1.3.0/additional-resources/nvidia-request-extensions-nvext](https://docs.nvidia.com/dynamo/v1.3.0/additional-resources/nvidia-request-extensions-nvext)
- Dynamo `kv-hashing` 1.5.1 Source: [https://docs.rs/crate/dynamo-kv-hashing/1.5.1](https://docs.rs/crate/dynamo-kv-hashing/1.5.1)
- Developer Certificate of Origin: [https://developercertificate.org](https://developercertificate.org)

#### Acronyms

| Acronym | Expansion |
|---|---|
| CI | Continuous Integration |
| AI | Artificial Intelligence |
| CLI | Command-Line Interface |
| CPU | Central Processing Unit |
| DCO | Developer Certificate of Origin |
| HTTP | Hypertext Transfer Protocol |
| HMAC | Hash-based Message Authentication Code |
| ITL | Inter-Token Latency |
| JWT | JSON Web Token |
| KV | Key-Value |
| KVBM | KV Block Manager |
| LFS | Large File Storage |
| mTLS | Mutual TLS |
| NIXL | NVIDIA Inference Xfer Library |
| PLH | Positional Lineage Hash |
| PR | Pull Request |
| SPIFFE | Secure Production Identity Framework for Everyone |
| SVID | SPIFFE Verifiable Identity Document |
| TLS | Transport Layer Security |
| TPOT | Time Per Output Token |
| TTFT | Time to First Token |
