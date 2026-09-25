# Runtime contracts

The task request, workspace path, provider, capability grants, budgets, and acceptance criteria are immutable once a run is created. A run has a stable UUID and one of `ready`, `running`, `waiting_recovery`, `completed`, `failed`, or `cancelled`. A client attachment is not the run: detaching or losing the terminal never changes the run state. Only a verified result with evidence may complete a milestone or the run.

Every state change is committed with a monotonically increasing per-run event sequence. Events contain their type, timestamp, operation or model-turn ID where applicable, and a bounded JSON payload. A replay reads committed events and artifacts without calling the model or executing tools. The current snapshot is a projection of this log, not a substitute for it.

An operation has a stable ID, capability version, validated arguments, idempotency key, side-effect class, deadline, and state. Its transitions are `pending -> dispatched -> succeeded | failed | timed_out | cancelled | outcome_unknown`. The intent and `pending` event commit before dispatch; a durable artifact is written and synchronized before the result event references it. A timeout does not establish that an external side effect did not happen.

On restart, an operation without a terminal outcome is reconciled. A read-only/idempotent operation may be repeated with the original idempotency key. An externally queryable operation checks the external system first. A non-idempotent operation whose outcome cannot be queried becomes `outcome_unknown`, pauses the run, and requires explicit reconciliation; it is never blindly retried. The worker enforces the kernel's grant and deadline, not merely the model-visible manifest.

## Crash cases

1. Crash after intent commit and before worker dispatch: restart sees `pending`; a safe action can dispatch once, and an unsafe action requires reconciliation unless non-dispatch is provable.
2. Crash after external side effect and before result commit: restart sees `dispatched`; it queries external state or pauses. It must not duplicate an unsafe side effect.
3. Crash after artifact write and before result commit: the artifact may be orphaned. Recovery treats the operation as unresolved and cleanup may remove unreferenced artifacts; no event may reference missing bytes.

## Evaluation contract

Use the same model, prompts, tool implementations, grants, time and token budgets for paired runs. Compare eager schemas, lazy discovery, lazy plus artifact-backed context, and the durable runtime. Registry sizes: 50, 100, 250, and 500, with overlapping names and deliberately irrelevant capabilities. Record schema tokens, total model tokens, wrong-tool choices, invalid arguments, elapsed time, completion against external acceptance tests, duplicate side effects, and recovery time; report paired observations and uncertainty, not invented targets.

Initial tasks: locate a symbol among similarly named repository search tools; identify a failing test from a large compiler log by inspecting a slice; repair a small repository under an acceptance test; interrupt a read-only operation before its result; interrupt a non-idempotent external operation after dispatch; restart after artifact creation before event commit. A baseline is reproducible only when task fixtures, tool registry, model/version, seed where supported, budgets, and raw event logs are saved.
