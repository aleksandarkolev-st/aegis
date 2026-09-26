# Aegis — Terminal Agent Runtime

`arun` is a Rust terminal agent with a durable SQLite event log, content-addressed artifacts, permission-filtered capability discovery, and out-of-process execution. The model returns one structured action at a time; the kernel owns state, grants, retries, context assembly, and evidence.

## Open Aegis

Launch `aegis` (or `arun`) with no arguments. Pick a provider and workspace access, then describe what you want to build. Native providers use your existing login and their default model—no authentication, model, budget or acceptance wizard to get through first. F4 signs in when needed, F6 changes models, and F7 opens optional settings. Custom endpoints need a URL/key/model and offer compatible response modes. The terminal handles task creation, execution, progress, and evidence automatically; you do not need to enter `run`, `attach`, or task IDs.

The scrollback interface has an animated activity line, elapsed time and token counts, editable input with history, and these shortcuts:

- F2: choose a provider without resetting workspace access.
- F3: select saved tasks, follow or resume them, cancel, inspect context/tools/evidence, or review interrupted outcomes.
- F4: open native sign-in or enter a custom endpoint key privately.
- F5: start a fresh conversation without deleting previous tasks.
- Ctrl+C once: interrupt the active operation. Press twice within 900 ms to interrupt the model turn. Ctrl+D detaches without stopping the task; cancel the entire task from its F3 menu.
- Ctrl+D at an empty prompt: exit. Up/Down: recall task input.

Set `NO_COLOR=1` to disable colors or `AEGIS_REDUCED_MOTION=1` to disable animation. Custom keys are held in session memory and passed to the model runner, not saved in the profile or forwarded to tool workers. The profile remembers only the key's environment-variable reference.

Sensible budgets and evidence checks are enabled by default. Optional F7 **Task budgets** settings offer Standard (four hours, 200 model turns, 800,000 model tokens and ten-minute commands), Quick (one hour and one-minute commands), or custom limits up to 24 hours per task and two hours per command. Limits are saved in the profile and copied into immutable task contracts. Provider usage allowances still apply; these limits are not price estimates. Advanced runs can set `--process-seconds` separately from `--wall-seconds`. A command deadline is always clamped to the task's remaining time.

Follow-up tasks carry bounded summaries of the previous four tasks, including after reopening Aegis or switching providers. Earlier summaries do not grant permissions or count as evidence for a new task. F5 (or optional `/new`) clears that continuation; task history remains available under F3.

Reopening an interactive terminal offers continuing your unfinished task. Interrupted non-idempotent calls remain paused: the recovery menu lets you select the operation and record an externally verified success or failure with a receipt, without entering operation IDs or replaying uncertain side effects. Evidence inspection uses bounded previews or text search, even for large stored logs.

### Model selection and terminal feedback

F6 opens a searchable model picker; F2 switches providers without resetting workspace permissions, budgets or saved tasks. The current provider and model are shown after startup and selection. Saved tasks retain their original provider/model; selections apply to new tasks. F7 opens focused settings for permissions, budgets or completion checks without signing in again. F8 shows the saved checkpoint and F9 asks before cancelling the entire task, including while following a live task. F1 explains the keyboard controls; slash commands are optional. Menus stay in terminal scrollback, with arrow navigation and text filtering. Animated task activity respects `AEGIS_REDUCED_MOTION`; `NO_COLOR` disables colors.

ChatGPT models come from the official Codex CLI's model metadata cache under `CODEX_HOME` (or `~/.codex`), excluding hidden entries. Claude choices use documented [Claude Code model aliases](https://code.claude.com/docs/en/model-config), resolved by the installed CLI rather than guessing versioned IDs. Grok uses only public ID/name/visibility fields from its model cache, falling back to a bounded native `grok models` command when no usable cache exists; unauthenticated fallback catalogs are labeled explicitly. Cache timestamps are displayed and do not guarantee current account access. Credentials and other cache fields are never copied into the picker or profile. Custom endpoints use an authenticated, five-second bounded `GET /models` request, without following redirects; manual IDs remain available when listing is unsupported.

