# Plan implementation audit — 2026-09-27

Scope: the full `plan.txt`, native ChatGPT/Claude/Grok login operation, custom OpenAI-compatible endpoints, installable `aegis-arun`, and the user's same-terminal autonomous UX. This is a requirement audit, not a completion claim. Previous UI work is verified progress; it does not prove the whole goal.

## Runtime requirements

| Requirement | Authoritative implementation/evidence | Status or remaining gate |
| --- | --- | --- |
| Tiny stable model interface; deterministic kernel owns execution | `src/model.rs` six action variants; `src/kernel.rs` action validation/context/dispatch; native adapters disable their own tools | Implemented; real read/repair observations and mocked action tests cover the interface |
| Immutable request/workspace/grants/budgets/provider/acceptance; stable run ID | `src/storage.rs` run creation; `docs/contracts.md`; saved evaluation contracts | Implemented; changes apply to new runs, not mutable old contracts |
| Durable milestones with evidence and tractable next work | Storage checkpoint/milestone validation; kernel milestone policy and handoff | Implemented evidence gate; model-dependent planning quality is not guaranteed |
| Reconstruct context from state, not lifetime transcript | Twelve recent events, four bounded previous summaries, checkpoint, eight working schemas; archive/snapshot tests | Implemented; long-history synthetic tests and forced restart cover reconstruction |
| Versioned manifests outside prompt, permissions, ranking, cost and bounded activation | `src/capability.rs`, `Store::activate`, eight-schema eviction tests, 50/100/250/500 fixture registries | Implemented lexical ranking and cost tie-break; no claim of learned semantic routing |
| Capability-based filesystem, process and network security at execution boundary | Worker direct grant/schema/version checks; Docker read-only root/workspace policy, hidden metadata, program grants, immutable exact command scopes, no network; MCP isolated or explicit host trust | Exact program/argument-vector restrictions now reject before kernel intent and direct-worker claim, with inline guided configuration and a successful isolated Docker execution. Fine-grained filesystem/domain scopes remain missing; textual `process:cargo:test` syntax is not supported, while equivalent exact argv policy is |
| Dangerous tools out of process; narrow IPC; deadlines, cancellation and tree ownership | Managed worker subprocesses, JSON IPC, Windows kill-on-close job; process/parent-crash/scoped-interruption tests | Implemented tested Windows ownership; no actual Linux/macOS release build evidence |
| Ordered durable intent/model/result/state events; side-effect-free replay | SQLite WAL/FULL, transactionally sequenced events, synced SHA256 artifacts; full-audit/archive tests | Implemented; native replay does not call providers or tools |
| Idempotent retries and unknown unsafe outcomes | Atomic worker claim; same operation/key for safe retry; explicit external receipts for unsafe reconciliation; restart fixtures | Implemented. No exactly-once external-effect claim |
| Snapshot plus tail recovery and older audit archival | `src/history.rs`, projection migration, 3,000-turn history tests and archived restart fixture | Implemented and integrity checked |
| Model tokens, tool-result tokens, wall/process/network/action budgets | Model turn-boundary token guard, wall/process/actions/context limits; fixed adapter output/network isolation bounds; configurable native/HTTP response capture budget with oversized stdout/stderr/reply and chunked-body tests | Partial: explicit tool-result token accounting/limits and scoped network-byte budgets need stronger coverage. Native CLI HTTPS bytes are not observable and must not be reported as zero |
| Bulky output stored as handles; slice/search inspection | Worker output artifacts, bounded kernel inspection, linked evidence; multi-megabyte Docker tests and large-log matrix; persisted result target/bytes/exit/timing and human result summaries | Implemented; result metadata/unit and end-to-end read assertions cover the added normal feedback |
| Structured evidence-backed handoffs across contexts | Checkpoints with decisions/unresolved/next action/milestones; completion evidence and independent acceptance gate | Implemented; successful multi-hour live completion is still missing |
| MCP at edge, untrusted annotations do not grant permission | `src/mcp.rs`, capability mapping, Docker isolation tests, direct adapter authorization tests | Implemented synchronous tool edge. Optional protocol Tasks extension is not a kernel requirement and is not claimed |
| Trace of model → lookup → execution → result, tokens/timing/cost | `src/trace.rs` and paired reports; missing/estimated usage exclusions | Tokens/timing implemented; actual billed prices unavailable, schema exposure currently measured in UTF-8 bytes rather than tokenizer-exact tokens |

## Evaluation and build-order gates

