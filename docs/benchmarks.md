# Live development observations

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

A separate repair/build/log task is running under `.arun/long-live-20260926/`, run `1a9ff1ad-e805-4f2f-b49a-67a82780d917`. Its immutable contract selects the same native model and a locally pinned Node container image. The task repairs an incorrect exported addition function, checkpoints verified repair evidence, runs a frozen paced log producer, and searches the eventual output artifact for diagnostic/completion markers. Its completion check independently verifies arithmetic, the producer's source hash, a frozen absolute minimum finish time, and the terminal receipt in a read-only container.

This is a **paced synthetic endurance workload**, not two hours of compilation or a throughput benchmark. Completion is not yet claimed here.

Forced interruption exposed an actual Windows ownership bug: killing the supervising runtime did not close the dependency's non-kill-on-close job, so its worker survived. The owned worker tree and named container were explicitly removed before recording a failed external receipt. The fix adds a non-inheritable kill-on-close lifetime job. A subsequent live interruption of the patched runner stopped its worker automatically within two seconds; resumed recovery removed its orphan Docker container and paused the unsafe operation as unknown. Only after host-verified receipts were committed did fresh native turns create new operation identities. The original uncertain intents were not replayed, and repair/checkpoint evidence was retained.

The continued task now runs through the workspace-local installed `aegis` npm command. Its remaining timed execution and independent final acceptance are still pending; the full raw history retains both the original failure and the successful ownership regression.
