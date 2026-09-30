# Aegis: Codex, Claude Code, and Vectant patterns for SWE

**Status:** Research and roadmap only. No runtime changes are part of this document.
**Research date:** 2026-09-30.

## Product boundary

Aegis must remain its own agent runtime. It owns the task loop, model routing, tool execution, permissions, durable state, recovery, obligations, and evidence checks. The model proposes one structured action at a time; the Aegis kernel decides whether and how to execute it. Do not invoke the Codex CLI, Codex agent harness, or Claude Code as Aegis's hidden executor.

Keep the model and reasoning effort selected by the user. For Aegis self-improvement and difficult SWE work, the requested route is GPT-6.1-Sol at high effort. A cheaper automatic route can be considered later as an explicit opt-in; it must not silently replace the selected route.

## Research findings

| System | Useful pattern | Aegis application |
| --- | --- | --- |
| Codex | Recent OpenAI guidance says to prune old prompt, skill, and AGENTS.md scaffolding; keep descriptions short; load repository documentation when relevant; and avoid requiring a full codebase map for every small task. | Keep Aegis's always-present protocol small and precise. Add detailed playbooks and repo context only when the task needs them. Define the requested result and stop condition clearly, without a fixed ritual for simple edits. [OpenAI: Rethinking skills and prompts](https://developers.openai.com/blog/rethinking-skills-and-prompts-for-gpt-6-astra) |
| Claude Code | Its workflow pairs scoped requests and automated verification with progressive context loading: short project instructions are always available, skills load when relevant, MCP tool names are visible while full schemas are deferred, and hooks handle deterministic work outside the model context. Larger tasks can explore, plan, implement, and verify; small obvious changes can skip planning. | Separate permanent rules, task-specific workflows, optional tool schemas, and hard runtime checks. Verify SWE changes with focused tests and independent acceptance evidence. [Claude Code best practices](https://code.claude.com/docs/en/best-practices), [Claude Code extension and context loading](https://code.claude.com/docs/en/features-overview) |
| Vectant | Its code-intelligence path routes queries to likely files and symbols, ranks lexical and structural results, uses a confidence-based fast path with a slower fallback, applies deterministic context limits, and loads selected code chunks on demand with content-hash and file-stat checks. | Use a lightweight router before loading costly schemas or source text. Return only high-confidence candidates, then load exact source slices as needed and reject stale indexed content. See the pinned [Vectant QueryRouter](https://github.com/aleksandarkolev-st/vectant/blob/a5b6a91d52bba59bcdbd6b8e086fbb2e56489722/ai-backend/ai-engine/code_intel/routing/router.py), [retrieval pipeline](https://github.com/aleksandarkolev-st/vectant/blob/a5b6a91d52bba59bcdbd6b8e086fbb2e56489722/ai-backend/ai-engine/code_intel/retrieval/pipeline.py), [deterministic retrieval controller](https://github.com/aleksandarkolev-st/vectant/blob/a5b6a91d52bba59bcdbd6b8e086fbb2e56489722/ai-backend/ai-engine/code_intel/retrieval/controller.py), and [lazy, hash-checked SemanticChunk loading](https://github.com/aleksandarkolev-st/vectant/blob/a5b6a91d52bba59bcdbd6b8e086fbb2e56489722/ai-backend/ai-engine/code_intel/core/types.py). |

### Vectant caveat

Vectant's demonstrated lazy-loading pattern is for code bodies, not proof that all tool schemas are deferred: its [tool registry](https://github.com/aleksandarkolev-st/vectant/blob/a5b6a91d52bba59bcdbd6b8e086fbb2e56489722/ai-backend/ai-engine/code_intel/tools/tool_registry.py) exposes registered definitions through its OpenAI-format path. Its [intent classifier](https://github.com/aleksandarkolev-st/vectant/blob/a5b6a91d52bba59bcdbd6b8e086fbb2e56489722/ai-backend/ai-engine/code_intel/routing/intent_classifier.py) can also call Gemini when configured, which may add a remote model request. Aegis should copy the retrieval and integrity ideas, but start with local routing and only add a model-backed router if measured quality gains justify its extra call.

### Aegis baseline from this repository

- **The runtime boundary is already right.** `src/kernel.rs` owns the action loop and structured state; `src/direct.rs` sends a narrow Aegis action request to the selected provider. The runtime does not need a Codex subprocess.
- **Capability discovery is safe but model-mediated.** `src/capability.rs` filters manifests by granted permission and ranks exact word matches. In non-eager modes the model calls `search_capabilities`, receives up to three matches, then calls a second time to use one. Active schemas are bounded to eight in non-eager modes. This is the clearest routing opportunity: remove avoidable discovery turns without widening grants.
- **Context is already bounded and measured.** `src/kernel.rs` assembles a bounded recent event view, active schemas, task contract, obligations, and applicable policies. `src/tokenization.rs` counts schema exposure, tool-event exposure, and the full prompt with pinned `o200k_base` encoding. `src/budget.rs` enforces action, model-token, wall-time, and context limits.
- **Large outputs already have an artifact path.** `workspace.read_batch` can return selected ranges from several files in one operation; larger results stay artifact-backed and can be inspected in excerpts. Keep this behavior and avoid copying full logs or files into every turn.
- **Completion evidence is already a strength.** Obligations are kernel-owned, successful operation artifacts provide evidence, and an independent acceptance check can gate task completion. Router choices, summaries, and old conversation text must never become permission or proof.
- **Lazy tool discovery has a real overhead trade-off.** `docs/benchmarks.md` records a two-repeat read pilot in which lazy, artifact, and durable modes exposed far fewer schemas but used about 3.2–3.4k more recorded model tokens and took about 5.8–8.4 seconds longer than eager mode. That tiny pilot is not a final result; it does show that fewer schema bytes alone do not prove fewer total tokens. Avoid adding a separate router model call by default.
- **Current performance evidence is not a fair cross-product win claim.** Existing experiments have small samples, different modes, and incomplete provider receipts in some cases. Compare Aegis improvements first against a pinned Aegis baseline. Treat Codex or Claude Code runs as reference points unless model, task, repository, sandbox, and usage accounting can genuinely be matched.

## Proposed design

### 1. Measure before tuning

Extend the existing evaluation corpus with fixed SWE cases covering a small bug fix, a multi-file feature, a regression test, a refactor, a failing build, a large-log diagnosis, code navigation, and a read-only architecture audit. Include the recent obligation-gap investigation as a bounded audit case: the expected result may be “no concrete defect found,” but it must cite inspected source and stop after a bounded search.

For every run, record the pinned Aegis binary hash, provider, exact model and effort, task, grants, workspace snapshot, budgets, acceptance result, exit codes, provider-reported input/output tokens, estimated versus unaccounted usage, prompt exposure, active schemas, tool-result exposure, action count, repeated searches/reads, and elapsed time. Keep provider usage separate from Aegis's local tokenizer estimate.

Use paired repeats on identical fixtures and rotate variant order. Begin with the user-selected GPT-6.1-Sol/high route for self-improvement. Do not treat schema bytes, tool-result tokens, or an incomplete provider receipt as billed-token savings.

### 2. Add a local, confidence-aware tool router

Build a small in-process router that ranks already-granted capability manifests from the task text and safe local hints. Start with manifest IDs and purposes, a compact intent/synonym map, explicit paths or symbols in the request, and successful accepted workflow patterns as ranking hints. If later evaluation shows that code location is still expensive, add an optional local symbol/file index; do not start with remote embeddings or another model call.

For high-confidence routes, pre-activate only the few likely schemas before the first model request. For ambiguous or unfamiliar requests, retain the existing model-directed discovery path as a fallback. Cache activation within the run and record why the router selected each candidate. Keep the current grant filter before ranking; the router cannot authorize tools or change command, filesystem, network, or provider policy.

The first implementation should test whether it can remove the extra search turn on common tasks while preserving fallback discovery on unfamiliar tasks. Limit the candidate count by evidence from the evaluation set rather than hard-coding a universal route for every task.

### 3. Load source and tool detail progressively

Keep a compact catalog of capability names and purposes available to the router. Add full schemas to model context only for activated capabilities. Prefer known paths, symbols, and small source ranges over broad file dumps. Reuse `workspace.read_batch` for grouped, selected reads; preserve artifact handles for large outputs.

If a file/symbol index is added, store source revision hashes with its entries. Load source only after ranking chooses a range, verify the indexed revision when reading, and fall back to fresh text search when the index is stale or uncertain. Treat summaries as navigation aids, never as evidence that a change passed.

Do not copy Vectant's optional external intent-model call into the default Aegis path. A local router should not spend model tokens to decide which other model tools to show unless evaluation demonstrates a net quality and efficiency improvement.

### 4. Keep each request context task-relevant

Audit the fixed instructions and dynamic policy text. Keep the action protocol, safety boundary, current task, applicable user rules, active obligations, and essential current evidence. Keep verbose workflows out of every request; load them only for matching task types. Preserve the durable event history in Aegis storage while giving the model a short current-state projection.

For long tasks, define a compact continuation record that preserves the immutable task/acceptance contract, active obligations, decisions, changed paths and revisions, verified evidence hashes, exact test commands/results, unresolved questions, and one next action. Drop duplicated explanations and raw outputs that remain available as artifacts. A summary or checkpoint remains context, not proof.

Add a no-progress guard for repeated identical searches, repeated reads without a decision, invalid action loops, and long reasoning/tool sequences without an accepted milestone. On threshold, checkpoint the useful state and show the user the remaining blocker or ask a focused question. Do not silently raise a budget or switch models to escape a stalled loop.

### 5. Make `/goal` a verifiable SWE workflow

Keep `/goal <text>` and pasted multiline input as task submission. Extend the goal/contract experience so a user can see the goal, acceptance checks, current milestone, changed files, tests and results, budget remaining, and next action from the running task.

Use a lightweight sequence: inspect first; plan when the request spans subsystems or the approach is uncertain; implement one coherent change; run the narrowest useful tests; run independent acceptance when configured; report evidence. A one-line, obvious fix should not pay for a separate plan turn. A broad audit should return specific findings or explicitly state that none were verified, rather than reading without a stopping condition.

Keep deterministic requirements in the kernel or executable checks. Model-written milestone text may describe progress, but it cannot delete obligations or bypass acceptance. Preserve `/` command discoverability and make every goal-related command behave as a real action or return a clear reason when it cannot.

### 6. Delegate selectively and isolate high-volume research

After the single-agent route and budgets are measured, consider Aegis-owned read-only investigators for independent questions that would otherwise flood the main task with search results, logs, or source text. Return concise findings with file paths, line ranges, evidence hashes where available, and confidence. Use a separate read-only reviewer after implementation for adversarial verification.

Only delegate work that is independent and large enough to offset an extra model request. Give each worker scoped tools and a small task; share a total task budget across workers; keep writes and overlapping edits with the main Aegis run unless explicit coordination is added. Do not spawn workers for simple tasks. Preserve the user's selected model and effort unless the user enables a distinct cost-routing mode.

## Delivery sequence: small atomic commits

1. **Baseline only:** freeze SWE fixtures, acceptance checks, usage accounting, and a report of the existing eager/lazy behavior.
2. **Router in isolation:** add a deterministic router and ranking tests; no change to execution authority.
3. **Schema pre-activation:** connect high-confidence route results to the existing activation ledger; test grant filtering, fallback discovery, exact schema availability, and unknown requests.
4. **Progressive source retrieval:** only if measurements justify it, add the smallest useful file/symbol index and hash-checked range loading.
5. **Context summary and stall guard:** add bounded continuation state and repeated-no-progress detection, keeping complete events and artifacts intact.
6. **Goal acceptance flow:** expose acceptance and evidence status in `/goal`/`/status` and test paste, pause, resume, finish, and failure recovery.
7. **Optional read-only workers:** add isolated investigation/review only after single-agent gains are established; enforce shared budgets and explicit stop conditions.
8. **Calibration:** repeat the pinned evaluation matrix with the actual Aegis binary and the requested GPT-6.1-Sol/high route. Promote only changes that pass quality and efficiency gates.

Keep each numbered change in its own reviewable commit. Do not bundle router, context compaction, and delegation into a single rewrite.

## Proposed release gates

Set final thresholds after the baseline is captured. Initial targets:

- No regression in permission enforcement, obligation provenance, artifact freshness, recovery behavior, or deterministic acceptance tests.
- No acceptance-rate drop on the fixed SWE regression set. A failure in any kernel safety or acceptance case blocks promotion.
- At least 15% lower median recorded provider input tokens on multi-turn cases with five or more tool actions, measured on complete usage receipts and paired runs.
- No additional provider call for high-confidence routed tasks; ambiguous tasks may use the existing discovery fallback.
- No more than a 5% increase in model calls or p95 wall time on simple tasks. If a router increases either, it must show a compensating quality gain on the relevant cases.
- Report exact usage separately from estimates and unaccounted attempts. Never claim a token win from prompt characters, schema bytes, or incomplete usage alone.
- Recheck all gates using the packaged Aegis binary, not only unit tests or a development build.

These are proposed gates, not results. Aegis should compete on verified task success, lower measured input/context waste, recovery and control, and fast direct interaction. Avoid claiming an overall win over Codex or Claude Code from mismatched model/provider runs.

## Repository references used for the Aegis baseline

- [docs/benchmarks.md](benchmarks.md)
- [src/kernel.rs](../src/kernel.rs)
- [src/capability.rs](../src/capability.rs)
- [src/tokenization.rs](../src/tokenization.rs)
- [src/budget.rs](../src/budget.rs)
- [src/obligations.rs](../src/obligations.rs)