| Stage / deliverable | Evidence | Remaining gate |
| --- | --- | --- |
| 1. Written contracts, failure model, baseline task set | `docs/contracts.md`; saved fixture manifests/contracts/source hashes; evaluation preparation tests | Reproducible local fixture generation exists; live backend/version drift is labeled |
| 2. Durable Rust/SQLite/artifact/worker vertical slice; crash several points | `tests/restart.rs`, `tests/interrupts.rs`, `tests/parent_crash.rs`, artifact/claim tests | Passed local tests; unsafe outcomes are paused rather than guessed |
| 3. Lazy overlapping capabilities | Resolver tests; 50/100/250/500 complete matrix | Implemented and measured, without an improvement claim |
| 4. Virtualization, budgets, checkpoints, evidence | Large-log matrix; archive/context/checkpoint/acceptance tests | Multi-hour native task did not complete successfully; token/byte distinctions must remain explicit |
| 5. Isolation and execution controls; adapter cannot bypass rejection | Direct-worker tests, four Docker tests, scoped interruption, exact command scope preflight/claim tests | Exact command scopes implemented; fine-grained filesystem/domain scopes remain a gap |
| 6. MCP, trace, benchmark runner, paired interrupted results | `docs/benchmarks.md`; complete 48-case ordinary matrix; eight-case forced-read restart; paired report tests | Only one repeat per large-matrix condition. Later repair repeat hit provider usage limits; successful multi-hour live demonstration missing |
| Metrics: acceptance, schema/total tokens, wrong choices, invalid args, duplicate effects, latency/search/recovery | Raw JSONL/events plus independent fixture acceptance; unknown attempts excluded from complete-token pairs | Schema tokenizer-exact counts and general external duplicate-effect observations remain unavailable; repeated dispatch is only a proxy |
| Honest paired results and uncertainty, including context overflow | Paired reports validate matching metadata and missing/estimated receipts; all eight inline log cases overflowed explicitly | Completed matrix is descriptive at one repeat; do not invent confidence or price claims |

## Terminal and distribution requirements

| Requirement | Evidence | Status |
| --- | --- | --- |
| Terminal is a detachable client; prompt in native scrollback | `src/session.rs`, Crossterm without alternate screen, installed Windows PTY exercise | Implemented; Ctrl+D exits attachment without cancelling the run |
| Natural-language requests, editable input, animations, keyboard menus | Installed F1/F2/F6/F7 flow, provider/model/settings/recovery fixtures, reduced-motion/color controls | Implemented; no shell commands needed for normal configuration/recovery |
| Readable errors and same-window operation | Shared human event renderer; native-error and Windows no-console tests; installed npm launcher | Implemented; explicit raw replay/diagnostic views remain separate |
| Inspectable tasks/context/tools/artifacts/trace/checkpoint/cancellation | F3 views, advanced CLI, F8 checkpoint and confirmed F9 whole-task cancellation; optional `/checkpoint` and `/cancel` | Implemented; fixture verifies checkpoint display/cancellation do not revive or replay an unknown write |
| Compact context/tool/artifact/operation/checkpoint accounting | Two-line live activity area: last-model character/schema counts, durable operation/evidence counts and checkpoint age | Verified in an installed Windows PTY with a delayed decision fixture; F8 checkpoint and F9 keep-running confirmation were exercised without opening another window |
| Tool output / verified result / warning distinction, large-output metadata | Semantic operation labels, measured result metadata and separate warning/success tones, including nonzero/unknown exit status | Implemented and tested; no JSON previews dumped in normal completion feedback |
| Native ChatGPT login works | Verified read pilot, matrix and UI tasks using Codex login, including a fresh bounded `gpt-5.5` read with response capture guards | Proven for observed calls; historical subscription usage limits are recorded, not bypassed |
| Native Grok login works | Independently accepted native Grok read task and current model picker | Proven for observed read; no broad reliability estimate |
| Native Claude login works | Adapter and native login flow; expired saved session | Live completion pending at the user's explicit request; do not silently reauthenticate |
| Custom OpenAI-compatible endpoint | Authenticated local HTTP tests across schema/json/prompt formats and `/models` | Local compatible protocol verified; unspecified production endpoint not certified |
| Installable Node package, `aegis` / `arun` commands | Bundled local tarball; updated user-global installation and launch checks | Windows local install verified; public publication and release assets not authorized/verified |
| Cross-platform release distribution | Five-platform checksum/version workflow and four npm tests | Actual Linux/macOS CI builds and release assets still missing |

## Current endurance state

The existing run `1a9ff1ad-e805-4f2f-b49a-67a82780d917` is terminally paused, not live. Its last operation `7eb1cd1f-f1a8-4426-b077-b877f64cdf63` became unknown after a 7,200-second worker timeout; its receipt says started, not completed. Host verification found the named Docker container and known worker PID absent and the frozen producer unchanged. An explicit failed reconciliation receipt was then recorded; the operation is now failed, with zero unresolved operations. Resume honored the expired immutable three-hour task wall budget and paused without another model call or tool replay. Neither output completion nor acceptance passed. Do not extend that old contract or shorten its frozen producer to manufacture success. A future measured demonstration needs a new explicitly recorded contract and acceptance evidence, preserving this failed run.

A new separately frozen experiment lives under `.arun/long-live-v2-20260927/`, run `1df27f86-7d20-448f-a077-8318ca748880`, pinned `gpt-5.5` and the private installed binary built from `1bb0e9c`. It has a four-hour task budget, two-hour process deadline, approximately 20 MiB output, exact `node [long-build.mjs]` command scope and independent source-hash/elapsed-time/repair/receipt acceptance. Its first started producer operation was deliberately interrupted; verified worker death and orphan-container cleanup preceded an explicit failed receipt. Resume preserves the same frozen absolute finish time (2026-09-26 23:54:38.812 UTC). This is ongoing evidence, not a completion claim; revalidate the actual process/operation/container before treating it as live.