Normal task feedback shows readable operations, saved evidence and short recovery hints instead of dumping provider JSON. Tool completion shows its path/program, measured bytes, exit code, elapsed time and evidence handle when available. A nonzero command exit is a warning, not a verified success. The live two-line activity area includes last-model context characters/schema count, durable operation/evidence counts and the last observed checkpoint age; characters are not mislabeled as tokens, and unknown measurements stay unknown. Raw events remain available in explicit replay/diagnostic views. Windows workers, model calls, MCP helpers and discovery probes do not allocate separate console windows; native login stays attached to the calling terminal (the provider may open a browser for authentication).

### Persistent project memory

**Workflow learning** uses a bounded SQLite experience journal, not growing Markdown files or a vector database. After an independently accepted task, Aegis records a short, versioned capability path, fixed coding-topic labels and acceptance evidence references. It copies no transcript, file contents, command arguments or free-form model advice into the learner. Two verified similar runs with the same verifier can suggest a path for a future task; current permissions and tool versions still filter it. At most two hints enter context, and none enter cold starts. Hints are historical suggestions, never authority or current-task evidence.

**Habit adaptation** observes repeated preferences in normal user requests without an extra model call. Two observations can teach a tentative preference such as pnpm, small changes, atomic commits, concise explanations, testing timing, or avoiding dependencies/comments. It uses eight fixed categories, not a copied conversation or unrestricted model-generated rules. Contradictory requests replace the candidate; current instructions always win. At most four relevant preferences enter a new task. Unrecognized habits can still be saved explicitly with “Remember: …”.

The journal keeps at most 128 experiences and eight habit candidates per workspace; automatic hints expire after 30 days. F7 → Project memory → Habit and workflow learning shows paths and inferred preferences, lets you confirm/change/forget a habit, pauses/enables learning, or confirms a reset of both. Explicit notes remain separate. Reset affects future tasks; immutable task snapshots and audit records remain. Tasks without independent completion checks do not train workflow paths, but their user-authored preferences can teach habits. Neither memory type grants permissions. This is a conservative first version, not general behavioral profiling; speed or token improvements require measurement.

Say **“Remember: use pnpm and preserve the lockfile”** at the prompt. Aegis saves it immediately without calling a model. F7 → **Project memory** lets you add, edit or forget notes; no setup is required. Memories survive restart and provider changes, are scoped to this workspace, and are frozen into new task contracts. Existing tasks keep their original snapshot when you edit or forget a note.

Memory stays deliberately small: at most 16 notes, 512 UTF-8 bytes each, 4 KiB of text total; duplicates are not added twice. Only explicit user notes are saved—tool output cannot silently become memory. Obvious credential formats are rejected; never store secrets. Notes provide context, not permission or completion evidence, and remembered technical facts must be verified against the current workspace. Forgetting affects future tasks, not old run records or SQLite backups. This bounded context is not an ever-growing chat transcript, and it is not a measured token-savings claim.

### Make it yours

Pip, Aegis's tiny shield sidekick, blinks while thinking and perks up when tools are working. The default look is ready immediately; appearance is never an onboarding step. F7 → **Appearance** switches between Mint/Pip, Midnight/Byte, Solar/Orbit or Calm (no mascot/motion). `NO_COLOR` and `AEGIS_REDUCED_MOTION` still take precedence. Rendering stays in normal terminal scrollback—no separate window or alternate screen.

Optional `.arun/ui.json` is a small, validated data file. Mix and reorder welcome `blocks` (`mascot`, `provider`, `workspace`, `hint`, `shortcuts`), set RGB `palette` values or turn `colors` off, choose `mascot`, change `input_prefix`, hide the context footer with `show_context`, and supply your own `portrait`, `frames`, `frame_millis` and `motion`. F7 can load a style from another file. Missing styles use defaults; invalid ones fall back without blocking startup. Files cannot execute scripts or embed terminal control sequences.

```json
{
  "palette": { "accent": [183, 157, 255] },
  "frames": ["<o.o>", "<o.->", "<^.^>"],
  "blocks": ["mascot", "workspace", "hint", "shortcuts"],
  "input_prefix": "build > ",
  "frame_millis": 500
}
```

For custom Rust rendering, the lightweight `arun::ui::Skin` trait provides the same building blocks: palette, portrait, phase-aware animation frames, frame interval, welcome layout, input prefix and context-footer visibility. Pass your implementation to `Terminal::with_skin`; `examples/custom_skin.rs` is a runnable, model-free preview. This is a composable library API, not an auto-executing plugin loader or an extra UI framework dependency. Appearance settings never change permissions, model configuration or task contracts.

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

