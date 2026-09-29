# Kernel obligations and provider transitions

Status: partial implementation. The current source enforces explicit obligation persistence, evidence/revision gating, confirmed supersession, and an opt-in ChatGPT/Grok/OpenAI-compatible custom endpoint same-run route switch. The full contract below also describes open gates; do not infer semantic proof or live cross-provider readiness from unit fixtures.

Implemented in current source:

- `Requirements:` bullets, or an explicitly supplied obligation list, are frozen at run creation separately from model milestones. A task without those still has an umbrella task-request record, but arbitrary prose is not reliably decomposed into separate obligations.
- Explicit obligations require same-run successful-operation artifacts at the current workspace revision before completion. Dispatched write-capable operations conservatively advance the revision and stale prior verifications even if the command fails or its outcome is uncertain; cancellation before dispatch does not. The terminal shows states and offers a user-confirmed replacement with a reason; the old record remains.
- A user-approved different ChatGPT, Grok, or custom-endpoint fallback can be selected in terminal settings after live account/model or endpoint-catalog discovery. The custom route can use a keyless endpoint or a hidden session-only key; its approved profile retains only a non-secret environment-variable reference. Custom-primary tasks can also select a direct fallback. Eligible failed model attempts are recorded before the route changes; the run ID, task contract, permissions, accounting, checkpoint, evidence, and obligation ledger remain in place. The transition is auditable and shown in terminal feedback.

Still open: semantic verification of each obligation beyond artifact provenance; robust extraction/review for arbitrary prose; precise workspace-diff tracking rather than conservative write-capable staleness; owned live failover trials and expiring fallback-session recovery; Claude subscription route; other release-platform verification. A local integration test completes the same run after an injected classified primary failure, but it does not exercise a real hosted quota response. Benchmarks remain deferred until end-to-end readiness.

## Obligations

- A run has a durable, ordered obligation ledger separate from model-authored milestones. The original task and approved obligations are immutable contract inputs; checkpoints may replace milestones but cannot delete or rewrite obligations.
- Each obligation has a stable ID, title, state (`open`, `verified`, `stale`, or `superseded`), evidence references, the workspace revision at verification, and an audit trail. A replacement retains the predecessor and requires a user-approved reason and new obligation ID. The model has no supersede action.
- Completion requires every non-superseded obligation to be verified at the current workspace revision, plus the existing successful-operation evidence and any independent acceptance check. A conversational answer remains explicitly unverified and cannot replace started tool work.
- Verification must bind evidence to the same run and revision. A passing test receipt is not proof of an unrelated semantic claim; configured external acceptance or user review is needed when that distinction matters. The kernel must never label arbitrary operation evidence as semantic proof on its own.
- A committed workspace mutation advances a run-scoped revision and makes earlier verification stale. For operations that may mutate the workspace but whose effect is uncertain, fail closed until the revision can be reconciled. A test result from an older revision cannot satisfy a current obligation.
- Obligations are established before execution from user-reviewed requirements. Automatic extraction may suggest entries but cannot silently drop unparsed request text or invent user approval. Existing runs need a compatibility path that does not retroactively claim stronger verification.

## Same-run provider transition

- The immutable run ID, task, obligations, grants, budgets, operations, evidence, acceptance check, checkpoint, and accounting survive a transition. A separate current-route projection contains only provider/model/reasoning and non-secret credential reference; the initial route stays in the run contract.
- Only a user-approved ordered fallback list may be used automatically. Recoverable provider-side failures such as an accounted usage limit, temporary outage, or removed model can trigger a switch after any in-flight attempt is recorded. Tests, engineering mistakes, and a single malformed response cannot.
- Transition writes an auditable `provider.transition` event with old/new route, classified reason, action/attempt sequence, workspace revision, and accumulated usage. It never replays a pending side effect or resets a budget. If a response may have executed an action but its outcome is unknown, reconciliation precedes transition.
- Context is reconstructed from Aegis state, not a provider transcript. The next provider sees obligations including stale/open states, a bounded checkpoint, verified evidence handles, current revision, and the next action. Provider credentials never enter that context.
- A direct fallback is unavailable at approval unless its sign-in, selected model, account entitlement, and protocol are checked against a live account catalog. A custom fallback needs a validated OpenAI-compatible endpoint and a live advertised model; this does not prove future availability or correctness. No native provider CLI starts. Claude Code subscription sign-in remains unavailable under current third-party authentication guidance; this design does not treat an API key as that subscription login.

## Required tests

1. Replacing the entire milestone list cannot remove or complete an obligation. A finish with one outstanding or stale obligation is rejected before acceptance dispatch.
2. A verified obligation survives restart; a later committed workspace mutation stales it. Evidence from an earlier revision cannot reverify it. A user-approved supersession retains predecessor, reason, and replacement.
3. A quota failure transitions only to an approved available route and keeps the same run ID, budgets, operations, evidence, checkpoint, and obligation ledger. The event and route survive restart.
4. Failed tests and one malformed reply do not switch providers. Missing usage remains unaccounted; an uncertain side effect blocks transition until reconciled.
5. An installed same-terminal run exposes obligations and provider transitions in readable form. It must not dump raw provider JSON, start a provider CLI, or claim full completion from a narrow fixture.
