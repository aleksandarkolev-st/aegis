# Native Codex coding comparison

This optional development runner compares Aegis using ChatGPT login with the native Codex coding harness on the same four public, hand-written repair tasks: transactional JSON Patch, bounded asynchronous DAG scheduling, incremental SSE decoding, and sparse interval overlays. It is not SWE-bench, a held-out dataset, ARC-AGI or a demonstration that Aegis is better. Graders are frozen separately from editable code and execute with a read-only workspace inside the approved network-disabled Node image.

Preparation makes no model calls:

```powershell
cargo run --locked --example coding_bench -- --prepare-only --repeats 2 --model gpt-5.5
```

Results live in `.arun/coding-bench/<id>/`: source/specification/grader hashes, immutable experiment settings, one workspace per task/repeat/harness, provider stdout JSONL and stderr, Aegis event audits, independent verifier operations/artifacts, and synced attempt start/finish records. `summary.json` totals tokens across failed and successful correction rounds; unknown usage and unfinished attempts cannot become a complete-token comparison. Two corrections are permitted per case. Repeat order rotates between harnesses; learning is disabled in these fresh workspaces.

Agent execution additionally requires `--run --ready <reviewed-readiness.json> --aegis <verified-native-binary>`. The readiness record must have `implementation_complete`, `functional_verified`, and `installed_ux_verified` set to true, an empty `pending` array, and matching `benchmark_binary_sha256`, `aegis_binary_sha256` and `plan_sha256` values. Preparation writes the benchmark/plan hashes into its experiment file. This is a review attestation, not an automated proof that all product requirements are complete. Do not manufacture it while work or verification is pending. No readiness record is distributed as pre-approved.

The runner resolves the already-local Node image to its immutable digest and records native Codex's version before agent calls. Both harnesses use the selected model and task-level wall deadline. Native Codex retains its own tools with `workspace-write`, ephemeral sessions and ignored user configuration/rules. Aegis has scoped file edits and exact containerized Node commands, 80 actions and 400,000 model tokens per outer attempt. These are different tool/security policies, not identical sandboxes; native Codex does not expose an equivalent model-action quota. API framing, native schema tokens, backend drift, cached-token prices and billed cost are not inferred. Outer correction rounds are distinct from internal tool/model decisions. Raw records retain Aegis's internal metrics separately.

Functional checks reject all four broken starters, accept all four reference implementations and test correction bookkeeping/preparation without agents. An optional Docker test exercises the isolated graders with those references and verifies unchanged source. The full implementation audit and installed UX verification remain readiness requirements. Graders are ordinary assertion suites, not protection against deliberately malicious runtime monkey-patching or public-test overfitting. No new agent comparison has started.
