# Native Codex coding comparison

Existing immutable attempt logs can be summarised without running either model:

```powershell
cargo run --locked --example coding_bench -- --report <path-to-attempts.jsonl>
```

The report separates accepted-task accuracy from independently graded assertion accuracy. Both describe the selected fixture sample. Each harness's token/time efficiency charges failed tasks and correction attempts to its accepted tasks. Missing usage or timing leaves the respective ratio unavailable, with known totals retained. Provider cache tokens remain within gross input totals; these metrics do not estimate billed cost or general model confidence. A paused run whose code passes assertions remains an unaccepted task.

This optional development runner compares Aegis using ChatGPT login with the native Codex coding harness on the same four public, hand-written repair tasks: transactional JSON Patch, bounded asynchronous DAG scheduling, incremental SSE decoding, and sparse interval overlays. It is not SWE-bench, a held-out dataset, ARC-AGI or a demonstration that Aegis is better. Graders are frozen separately from editable code. The default container mode uses a read-only workspace inside the approved network-disabled Node image; the explicit Windows mode below uses native Node.

Preparation makes no model calls:

```powershell
cargo run --locked --example coding_bench -- --prepare-only --repeats 2 --model gpt-5.5
```

Results live in `.arun/coding-bench/<id>/`: source/specification/grader hashes, immutable experiment settings, one workspace per task/repeat/harness, provider stdout JSONL and stderr, Aegis event audits, independent verifier operations/artifacts, and synced attempt start/finish records. `summary.json` totals tokens across failed and successful correction rounds; unknown usage and unfinished attempts cannot become a complete-token comparison. Two corrections are permitted per case. Repeat order rotates between harnesses; learning is disabled in these fresh workspaces.

Exploratory diagnostics can run before production readiness is established. Use `--run --exploratory` with explicitly selected native binaries. The manifest records `production_readiness_verified: false`; this cannot substitute for the production gate. `--tasks`, `--reasoning` and `--attempts` select bounded cases and settings. `--aegis-only` is permitted only in exploratory mode and makes no Codex calls or version queries; it is a single-agent diagnostic, not a comparison:

```powershell
cargo run --locked --example coding_bench -- --run --exploratory --aegis-only --aegis C:/path/to/arun.exe --model gpt-6-luna --reasoning medium --tasks json-patch --repeats 1 --attempts 1 --seconds 180
```

Acceptance requires a passing independent grader, unchanged task specification, successful process exit and, for Aegis, an actually `completed` run. A paused or recovery-waiting run cannot become a successful result merely because its edited code passes assertions.

For native Windows comparison, use `--windows-native --node <absolute-node.exe>`:

```powershell
cargo run --locked --example coding_bench -- --run --exploratory --windows-native --node "C:/Program Files/nodejs/node.exe" --aegis C:/path/to/arun.exe --codex-reference C:/path/to/codex.exe --model gpt-6-luna --reasoning medium --tasks json-patch --repeats 1 --attempts 1 --seconds 300
```

This mode makes no Docker calls. Both agents and the frozen Node grader run on Windows. Codex retains normal user configuration and execution rules, using its supported `--approve-for-me` workspace approval review. Aegis registers its existing trusted Windows host MCP server in each disposable workspace and grants only its PowerShell tool in addition to workspace file tools. Explicit narrowed file scopes are omitted: the kernel correctly prevents trusted host MCP from bypassing them. Node and both Windows helper scripts are hashed; execution dependencies are checked before each attempt. The private grader runs with the current Windows user's privileges, with source-integrity checks, rather than a read-only container. These policies are recorded explicitly and are not equivalent isolation. The native graders are tested against all four broken starters and all four references without agent calls.

Agent execution additionally requires `--run --ready <reviewed-readiness.json> --aegis <verified-native-binary> --codex-reference <reviewed-native-codex-binary>`. The readiness record must have `implementation_complete`, `functional_verified`, and `installed_ux_verified` set to true, an empty `pending` array, and matching `benchmark_binary_sha256`, `aegis_binary_sha256`, `codex_binary_sha256` and `plan_sha256` values. The external comparator must be a regular native executable, not a shell/npm shim; its bytes are checked again before each reference attempt. Aegis itself never resolves, installs or launches a provider CLI. The optional benchmark alone can start this explicitly reviewed comparator after the readiness gate; no PATH fallback exists. Preparation writes the benchmark/plan hashes into its experiment file and starts no reference agent. This is a review attestation, not an automated proof that all product requirements are complete. Do not manufacture it while work or verification is pending. No readiness record is distributed as pre-approved.

In container mode, the runner resolves the already-local Node image to its immutable digest and records native Codex's version before agent calls. Both harnesses use the selected model and task-level wall deadline. That mode runs native Codex with its own tools, `workspace-write`, ephemeral sessions and ignored user configuration/rules. Aegis has scoped file edits and exact containerized Node commands, no aggregate action or token cap; context, response and wall bounds remain explicit. Windows mode uses the configuration and trusted-host policy described above. These are different tool/security policies, not identical sandboxes; native Codex does not expose an equivalent model-action quota. API framing, native schema tokens, backend drift, cached-token prices and billed cost are not inferred. Outer correction rounds are distinct from internal tool/model decisions. Raw records retain Aegis's internal metrics separately.

Functional checks reject all four broken starters, accept all four reference implementations and test correction bookkeeping/preparation without agents. An optional Docker test exercises the isolated graders with those references and verifies unchanged source. The full implementation audit and installed UX verification remain readiness requirements. Graders are ordinary assertion suites, not protection against deliberately malicious runtime monkey-patching or public-test overfitting. Current exploratory results and their limitations are recorded in docs/runtime-audit-2026-10-02.md.

## Controlled acceptance recovery

The separate `acceptance_recovery` example deliberately seeds a known 5/6 interval-overlay candidate into a new run with frozen private acceptance. It tests rejection and repair within that run. It makes no Codex comparison and is not an unbiased coding score or production attestation. Preparation makes no model calls:

```powershell
cargo run --locked --example acceptance_recovery -- --prepare-only --source C:/path/to/failed/index.mjs --binary C:/path/to/arun.exe --model gpt-6-luna --reasoning medium --image sha256:<approved-local-node-image-digest>
```

Use the emitted `workspace`, `state_root`, `run_id` and explicit binary to serve the prepared run under the normal Aegis execution policy. Serving makes real model requests and runs containerized acceptance; the helper does not launch it automatically:

```powershell
Set-Location C:/path/to/emitted/workspace
& C:/path/to/arun.exe serve C:/path/to/emitted/state_root <run_id>
```

Return to this repository after completion and verify its manifest:

```powershell
cargo run --locked --example acceptance_recovery -- --verify .arun/acceptance-recovery/<id>/manifest.json
```

Verification requires the initial 5/6 failure before inference, final 6/6 acceptance before completion, the recorded model/effort, unchanged task/check/runtime/original candidate, intact artifacts and fully reported usage. The original failed trial stays unchanged. Receipts explicitly retain `production_readiness_verified: false`.
