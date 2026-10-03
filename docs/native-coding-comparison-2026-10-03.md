# Prepared-build native coding comparison

The exact prepared Aegis executable was compared with normal native Windows Codex on four frozen public repair tasks, using the requested GPT-6 Luna model and medium reasoning effort. Aegis completed and accepted all four tasks; Codex accepted three. Both agents ran once per task, with a 300-second allowance and no outer correction round.

| Measurement | Aegis | Native Codex |
| --- | ---: | ---: |
| Accepted tasks | 4/4 | 3/4 |
| Independent assertions passed | 23/23 | 22/23 |
| Gross reported tokens | 142,730 | 808,903 |
| Total agent elapsed time | 430.595 s | 391.484 s |
| Tokens per accepted task | 35,682.5 | 269,634.3 |
| Elapsed time per accepted task | 107.649 s | 130.495 s |
| Tasks with missing usage or timing | 0 | 0 |

Efficiency per accepted task charges the failed task's tokens and elapsed time to the accepted tasks. Codex used less total elapsed time in this sample. Cached input remains included in gross input usage; this table does not estimate billed cost. Task acceptance requires independent passing grades, unchanged specification, successful process exit and actual Aegis completion. This sample measures these fixtures, with different native execution policies, rather than general model accuracy.

| Task | Aegis grade | Codex grade | Aegis tokens | Codex tokens | Aegis time | Codex time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| JSON Patch | 7/7 | 7/7 | 37,612 | 143,736 | 85.433 s | 100.909 s |
| DAG scheduler | 5/5 | 4/5 | 18,774 | 266,622 | 88.926 s | 98.070 s |
| SSE decoder | 5/5 | 5/5 | 36,258 | 248,673 | 96.482 s | 127.301 s |
| Interval overlay | 6/6 | 6/6 | 50,086 | 149,872 | 159.754 s | 65.204 s |

Codex's scheduler initialized each dependency adjacency list during the same loop that appended dependents. When a dependent appeared before its prerequisite, the prerequisite's list did not yet exist, producing `Cannot read properties of undefined (reading 'push')` before any callback ran. A separate two-node reproduction confirmed that failure and Aegis's correct parent-then-child execution. The recorded candidates and original grades were preserved. This is a defect in one generated candidate, not a universal Codex failure.

The benchmark controller exited zero. Post-run inspection verified all eight terminal attempt records, runtime/controller/Node/helper/grader hashes, unchanged task specifications and solution hashes, original grader output, actual Aegis completion events and every Aegis model request's selected route. It also reconciled the summary's aggregate token, time, grade and accepted-task totals with the raw attempts.

Artifacts are in `.arun/coding-bench/1d76c83f-64cb-40e8-8f91-ec422e36130d/`: `experiment.json`, `attempts.jsonl`, `summary.json`, `verification-receipt.json`, each agent's logs and Aegis event audit, independent grader receipts, and `dag-order-diagnostic.json`. The benchmark uses native Windows Node and each agent's own tools. Codex retains user configuration/rules with automatic approval review; Aegis uses explicitly granted trusted-host PowerShell authority. The native grader runs with the current user's privileges. These are different isolation policies.

The prepared Aegis SHA-256 is `8da8867a444f1fea1d753c10c5d46d645d3e70d5d46d27fbb8024517934445e1`; native Codex 0.159.0 is `0e2a4cd6ac1b329e64ec74745e38b71a3d4fa701102c86f49cf605d23a02c4df`; benchmark controller is `6117426c96f46207853841f3c31537d0d5931189828c14d0550f5cc947d583e8`. The previous older-build 3/4 Aegis versus 4/4 Codex result remains unchanged in experiment `e32d8c6c-4051-47e4-97cd-1834b35913df`.

That exact executable was globally installed on October 3 and completed its separate native two-hour trial; installed terminal, host and recovery checks passed. A later October 4 executable includes [the README exploration repair](readme-read-loop-2026-10-04.md), with separate verification. See [the readiness checklist](native-readiness-checklist.md) for the pinned independent gates. This four-fixture comparison does not attest general production readiness.
