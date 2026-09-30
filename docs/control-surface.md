# Task controls

Slash commands inspect the selected saved task. Press `/` while following a task to enter a command. F3 selects another saved task. Inspection uses persisted kernel state and makes no model calls.

| Command | Shows or does |
| --- | --- |
| `/goal`, `/contract` | Original task, retained requirements and states, revision, plan, current provider and reviewed fallbacks |
| `/goal history` | Requirement additions, verifications, staleness and replacements, including archived events |
| `/goal add` | Review and confirm a new requirement with a reason |
| `/goal replace O2` | Review and confirm a replacement; retain O2, its reason and successor ID |
| `/status` | Current route, accounting, elapsed wall time, revision, requirement counts, active milestone and last operation |
| `/why` | Saved next action, unresolved requirements and checkpoint; no hidden reasoning |
| `/evidence O3` | Artifact hashes, integrity, source operations, arguments, receipts, revisions and freshness |
| `/verify` | Outstanding requirements, stale or invalid proofs, unresolved operations and acceptance blockers |
| `/provider`, `/provider history` | Primary, current and fallback routes; transitions with reason and recorded turn |
| `/budget` | Used, limit and remaining actions, model tokens, tool result tokens and wall seconds; provider input/cached/output when measured |
| `/handoff` | The normalized provider-neutral state used by the next model call |
| `/pause` | Persist a pause request; finish the current safe action boundary, save a checkpoint and stop inference |
| `/resume` | Continue the same run, contract, evidence, accounting and provider route |

F2 or `/providers` selects the provider for new tasks. `/provider` inspects the saved task. Requirement edits require the runner to be stopped; pause first. Neither additions nor replacements rewrite the original task or frozen configuration. Replacements require explicit confirmation and a reason, and the model has no replacement action.

The native CLI exposes the same inspection through `aegis <view> <run-id> [argument]`, plus `aegis pause <run-id>` and `aegis resume <run-id> --foreground`. `aegis models chatgpt` queries the account catalog and lists the supported reasoning levels.

## Start and finish

An interactive task with an explicit `Requirements:` list shows its contract before inference and offers Start, Edit, Add or Leave paused. Casual requests bypass this preview. Requirements explicitly supplied in configuration are combined with task requirements, so a partial configuration cannot suppress user bullets.

The kernel saves its completion explanation in `run.completed`. It contains retained requirement states and evidence, operation/program/exit receipts, final revision, independent acceptance evidence and provider transitions. It can still be inspected after event archival and restart.

## Proof boundaries

Completion rechecks successful operation provenance, artifact integrity, active requirement states and current revisions. Dispatching a write advances the revision even when its outcome fails or is uncertain. Writes also stale proofs for unfinished tasks sharing the workspace. Unknown side effects require reconciliation before a pause can be acknowledged or completion accepted.

For paths observed by workspace read/write/patch tools, Aegis records bounded file digests and detects later external changes before verification, inference and completion. `/verify` reports those changes without mutating state. This watches at most 1,024 observed paths and bounds each file at 2 MiB. It does not watch unobserved dependencies or make an external filesystem transaction atomic.

Artifact provenance does not establish semantic coverage. Use an independent acceptance check for claims such as API compatibility and parser correctness. Arbitrary prose is retained in the immutable task but is not automatically decomposed into separate requirements. Completed historical runs retain their original verification record.