Non-eager modes keep at most eight recently discovered capability schemas in the active working set. Rediscovery refreshes a schema's position; older schemas are deactivated transactionally and can be found again. Eviction is audited and recovered from snapshots without deleting operation identities, artifacts, or milestone evidence. Existing histories are also bounded when assembling model context. Eager baselines still expose all granted schemas, and oversized individual schemas fail the explicit context budget rather than being silently truncated.

Trace accounting distinguishes recorded tokens from unaccounted attempts, including interrupted or failed model calls with no usage receipt. Known failed-call elapsed time is retained. Paired reports exclude incomplete or estimated usage from complete-token comparisons, retain partial totals separately as `recorded_model_tokens`, and count excluded pairs. Discovery timing also marks older events without measurements as unavailable. No tokenizer-exact schema counts or billed price is inferred.

Model-token ceilings are enforced at turn boundaries, not as provider billing quotas: an in-flight native call may overshoot before its usage receipt arrives. Such a response is retained for audit, but its action is not applied and the task pauses before further work. Unreported usage remains explicitly unknown, not free.

### Exact command scopes

F7 → **Web access** optionally approves exact public HTTPS domains; it stays off by default and does not open container networking. Advanced usage: `--network-scopes reviewed.json` with `{"domains":["example.com"],"body_bytes":8388608}`. The read-only worker pins approved public DNS addresses, rejects private/special-use answers, credentials, non-443 ports, redirects, proxies and compressed bodies, and stores responses behind artifact handles. No cookies, login tokens or custom headers are forwarded.

Each response is at most 1 MiB and reserves capacity transactionally before fetching. A finished receipt charges its retained body length; interrupted/failed attempts conservatively keep their full reservation, including after restart. New attempts need new remaining budget. This bounds retained HTTP response bodies, not TLS/header/DNS overhead or native provider traffic; it is not an OS-level bandwidth quota. Scopes and capacity are frozen into each task. Network integration verification is still in progress; no agent benchmark has been started.

Advanced `--filesystem-scopes reviewed.json` narrows file access independently of command arguments. Use `{"read":["src/**","package.json"],"write":["src/**"]}`: exact files and directory subtrees only, with writable paths covered by read scopes. Empty lists deny host access; absent configuration keeps existing workspace permissions. Scopes are frozen into the task and checked before intent creation and again before a worker claims it. Search returns only permitted files.

Scoped commands bind only approved existing files/directories, with writes additionally requiring workspace-write permission. Other host files are not mounted; scratch files outside mounted paths stay ephemeral. Existing metadata directories are masked, links/junctions and ambiguous Windows filenames are rejected, and mount-tree inspection is bounded. Exact file mounts may prevent atomic rename; create new files inside an approved directory subtree instead. Narrow scopes reject MCP rather than silently granting whole-workspace or trusted-host access. Independently approved acceptance checks retain their separate read-only whole-workspace authority. F7 → **File access scopes** provides inline file/folder selection or reviewed JSON loading; removing restrictions requires confirmation. No extra setup is required for the default workspace mode.

F7 → **Exact command scopes** lets you approve commands inline: enter a program and one argument at a time, then save. No shell commands or JSON editing are needed. You can also load a reviewed JSON file. Existing container/program permissions still apply; scopes only narrow them. Removing argument restrictions requires confirmation and affects new tasks only.

Advanced usage: `arun run "Run the tests" --command-scopes commands.json --image rust:1.98`. A scopes file contains `{"commands":[{"program":"cargo","args":["test","--offline"]}]}`. Only that exact argument vector is allowed; extra flags, shell wrappers, changed order and combined arguments are rejected both before intent creation and by direct workers before claim. Empty `commands` denies every command. The reviewed file is copied into the immutable contract, not reread later. Do not put credentials in arguments: scopes are durable task data. For an empty-string argument, use the JSON file rather than the inline wizard's empty-input terminator.

These scopes do not constrain every filesystem access a permitted program can make within its workspace mount, or explicitly trusted-host MCP tools. Independent acceptance runs its own separately frozen read-only check.

### Provider response budgets

