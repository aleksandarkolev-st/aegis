# Live development observations

The measurements below are historical native-adapter observations, not current direct-provider results. Current evaluation preparation never probes or installs a provider CLI, and new cases freeze Aegis's direct transport. Live evaluations require an explicit model ID and a supported direct provider; Claude remains pending. These functional changes do not authorize running the deferred coding or ARC-AGI-3 comparisons before readiness.

These are small local experiments, not reliability, billed-cost, or production-performance claims. Native calls used an existing ChatGPT login, `codex-cli 0.156.0`, and the explicitly selected `gpt-5.5` model. A model name pin does not guarantee a frozen provider backend or independent sessions.

## Read pilot: 50 tools, two paired repeats

Experiment: `.arun/evaluations/d6391558-cab2-4198-9dea-71befecd78e1/`. Its `experiment.json`, `cases.json`, exact manifests, JSONL results/events, artifacts, and `paired-summary.json` remain in the local workspace. Fixture source SHA256: `2113e3e858fb20131b8e2d2819fe59741081e978094d90bc8ecd0e03e8471b06`.

Each case had 12 action, 200,000 model-token, 256,000 context-character, and 180-second execution limits. Mode order rotated between repeats. The same read task, fixture implementation, model, grants, and budgets were used. Acceptance checked the exact marker and successful-operation evidence independently of the model's completion claim.

| Mode | Repeat | Accepted | Reported tokens | Execution ms | Peak schemas | Peak schema bytes |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| eager | 0 | yes | 28,224 | 10,446 | 50 | 15,682 |
| lazy | 0 | yes | 31,454 | 17,040 | 3 | 877 |
| artifact | 0 | yes | 31,477 | 17,750 | 3 | 877 |
| durable | 0 | yes | 31,624 | 16,848 | 3 | 877 |
| eager | 1 | yes | 28,234 | 10,238 | 50 | 15,682 |
| lazy | 1 | yes | 31,393 | 15,299 | 3 | 818 |
| artifact | 1 | yes | 31,408 | 19,732 | 3 | 818 |
| durable | 1 | yes | 31,613 | 18,256 | 3 | 877 |

There were no fixture-defined wrong-tool choices, invalid arguments, or repeated dispatches in these eight cases. That is not evidence of zero duplicate external effects in general.

Candidate-minus-eager paired mean token differences were +3,194.5 (lazy), +3,213.5 (artifact), and +3,389.5 (durable). The corresponding two-pair percentile-bootstrap intervals were [3,159, 3,230], [3,174, 3,253], and [3,379, 3,400]. Execution differences were +5,827.5, +8,399, and +7,210 ms. These tiny-sample intervals are descriptive, not a robust estimate of future performance. The conservative acceptance-difference interval spans [-1, 1] in every comparison.

The pilot demonstrates lower schema exposure, **not** lower total tokens or latency: discovery adds an extra native-model turn on this small task, and the official CLI supplies its own substantial system context. Schema bytes are UTF-8 bytes, not tokenizer-exact tokens. No prices are assumed. This pilot predates the explicit incomplete-usage counters; newer reports exclude old records without accounting metadata from complete-token comparisons while retaining their reported partial totals separately.

## Endurance workload

A separate repair/build/log task is recorded under `.arun/long-live-20260926/`, run `1a9ff1ad-e805-4f2f-b49a-67a82780d917`. Its immutable contract selects the same native model and a locally pinned Node container image. The task repairs an incorrect exported addition function, checkpoints verified repair evidence, runs a frozen paced log producer, and searches the eventual output artifact for diagnostic/completion markers. Its completion check independently verifies arithmetic, the producer's source hash, a frozen absolute minimum finish time, and the terminal receipt in a read-only container.

This is a **paced synthetic endurance workload**, not two hours of compilation or a throughput benchmark. Completion is not yet claimed here.

Forced interruption exposed an actual Windows ownership bug: killing the supervising runtime did not close the dependency's non-kill-on-close job, so its worker survived. The owned worker tree and named container were explicitly removed before recording a failed external receipt. The fix adds a non-inheritable kill-on-close lifetime job. A subsequent live interruption of the patched runner stopped its worker automatically within two seconds; resumed recovery removed its orphan Docker container and paused the unsafe operation as unknown. Only after host-verified receipts were committed did fresh native turns create new operation identities. The original uncertain intents were not replayed, and repair/checkpoint evidence was retained.

The final attempt through the workspace-local installed npm command, operation `7eb1cd1f-f1a8-4426-b077-b877f64cdf63`, exceeded the 7,200-second worker deadline. Its last inspected receipt remained started, the operation became `outcome_unknown`, and the task paused. Neither final output nor independent acceptance passed. The observed wall-clock gap exceeded the planned pacing interval; its cause is not established. Do not restart or replay this uncertain operation without authoritative reconciliation. The full raw history retains both the failure and the successful ownership regression.

