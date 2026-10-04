# Task controls

Typing `/` at the task prompt opens the searchable command menu. Scroll or type to filter all commands and aliases; selection fills the prompt, and Enter runs it. Slash commands inspect the selected saved task. While following a task, `/` opens the same menu; choosing a permitted inspection or pause applies it to the running task. F3 selects another saved task. Inspection uses persisted kernel state and makes no model calls.

| Command | Shows or does |
| --- | --- |
| `/goal`, `/contract` | Original task, retained requirements and states, revision, plan, current provider and reviewed fallbacks |
| `/goal history` | Requirement additions, verifications, staleness and replacements, including archived events |
| `/goal add` | Review and confirm a new requirement with a reason |
| `/goal replace O2` | Review and confirm a replacement; retain O2, its reason and successor ID |
| `/status` | Current route, accounting, elapsed execution time with safe pauses excluded, revision, requirement counts, active milestone and last operation |
| `/why` | Saved next action, unresolved requirements and checkpoint; no hidden reasoning |
| `/evidence O3` | Artifact hashes, integrity, source operations, arguments, receipts, revisions and freshness |
| `/verify` | Outstanding requirements, stale or invalid proofs, unresolved operations and acceptance blockers |
| `/provider`, `/provider history` | Primary, current and fallback routes; transitions with reason and recorded turn |
| `/budget` | Recorded model responses, model tokens and tool-result tokens without aggregate caps; elapsed and remaining execution time with safe pauses excluded; provider input/cached/output when measured |
| `/handoff` | The normalized provider-neutral state used by the next model call |
| `/pause` | Persist a pause request; finish the current safe action boundary, save a checkpoint and stop inference |
| `/resume` | Continue the same run, contract, evidence, accounting and provider route |

F2 or `/providers` selects the provider for new tasks. `/provider` inspects the saved task. Requirement edits require the runner to be stopped; pause first. Neither additions nor replacements rewrite the original task or frozen configuration. Replacements require explicit confirmation and a reason, and the model has no replacement action.

The native CLI exposes the same inspection through `aegis <view> <run-id> [argument]`, plus `aegis pause <run-id>` and `aegis resume <run-id> --foreground`. `aegis models chatgpt` lists the account catalog and supported reasoning levels; add `--refresh` to bypass its 15-minute cache. The interactive model picker also refreshes expired catalogs automatically. Cache bindings include the reviewed catalog compatibility version, so upgrading that version discards obsolete lists.

Tool discovery returns at most three ranked granted schemas. Its saved result reports whether more matches were omitted. A focused search can activate another granted tool; missing from one search result does not establish that the tool is unavailable.

Simple identity questions such as `what model are u` are answered from the persisted active route without inference. The answer names Aegis, the configured model, provider and reasoning level. A fallback's current model is reported, rather than the original primary or a model's guessed identity. These replies remain conversational `answered` records; they cannot bypass explicit obligations or configured acceptance.

Aegis owns its agent loop and tool execution. ChatGPT subscription inference uses direct HTTP transport; it does not launch a Codex CLI or agent harness. The internal `codex` provider alias remains in existing immutable contracts for compatibility.

## Start and finish

An interactive task with an explicit `Requirements:` list shows its contract before inference and offers Start, Edit, Add or Leave paused. Casual requests bypass this preview. Requirements explicitly supplied in configuration are combined with task requirements, so a partial configuration cannot suppress user bullets.

Piped terminal input preserves bounded bracketed multiline pastes as one request, including the explicit requirement list. An incomplete paste is rejected rather than starting its first line as a partial task.

Legacy saved tasks without a complete reviewed contract must be reviewed locally before resuming, answering or completing. F3 offers **Review and adopt legacy task**; `/goal add` also opens that review. Adoption retains the original task and history, requires all saved requirements to remain covered, and starts a fresh ledger without carrying old proof into it. Remote resume and approval cannot bypass this review.

The kernel saves its completion explanation in `run.completed`. It contains retained requirement states and evidence, operation/program/exit receipts, final revision, independent acceptance evidence and provider transitions. It can still be inspected after event archival and restart.

## Proof boundaries

Failed operation receipts and their logs remain available through `/artifacts` and model artifact inspection for the owning run. The continuation state retains a bounded summary of recent receipts, including recent process failures, after context rotation and event archival. These summaries count toward tool result exposure; failed receipts cannot verify requirements or complete a task.

Completion rechecks successful operation provenance, artifact integrity, active requirement states and current revisions. Claiming and finishing a write are separate freshness boundaries, even when its outcome fails or is uncertain. Multiple invalidation causes at one terminal transition advance the revision once. Writes also stale proofs for unfinished tasks sharing the workspace. Unknown side effects require reconciliation before a pause can be acknowledged or completion accepted.

For paths observed by workspace read/write/patch tools, Aegis records bounded file digests and detects later external changes before verification, inference and completion. `/verify` reports those changes without mutating state. This watches at most 1,024 observed paths and bounds each file at 2 MiB. It does not watch unobserved dependencies or make an external filesystem transaction atomic.

Process proof also uses a scoped workspace fingerprint at claim, terminal outcome, verification and completion. The snapshot bounds the scan to 100,000 entries and 512 MiB; internal metadata and conventional ignored generated directories are excluded, while tracked files remain included. A process result cannot prove the workspace if its snapshot changed during execution or could not be checked. A fresh read claim detects earlier observed changes before assigning its proof epoch. Neither check makes concurrent external filesystem changes atomic.

Artifact provenance does not establish semantic coverage. Use an independent acceptance check for claims such as API compatibility and parser correctness. Arbitrary prose is retained in the immutable task but is not automatically decomposed into separate requirements. Completed historical runs retain their original verification record.