Native provider output is monitored against a per-turn `model_response_bytes` budget (8 MiB by default, configurable from 1 KiB to 32 MiB using `--model-response-bytes` or F7 custom budgets). Stdout, stderr and Codex's reply file share the allowance; oversized responses are stopped and rejected before parsing or executing actions. Reads are independently bounded, including after a fast natural exit. Monitoring can briefly overshoot in temporary files between polls; this is not an operating-system disk quota. Custom endpoint bodies honor the same configured allowance while streaming, including responses without Content-Length. Native CLI network transfer remains unobservable and is not reported as zero.

### Scoped interruption

Interrupting an operation does not cancel its durable task. An undispatched operation stops without executing; a claimed unsafe operation pauses with `outcome_unknown` until its external effects are reconciled. Read-only interrupted operations can stop without assuming a side effect was undone. A model-turn interrupt stops that provider call and pauses the task for resumption. Requests target an operation ID or a specific started-turn sequence, so an old request cannot interrupt a later resumed model turn. Work that already completed keeps its recorded result. Advanced automation can request `arun interrupt <run-id> operation|model`; terminal users use Ctrl+C and the task menu instead.

### Long-history recovery

Durable runs save a content-addressed recovery snapshot at startup and after roughly 256 committed events. A new runner verifies the task contract, replays only events after that snapshot, and checks the recovered state against transactional projections. Snapshots retain checkpoint references, milestones, activated capability versions, budgets/accounting, and unfinished operation identities; they never execute adapters.

Once hot history exceeds 1,024 events, older details move into synchronized, integrity-checked artifact archives. At least 64 events before the snapshot plus its subsequent tail stay hot for context and followers. Archive pointers and deletion of hot rows commit atomically; lifetime counters and event sequence numbers remain unchanged. `replay`, `trace`, and explicit historical followers still reconstruct the complete ordered audit, including archives. Cold-history corruption prevents a full audit but does not force normal recovery to load cold payloads; snapshot or tail corruption fails recovery instead of silently repeating work.

### Independent completion checks

Guided setup offers **Completion checks → Independent container check from a JSON file**. Aegis saves the validated check in the profile and snapshots it into each new task; the agent cannot replace or invoke the private verifier. Advanced usage accepts `--acceptance check.json`.

```json
{
  "name": "Addition returns the expected answer",
  "program": "node",
  "args": ["-e", "require('assert').equal(require('./math.cjs')(2,3),5)"],
  "image": "node:22-alpine",
  "seconds": 30
}
```

The image must already be installed locally. Verification runs with a read-only workspace, no network, and hidden runtime metadata. A nonzero exit rejects completion and returns bounded failure output to the agent for repair; unavailable verification pauses the task. Successful results are committed durably and reused after restart without repeating the check. Command arguments are frozen, but workspace test files referenced by them are not: use inline assertions or an independently controlled image for checks the agent must not weaken. Checks apply to every new task using that profile; reopen provider setup to change them.

## Capabilities and isolation

Read-only workspace search/read are granted by default. Add `--allow-write` to grant exact workspace writes. Paths cannot traverse out of the workspace or enter `.git` or `.arun` through the built-in file tools. `process.run` is disabled unless a specific program and a locally available Docker image are granted:

```powershell
arun run "Run the tests" --allow-process node --image node:22-alpine
```

The worker uses a read-only root filesystem, a bind-mounted workspace (read-only unless `--allow-write`), no container network, dropped capabilities, resource limits, and an ephemeral mount hiding `.arun`. Task execution never pulls images. Guided setup can download an image only after you explicitly choose it, or continue with commands disabled. Process output up to 32 MiB is stored as a separate artifact instead of being dumped into model context. Docker Desktop or an equivalent Docker daemon must be running for this capability.

Process workers and isolated MCP servers hide existing `.arun` and `.git` directories behind ephemeral mounts. Metadata files and links, including Git worktree `.git` files, are rejected rather than exposed. Missing metadata paths are not mounted or created, so read-only commands and acceptance checks also work in fresh workspaces whose runtime state lives elsewhere. These protections do not turn explicit trusted-host MCP execution into a sandbox.

Register an isolated stdio MCP server and grant individual tools. The server executable and script must be available inside the chosen image or workspace:

```powershell
arun mcp add fixture --image node:22-alpine -- node tests/fixtures/mcp.mjs
arun run "Use the echo tool" --allow-mcp fixture:echo
```

