# Agent Microkernel Runtime

`arun` is a Rust terminal agent with a durable SQLite event log, content-addressed artifacts, permission-filtered capability discovery, and out-of-process execution. The model returns one structured action at a time; the kernel owns state, grants, retries, context assembly, and evidence.

## Build and sign in

```powershell
cargo build --release
target/release/arun login chatgpt
target/release/arun login claude
target/release/arun login grok
target/release/arun probe chatgpt
```

Each provider adapter launches its installed CLI (Codex for a ChatGPT login, Claude Code for a Claude login, Grok Build for a Grok login). Credentials stay with those CLIs; arun does not read or copy their token stores. Re-authenticate with the corresponding `login` command if a provider reports an expired session. A provider may also refuse work when its account has no remaining usage balance.

## Run and recover

```powershell
arun run "Find the source of the failing test" --provider chatgpt
arun attach <run-id>
arun status <run-id>
arun tasks <run-id>
arun context <run-id>
arun artifacts <run-id>
arun replay <run-id>
arun trace <run-id>
arun inspect <artifact-hash>
arun cancel <run-id>
arun resume <run-id>
```

`run` starts a detached process by default; closing an attachment does not stop it. `--foreground` runs in the current terminal. Calling `arun` with no arguments opens a scrollback-style prompt; Ctrl-D detaches. State lives under the workspace's ignored `.arun/` directory. Replay prints committed JSON events without repeating model calls or side effects.

Interrupted read-only operations can replay with the original operation and idempotency key. An interrupted write or external call pauses with `outcome_unknown`; it is not blindly retried. After checking the external outcome, reconcile explicitly:

```powershell
arun resolve <run-id> <operation-id> succeeded "receipt or verification note"
arun resume <run-id>
```

Run options include `--actions`, `--model-tokens`, `--wall-seconds`, `--context-chars` (default 256,000), and `--mode eager|lazy|artifact|durable`. Eager exposes all granted schemas; lazy discovers them on demand. Both inline complete results and disable artifact inspection. Artifact adds bounded result handles and inspection; durable also reconciles interrupted operations. Non-durable modes fail on process restart. Context overflow is recorded as a failure, not silently truncated. Provider-reported token counts are recorded where available; otherwise counts are marked estimated. `trace` shows committed model, discovery, operation, and state transitions with schema-byte, token, and timing metrics; `replay` retains raw JSON events. Completion requires successful-operation artifact evidence, and any planned milestones must carry evidence. This is provenance checking, not a substitute for external acceptance tests.

## Capabilities and isolation

Read-only workspace search/read are granted by default. Add `--allow-write` to grant exact workspace writes. Paths cannot traverse out of the workspace or enter `.git` or `.arun` through the built-in file tools. `process.run` is disabled unless a specific program and a locally available Docker image are granted:

```powershell
arun run "Run the tests" --allow-process node --image node:22-alpine
```

The worker uses a read-only root filesystem, a bind-mounted workspace (read-only unless `--allow-write`), no container network, dropped capabilities, resource limits, and an ephemeral mount hiding `.arun`. Images are never pulled automatically. Process output up to 32 MiB is stored as a separate artifact instead of being dumped into model context. Docker Desktop or an equivalent Docker daemon must be running for this capability.

Register a trusted local stdio MCP server and grant individual tools:

```powershell
arun mcp add fixture node tests/fixtures/mcp.mjs
arun run "Use the echo tool" --allow-mcp fixture:echo
```

MCP annotations and descriptions do not grant permissions or retry safety. **MCP server processes themselves are currently trusted local code, not OS-sandboxed.** Only register servers you trust; stronger per-server filesystem/network isolation remains required for untrusted servers.

## Validation

```powershell
cargo test
cargo test --test docker -- --ignored
```

The ignored Docker test requires a running daemon and the local `node:22-alpine` image. It verifies a multi-megabyte output handle and that the runtime database is hidden in the container. The runtime contracts, crash cases, and evaluation design are in `docs/contracts.md`.

## Paired evaluation

```powershell
arun eval --prepare-only
arun eval --provider chatgpt --sizes 50 --modes eager,lazy,artifact,durable --tasks read,log
arun eval --provider chatgpt --repeats 3 --image node:22-alpine
```

The default matrix has 48 cases: three fixtures, four modes, and registries of 50, 100, 250, and 500 tools. Each repeat rotates mode order. Fixtures, exact manifests, immutable run configurations, CLI versions, JSONL events, artifacts, and results are saved under `.arun/evaluations/<id>/`. `--prepare-only` creates cases without model calls. Other runs consume the selected provider's usage allowance. Budgets include `--actions`, `--model-tokens`, `--context-chars`, and `--wall-seconds`; queued cases start their execution clock on first dispatch rather than at preparation.

Read and large-log fixtures require both the expected final answer and matching successful-operation evidence. Repair is checked independently by Node assertions in a network-disabled Docker container; the selected image must already exist. Results distinguish execution time from acceptance-check time, record wrong-tool and invalid-argument counts, and report context overflow explicitly. Schema exposure is measured in UTF-8 bytes, not claimed as exact tokenizer tokens. Models currently use provider defaults; version metadata records that limitation. Raw observations are not success-rate claims or uncertainty estimates.

## Remaining work

The broader `plan.txt` also calls for forced-restart benchmarks, pinned model configurations, paired uncertainty reporting, full MCP server isolation, and configurable external acceptance for ordinary runs. These are not yet claimed as implemented. The Codex CLI adapter disables its built-in tools, but its own system context still incurs substantial token overhead; measured usage is reported rather than presented as a kernel-only schema cost.