Subsequent host verification confirmed that the named container and known worker were absent, the final receipt was still started rather than completed, and the frozen producer hash was unchanged. An explicit failed reconciliation receipt was committed. Resume recovered with zero unresolved operations, then paused on the expired immutable wall budget without another model call or tool replay. The final operation is now failed, not unknown; this reconciliation does not imply that partial writes were undone or that acceptance passed.

A new independent experiment under `.arun/long-live-v2-20260927/`, run `1df27f86-7d20-448f-a077-8318ca748880`, freezes its producer and acceptance rather than altering the earlier failed task. Its manifest records source commit `1bb0e9c`, private installed binary SHA256 `73c2978ae13ba2191d201a04032b8085b05606b2d5281b6d4de97046326309b5`, producer SHA256 `f00c5246c48e6b170f1f44c46d70d572d4e20f68385f3e164b103f9329100f8a`, pinned image/model, four-hour task budget, two-hour command deadline, 250,000 model-token ceiling and 30 actions. The frozen minimum finish time is 2026-09-26 23:54:38.812 UTC, two hours after fixture preparation. It is still a paced synthetic log/repair task, not a real compilation performance measurement.

The first producer operation `5b291154-2639-44ba-88e5-da081a0b7946` recorded a started receipt with its durable operation/idempotency identities. Host inspection verified the private supervising runtime PID 5108 and child worker PID 7992 before deliberately terminating the runtime. The worker was absent within two seconds; the named orphan container remained until recovery removed it. Recovery paused the unsafe operation as unknown without replay. Only after checking container/worker absence, matching incomplete receipt and unchanged source hash was an explicit failed receipt recorded and the task resumed. Final output and acceptance are not yet claimed. An earlier setup launch `2e4950a1-1530-43fd-8567-8ab93500bd46` lost its CLI options through the Windows CMD shim's trailing-newline task argument; its inspected wrong contract was cancelled and is excluded, not silently counted as the endurance run.

Subsequent read-only inspection at 2026-09-27 08:04 UTC found the v2 owner, worker and named container absent. The resumed operation `59399bb0-0a84-44da-b0b6-a872048b0e5d` had succeeded with exit 0, 20,512,371 output bytes and 6,936,360 ms elapsed; its producer hash, operation/key receipt and fixed finish time were unchanged. A separate model-free, read-only, network-disabled execution of the exact frozen acceptance check passed at 08:06 UTC, recorded in `.arun/long-live-v2-20260927/.arun/external-verification-20260927.json`. However, the original agent remains `waiting_recovery`: its next model request hit subscription usage limits. It records 102,029 model tokens plus an unaccounted failed attempt, with no original runtime acceptance/completion event. Worker success and an external check are not finalized agent completion. No resume, replacement binary, changed workload or mutation of the original database manufactured a successful run.

## Completed matrix and restart observations

The completed experiment `.arun/evaluations/e3e4768c-9f5f-4942-9873-d460967593f6/` contains 48 cases: 50/100/250/500 tools, four modes, three tasks, **one repeat per condition**. It ran the binary built from the metadata-isolation fix, SHA256 `6dc4a3100d1df33fe8d87e71107775ed71750adf615f7adc1cb36052810c2c87`, before later unauthorized-program and post-response token-budget guards.

| Task | Eager accepted | Lazy accepted | Artifact accepted | Durable accepted |
| --- | ---: | ---: | ---: | ---: |
| Read | 4/4 | 4/4 | 4/4 | 4/4 |
| Large log | 0/4 | 0/4 | 4/4 | 4/4 |
| Repair | 2/4 | 3/4 | 1/4 | 2/4 |

All eight eager/lazy large-log cases failed the explicit inline-context limit. Repair failures include model behavior and the pre-fix ungranted-program recovery behavior; results are not retroactively attributed to newer code. One recorded response overshot the token ceiling, motivating the later guard that retains its usage receipt but refuses its action. A single repeat cannot support a useful paired bootstrap interval or reliability claim.

The earlier matrix `.arun/evaluations/c3ce9b0d-9698-4fa6-9dc7-7229c9895acd/` was aborted after acceptance containers failed to mount missing metadata directories on fresh read-only workspaces. Its partial records are preserved but excluded from the completed matrix.

Forced-read restart experiment `.arun/evaluations/08ffcb5c-74e9-4cdf-a3ee-3b44a371e652/` has eight cases at 50 tools, two repeats, cutting after `operation.succeeded`. All eight were actually interrupted. Both durable cases passed independent acceptance; the six non-durable cases failed by design. No repeated dispatch was observed. Durable recovery was 87/91 ms to the first post-restart state/model-start receipt, **not task completion latency**.

Post-program-policy repair pilot `.arun/evaluations/5744e272-01e5-4e8d-bc01-cbf2007aa08c/` has eight cases at 50 tools. All four first-repeat cases passed. All four second-repeat cases failed; inspected native model-failure receipts report the ChatGPT usage limit. Three recorded zero successful-turn tokens but an unaccounted failed attempt, not zero cost. These records are not evidence of model reliability or a complete paired token comparison.