MCP annotations and descriptions do not grant permissions or retry safety. Untrusted servers execute inside network-disabled, resource-limited containers with a read-only root, hidden runtime/Git directories, no inherited provider credentials, and a read-only workspace. Optional registration `--allow-write` permits workspace writes only when the task also grants `workspace.write`; discovery always stays read-only. Images are never downloaded implicitly. Containers have operation-derived names and are removed on completion, timeout, or recovery; interrupted MCP effects still require reconciliation rather than blind retries.

Explicitly trusted local servers can opt out with `arun mcp add fixture --trusted-host -- node tests/fixtures/mcp.mjs`. This executes host code and is not a sandbox. Existing registrations without a stored policy must be registered again; they do not silently inherit trust. The benchmark's bundled fixture explicitly uses this trusted-host path. Isolated MCP currently rejects Git worktrees using a `.git` metadata file rather than exposing that file.

The MCP transport caps each input line at 32 MiB, registry data at 16 MiB/8,192 tools/100 pages, and rejects repeated pagination cursors. Server stderr is not displayed in the terminal. These bounds protect the host-side protocol reader in addition to the server container's resource limits.

## Validation

```powershell
cargo test
cargo test --test docker -- --ignored
```

The ignored Docker test requires a running daemon and the local `node:22-alpine` image. It verifies a multi-megabyte output handle and that the runtime database is hidden in the container. The runtime contracts, crash cases, and evaluation design are in `docs/contracts.md`.

See `docs/verification.md` for the dated local package/UI checks, live ChatGPT and Grok observations, and explicit verification gaps. It does not claim all providers or public releases have been certified.

## Paired evaluation

```powershell
arun eval --prepare-only
arun eval --provider chatgpt --sizes 50 --modes eager,lazy,artifact,durable --tasks read,log
arun eval --provider chatgpt --repeats 3 --image node:22-alpine
arun eval --provider chatgpt --tasks read --restart-at operation.succeeded
```

The default matrix has 48 cases: three fixtures, four modes, and registries of 50, 100, 250, and 500 tools. Each repeat rotates mode order. Fixtures, exact manifests, immutable run configurations, CLI versions, JSONL events, artifacts, and results are saved under `.arun/evaluations/<id>/`. `--prepare-only` creates cases without model calls. Other runs consume the selected provider's usage allowance. Budgets include `--actions`, `--model-tokens`, `--context-chars`, and `--wall-seconds`; queued cases start their execution clock on first dispatch rather than at preparation.

Read and large-log fixtures require both the expected final answer and matching successful-operation evidence. Repair is checked independently by Node assertions in a network-disabled Docker container; the selected image must already exist. Results distinguish execution time from acceptance-check time, record wrong-tool and invalid-argument counts, and report context overflow explicitly. Schema exposure is measured in UTF-8 bytes, not claimed as exact tokenizer tokens. Supply `--model` for pinned comparisons; otherwise metadata explicitly records a provider-default model. Raw observations are not success-rate claims or uncertainty estimates.

Use `--restart-at operation.executing`, `operation.succeeded`, or `checkpoint.created` to terminate the supervised runner process tree after observing that durable event and launch a fresh runner. Results record whether the boundary was reached, the operation's persisted state after termination, recovery latency, and whether reconciliation is required. Fast operations may finish before termination; the recorded state distinguishes this from an interrupted execution. Unsafe interrupted effects remain paused, rather than being retried just to improve benchmark acceptance. Non-durable modes fail after interruption. Budgets and provider usage still apply, and unreached boundaries are not represented as successful forced restarts.

Every completed evaluation writes `paired-summary.json`, matching candidate runs to eager runs by registry size, task, restart condition, and repeat. Acceptance differences include distribution-free 95% bounds; numeric differences use a deterministic paired bootstrap, with no interval for a single pair. Missing pairs and metrics are counted explicitly. Cost differences include failures and context overflow, so they are not automatically improvements. Regenerate a summary from saved observations with `arun eval-report <results.jsonl>`.

## Remaining work

The broader `plan.txt` still needs multi-hour live demonstrations. Forced-restart integration fixtures exercise a committed read across all four modes, checkpoint continuity through an archived history, and an unsafe in-flight MCP call; these are mock-provider correctness checks, not live comparative performance results. Live Claude completion remains pending reauthentication. The Codex CLI adapter disables its built-in tools, but its own system context still incurs substantial token overhead; measured usage is reported rather than presented as a kernel-only schema cost.
