# Aegis — Terminal Agent Runtime

`arun` is a Rust terminal agent with a durable SQLite event log, content-addressed artifacts, permission-filtered capability discovery, and out-of-process execution. The model returns one structured action at a time; the kernel owns state, grants, retries, context assembly, and evidence.

## Open Aegis

Launch `aegis` (or `arun`) with no arguments. Choose ChatGPT, Claude Code, Grok, or a custom OpenAI-compatible endpoint, choose your model and workspace permissions, and describe your task in plain language. Custom endpoints also offer schema, JSON-object, or prompt-only response modes. The terminal handles task creation, execution, progress, and evidence automatically; you do not need to enter `run`, `attach`, or task IDs.

The scrollback interface has an animated activity line, elapsed time and token counts, editable input with history, and these shortcuts:

- F2: choose a provider and permissions.
- F3: select saved tasks, follow or resume them, cancel, inspect context/tools/evidence, or review interrupted outcomes.
- F4: open native sign-in or enter a custom endpoint key privately.
- F5: start a fresh conversation without deleting previous tasks.
- Ctrl+C during execution: cancel the task. Ctrl+D during execution: detach without stopping it.
- Ctrl+D at an empty prompt: exit. Up/Down: recall task input.

Set `NO_COLOR=1` to disable colors or `AEGIS_REDUCED_MOTION=1` to disable animation. Custom keys are held in session memory and passed to the model runner, not saved in the profile or forwarded to tool workers. The profile remembers only the key's environment-variable reference.

Follow-up tasks carry bounded summaries of the previous four tasks, including after reopening Aegis or switching providers. Earlier summaries do not grant permissions or count as evidence for a new task. F5 (or optional `/new`) clears that continuation; task history remains available under F3.

Reopening an interactive terminal offers continuing your unfinished task. Interrupted non-idempotent calls remain paused: the recovery menu lets you select the operation and record an externally verified success or failure with a receipt, without entering operation IDs or replaying uncertain side effects. Evidence inspection uses bounded previews or text search, even for large stored logs.

### Node package

The package is named **`aegis-arun`**, with both `aegis` and `arun` commands. A local bundled installation can be built with:

```powershell
npm run build:native
npm pack
npm install -g ./aegis-arun-0.1.0.tgz
aegis
```

Building locally requires Node 20+ and Rust. Published packages download a platform-specific native runtime with SHA-256 verification; end users do not need Rust. Public `npm install -g aegis-arun` requires publishing the package and matching GitHub release assets first; publication is not yet claimed.

The native release workflow builds and tests Windows x64, Linux x64/arm64, and macOS Intel/Apple Silicon. A version tag matching both Rust and Node manifests creates the five assets, `SHA256SUMS`, and an npm tarball. Manual workflow dispatch only builds artifacts. npm publication is a separate authenticated step after the native release is available; no GitHub token is bundled into the package.

## Advanced CLI and source builds

```powershell
cargo build --release
target/release/arun login chatgpt
target/release/arun login claude
target/release/arun login grok
target/release/arun probe chatgpt
```

Each provider adapter launches its installed CLI (Codex for a ChatGPT login, Claude Code for a Claude login, Grok Build for a Grok login). Missing CLIs have an in-terminal installation confirmation: Aegis uses the official npm package in `~/.aegis/providers/`, without changing your global npm installation or workspace. `AEGIS_PROVIDER_HOME` overrides this location. Existing PATH installations take priority. This distribution uses the official [Codex package](https://github.com/openai/codex), [Claude Code npm installation](https://code.claude.com/docs/en/setup#install-with-npm), and [Grok npm distribution](https://docs.x.ai/build/enterprise#additional); provider requirements can exceed Aegis's Node 20 minimum, so Node 22+ is recommended for guided installation.

Credentials stay with those CLIs; arun does not read or copy their token stores. F4 opens native sign-in, and expired-login failures offer signing in and resuming inside the terminal. A provider may also refuse work when its account has no remaining usage balance.

Model and tool-worker subprocesses run in a Windows Job Object or POSIX process group. Cancellation, timeout, and runner cleanup terminate their descendants as well as the launcher; detached task runners are intentionally separate from the interactive terminal's lifetime.

`--model <id>` pins the model for a run or evaluation and persists its ID with the task contract. CLI prompts are supplied through stdin (Codex/Claude Code) or a prompt file (Grok), rather than large command-line arguments.

### Custom endpoints

OpenAI-compatible Chat Completions endpoints are a separate provider; they do not replace native ChatGPT, Claude Code, or Grok sign-in. Advanced CLI usage:

```powershell
arun run "Inspect this repository" --provider custom --endpoint http://127.0.0.1:1234/v1 --model local-model
arun run "Inspect this repository" --provider custom --endpoint https://example.com/v1 --model model-id --api-key-env CUSTOM_API_KEY
```

The key value is read from the named environment variable at request time and is not stored in run configuration. Unauthenticated local endpoints need no key. `--response-format schema|json|none` accommodates servers with different structured-output support. Base URLs or full `/chat/completions` URLs are accepted. Credentials in URLs, query strings, and fragments are rejected; HTTP is allowed on loopback, while remote HTTP requires explicit `--allow-insecure-endpoint`. Redirects are not followed, responses are capped at 8 MiB, and in-flight requests honor cancellation and timeout.

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

Read and large-log fixtures require both the expected final answer and matching successful-operation evidence. Repair is checked independently by Node assertions in a network-disabled Docker container; the selected image must already exist. Results distinguish execution time from acceptance-check time, record wrong-tool and invalid-argument counts, and report context overflow explicitly. Schema exposure is measured in UTF-8 bytes, not claimed as exact tokenizer tokens. Supply `--model` for pinned comparisons; otherwise metadata explicitly records a provider-default model. Raw observations are not success-rate claims or uncertainty estimates.

## Remaining work

The broader `plan.txt` also calls for forced-restart benchmarks, paired uncertainty reporting, full MCP server isolation, and configurable external acceptance for ordinary runs. These are not yet claimed as implemented. The Codex CLI adapter disables its built-in tools, but its own system context still incurs substantial token overhead; measured usage is reported rather than presented as a kernel-only schema cost.
