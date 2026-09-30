# Aegis — Terminal Agent Runtime

`arun` is a Rust terminal agent with a durable SQLite event log, content-addressed artifacts, permission-filtered capability discovery, and out-of-process execution. The model returns one structured action at a time; the kernel owns state, grants, retries, context assembly, and evidence.

## Open Aegis

Launch `aegis` (or `arun`) with no arguments. Pick ChatGPT, Grok or a custom endpoint, sign in if needed, choose a model and workspace access, then describe what you want to build. On later workspaces, interactive setup can reuse your last direct account, model and reasoning selection; it still reviews workspace permissions separately. This small preference is saved in `~/.aegis/selection.json` without credentials, custom endpoint settings or grants. Current source uses Aegis-owned direct HTTP, not native agent CLIs. F4 manages sign-in, F6 changes models/reasoning, and F7 opens optional settings. Custom endpoints need a URL/model, optionally a key, and offer compatible response modes. The terminal handles task creation, execution, progress and evidence automatically; you do not need to enter `run`, `attach` or task IDs.

ChatGPT/Grok sign-in now offers browser approval with a protected local callback as the recommended desktop flow, plus an explicit device-code choice for remote/headless terminals. Both return to the same terminal without a provider CLI. Existing sign-ins are retained. Protocol, menu and package checks pass; fresh hosted approval of this browser flow is not yet claimed. See [browser sign-in boundaries](docs/browser-signin.md).

**Direct-provider transition:** owned remote model/reasoning discovery and reviewed saved-task continuation are implemented in source. The current Windows package passes offline private installation and command-shim checks; its same-terminal guided ChatGPT coding path completed one independently accepted task. This is not a hard-task or token-efficiency result, and owned Grok operation is not independently verified on the current install. Without a catalog, an advertised manual model ID is required. Claude subscription sign-in is pending, not silently delegated to Claude Code. See [current evidence and remaining work](docs/direct-providers.md).

The scrollback interface has an animated activity line, elapsed time and token counts, editable input with history, and these shortcuts:

- F2: choose a provider without resetting workspace access.
- F3: select saved tasks, follow or resume them, cancel, inspect context/tools/evidence, or review interrupted outcomes; resume privately asks for a missing custom endpoint key, and Follow offers sign-in for a paused task's active provider.
- F4: manage Aegis-owned sign-in or enter a custom endpoint key privately; an unfinished saved task can take a key bound to its own endpoint without replacing the key for new tasks.
- If an active task gets an authentication error, Aegis offers sign-in for that task's current provider or a replacement custom key, then retries the same saved task once; declining leaves it paused.
- F5: start a fresh conversation without deleting previous tasks.
- Ctrl+C once: interrupt the active operation. Press twice within 900 ms to interrupt the model turn. Ctrl+D detaches without stopping the task; cancel the entire task from its F3 menu.
- Ctrl+D at an empty prompt: exit. Up/Down: recall task input; Down past the latest request restores your unsent draft and cursor. Ctrl+U clears it.

Opening a keyboard menu keeps your unfinished prompt and cursor position in this terminal session. Returning to the same prompt restores it, even after entering other settings fields. Drafts are not saved to disk or restored into secret-key fields.

Set `NO_COLOR=1` to disable colors or `AEGIS_REDUCED_MOTION=1` to disable animation. Custom keys are held in session memory and passed to the model runner, not saved in the profile or forwarded to tool workers. The profile remembers only the key's environment-variable reference.

Sensible budgets and evidence checks are enabled by default. Optional F7 **Task budgets** settings offer Standard (four hours, 200 model turns, 800,000 model tokens and ten-minute commands), Quick (one hour and one-minute commands), or custom limits up to 24 hours per task and two hours per command. Limits are saved in the profile and copied into immutable task contracts. Provider usage allowances still apply; these limits are not price estimates. Advanced runs can set `--process-seconds` separately from `--wall-seconds`. A command deadline is always clamped to the task's remaining time.

Follow-up tasks carry a bounded latest-chat preview and up to two older relevant previews, including after reopening Aegis or switching providers. Earlier summaries do not grant permissions or count as evidence for a new task. F5 (or optional `/new`) clears that continuation; task history remains available under F3.

For a task with a `Requirements:` bullet list, Aegis saves each user-written item as a kernel-owned obligation, separate from the model's editable milestones. Interactive starts preview this contract for review. `/goal`, `/status`, `/why`, `/evidence O3`, `/verify`, `/provider history`, `/budget` and `/handoff` inspect the selected task without inference. `/goal add` and `/goal replace O2` review confirmed requirement changes while retaining the original task and predecessors. `/pause` stops inference at a safe action boundary with a durable checkpoint; `/resume` continues the same run. Press `/` while following a task to enter controls. A successful current-revision operation artifact is required for each explicit obligation, and completion rechecks its integrity and provenance. Observed external edits and edits from peer tasks stale earlier proofs. Semantic coverage still needs independent acceptance; arbitrary prose is retained but not decomposed. See [task controls and proof boundaries](docs/control-surface.md).

F7 → Automatic provider fallback optionally approves an ordered list of other ChatGPT, Grok, or OpenAI-compatible endpoint/models for **new** tasks after live catalog checks. Add one fallback, then reopen F7 to add the remaining provider or swap their order; the current three-provider set permits two fallbacks. A direct fallback needs its own working Aegis sign-in; a custom fallback needs a reachable `GET /models` catalog and can be keyless or use a hidden API key entered in this terminal. The key is held in session memory and passed through the detached runner's process environment; only an environment-variable reference enters the profile and new-task contract. On restart, Aegis asks for the key again rather than storing it. On classified quota, provider outage, or HTTP 410 failures, Aegis records the failed attempt and advances through the reviewed list within the same run without resetting its permissions, budgets, evidence, or obligation ledger. Test failures, ordinary authentication errors, and malformed model replies do not trigger a switch. The approval screen names the destination and warns that bounded task/workspace context is shared there; no account credential is copied. This does not launch native CLIs. Claude subscription fallback is pending, and real hosted failover still needs end-to-end verification.

Reopening an interactive terminal offers continuing your unfinished task. Interrupted non-idempotent calls remain paused: the recovery menu lets you select the operation and record an externally verified success or failure with a receipt, without entering operation IDs or replaying uncertain side effects. Evidence inspection offers bounded previews, literal text search, line ranges and character ranges through menus, even for large stored logs. Browsing evidence makes no model or tool calls.

### Model selection and terminal feedback

F6 opens a searchable model picker; F2 switches providers without resetting workspace permissions, budgets or saved tasks. The current provider and model are shown after startup and selection. Saved tasks retain their original provider/model; selections apply to new tasks. F7 opens focused settings for permissions, budgets or completion checks without signing in again. F8 shows the saved checkpoint and F9 asks before cancelling the entire task, including while following a live task. F1 explains the keyboard controls; slash commands are optional. Menus stay in terminal scrollback, with arrow navigation and text filtering. Animated task activity respects `AEGIS_REDUCED_MOTION`; `NO_COLOR` disables colors.

Signed-in ChatGPT/Grok accounts use Aegis-owned remote model discovery and an account/session-bound cache, with advertised reasoning choices. Discovery is cancellable in this terminal. F6 opens a validated catalog immediately when it is less than 15 minutes old and offers **Refresh live model list**. Missing or expired catalogs trigger discovery automatically. If discovery fails, a saved list from the same account and compatible transport can be shown with its age and a warning. `aegis models chatgpt --refresh` forces a live lookup. Inference still validates account access. Choosing a model no longer repeats discovery just to show reasoning options. Without an owned session, read-only metadata from `CODEX_HOME` (or `~/.codex`) or Grok's cache can be explicitly labeled as external metadata; this is not an Aegis login or a guarantee of current account access. Hidden entries are excluded, no provider CLI starts, and external credentials are not copied into the picker or profile. An owned authentication failure never silently switches to external credentials. Claude currently reports pending rather than offering native CLI aliases. Custom endpoints use an authenticated, five-second bounded `GET /models` request, without following redirects; manual IDs remain available when listing is unsupported.

Normal task feedback shows readable operations, saved evidence and short recovery hints instead of dumping provider JSON. Tool completion shows its path/program, measured bytes, exit code, elapsed time and evidence handle when available. A nonzero command exit is a warning, not a verified success. The live two-line activity area includes last-model context characters/schema count, durable operation/evidence counts and the last observed checkpoint age; characters are not mislabeled as tokens, and unknown measurements stay unknown. Raw events remain available in explicit replay/diagnostic views. Windows workers and MCP helpers do not allocate separate console windows; direct model calls launch no provider process. Owned sign-in remains in this terminal with a cancellable animated status. Windows opens the validated browser authorization link; other platforms currently display a copyable link. Sign-in activity does not pretend to consume model tokens.

### Persistent project memory

**Workflow learning** uses a bounded SQLite experience journal, not growing Markdown files or a vector database. After an independently accepted task, Aegis records a short, versioned capability path, fixed coding-topic labels and acceptance evidence references. It copies no transcript, file contents, command arguments or free-form model advice into the learner. Two verified similar runs with the same verifier can suggest a path for a future task; current permissions and tool versions still filter it. At most two hints enter context, and none enter cold starts. Hints are historical suggestions, never authority or current-task evidence.

**Habit adaptation** observes repeated preferences in normal user requests without an extra model call. Two observations can teach a tentative preference such as pnpm, small changes, atomic commits, concise explanations, testing timing, or avoiding dependencies/comments. It uses seven fixed categories, not a copied conversation or unrestricted model-generated rules. Contradictory requests replace the candidate; current instructions always win. At most four relevant preferences enter a new task. Unrecognized habits can still be saved explicitly with “Remember: …”.

The journal keeps at most 128 experiences and eight habit candidates per workspace; automatic hints expire after 30 days. F7 → Project memory → Habit and workflow learning shows paths and inferred preferences, lets you confirm/change/forget a habit, pauses/enables learning, or confirms a reset of both. Explicit notes remain separate. Reset affects future tasks; immutable task snapshots and audit records remain. Tasks without independent completion checks do not train workflow paths, but their user-authored preferences can teach habits. Neither memory type grants permissions. This is a conservative first version, not general behavioral profiling; speed or token improvements require measurement.

Say **“Remember: use pnpm and preserve the lockfile”** at the prompt. Aegis saves it immediately without calling a model. F7 → **Project memory** lets you add, edit or forget notes; no setup is required. Memories survive restart and provider changes, are scoped to this workspace, and are frozen into new task contracts. Existing tasks keep their original snapshot when you edit or forget a note.

Memory stays deliberately small: at most 16 notes, 512 UTF-8 bytes each, 4 KiB of text total; duplicates are not added twice. Only explicit user notes are saved—tool output cannot silently become memory. Obvious credential formats are rejected; never store secrets. Notes provide context, not permission or completion evidence, and remembered technical facts must be verified against the current workspace. Forgetting affects future tasks, not old run records or SQLite backups. This bounded context is not an ever-growing chat transcript, and it is not a measured token-savings claim.

### Pinned project instructions

For coding reads, complete `workspace.read` results of at most 1024 Unicode characters appear in the next decision context, with their artifact and available content digest. Larger files stay artifact-backed without an automatic prefix dump. `workspace.read_batch` selects up to eight file ranges in one read-only operation, with 3000 Unicode characters total; those ranges also enter context directly. Both avoid requiring a separate inspection action for already-visible text. This is bounded runtime behavior, not a guarantee that every model chooses fewer actions or every task costs fewer tokens.

F7 → **Project instructions** adds, edits or removes explicit rules for this workspace, an exact relative file, or a `folder/**` subtree. No JSON file or model call is needed. The SQLite ledger keeps at most eight rules / 3 KiB, rejects oversized text instead of truncating it, and gives each edit a revision. New tasks freeze the complete ledger; recovery and checkpoints cannot rewrite it. Saved tasks show their original scoped revisions in Task details → Context.

Runtime safety and grants take precedence, then the current request, applicable pinned rules, reviewed repository guidance, project notes and learned preferences. Ambiguous same-scope prose conflicts are for the agent to clarify, not a semantic guarantee. Rules never grant file/command/network access. Removing a rule affects future tasks, not immutable saved contracts.

Root `AGENTS.md` / `CLAUDE.md` files are offered for review before a new task. Approve their full exact content once, or ignore that version; unchanged versions do not prompt again. F7 → Project instructions → **Review repository guidance** also handles nested files and removes saved reviews. Each source applies to its directory/subtree; deeper repository scopes take precedence. Changed approved content requires review before another task uses it. Saved tasks keep the original body, hash and revision outside recent-event summaries. No auxiliary summarizer or model call is needed.

Review storage is bounded to eight files / 16 KiB including path/scope text, at most 8 KiB per file. Oversize, credentials, unsafe controls, metadata paths and link traversal are rejected, never silently truncated. Narrow task read scopes or missing read permission exclude inaccessible guidance. This is reviewed file-content compatibility, not a parser for Claude frontmatter, every provider's rule format or automatic discovery of all nested/parent/home files. Select nested sources explicitly; rules stay in SQLite rather than an expanding memory document.

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

Maintainers can run `npm run test:package` after `npm run build:native` to check an offline private installation without changing the global command. The check retains a receipt and isolated workspace under `.arun/package-smoke-*`, verifies the packaged file allowlist and binary hash, runs the installed command shim, and declines onboarding/sign-in without creating tasks or starting provider CLIs. It deliberately verifies neither live inference nor interactive animation quality.

The native release workflow builds and tests Windows x64, Linux x64/arm64, and macOS Intel/Apple Silicon. A version tag matching both Rust and Node manifests creates the five assets, `SHA256SUMS`, and an npm tarball. Manual workflow dispatch only builds artifacts. npm publication is a separate authenticated step after the native release is available; no GitHub token is bundled into the package.

See [npm publishing](docs/npm-publishing.md) for the maintainer release sequence.

## Advanced CLI and source builds

```powershell
cargo build --release
target/release/arun login chatgpt
target/release/arun login grok
target/release/arun probe chatgpt
```

ChatGPT/Grok execution and login use direct, fixed provider endpoints. No provider CLI is installed or started and no native-CLI fallback is available. Hosted-service compatibility and account entitlement are not guaranteed by source-visible protocols; refusals are reported rather than bypassed. Claude subscription login reports pending at the user's request.

Aegis-owned credentials live in the user's `.aegis/auth` directory, separately from native caches: current-user DPAPI on Windows, owner-only plaintext 0600 files / 0700 directories on Unix. Refresh/save/sign-out use provider-specific locks. Uncertain rotating-token exchanges are not blindly replayed and can require fresh sign-in. F4 shows a verification link/code, waits for browser approval and supports Esc/Ctrl+C/Ctrl+D cancellation. Codes do not enter chat history or model context. The explicit read-only saved-native-session connection UI is still pending; native credential files are not automatically copied or modified.

Model and tool-worker subprocesses run in a Windows Job Object or POSIX process group. Cancellation, timeout, and runner cleanup terminate their descendants as well as the launcher; detached task runners are intentionally separate from the interactive terminal's lifetime.

`--model <id>` pins the model for a run or evaluation and persists its ID with the task contract. Direct prompts use bounded HTTP bodies, not subprocess arguments. New guided/advanced runs freeze `provider_transport: aegis-direct-v1`; legacy ChatGPT/Grok runs without that marker pause instead of silently changing immutable execution semantics. Saved chats remain inspectable; reviewed continuation into a new direct task still needs implementation.

### Custom endpoints

OpenAI-compatible Chat Completions endpoints are a separate provider; they do not substitute an API key for ChatGPT/Grok account sign-in. Advanced CLI usage:

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

Trace accounting distinguishes recorded tokens from unaccounted attempts, including interrupted or failed model calls with no usage receipt. Known failed-call elapsed time is retained. Paired reports exclude incomplete or estimated usage from complete-token comparisons, retain partial totals separately as `recorded_model_tokens`, and count excluded pairs. Discovery timing also marks older events without measurements as unavailable. No billed price is inferred.

New tasks freeze a separate tool-context budget of 800,000 `o200k_base` BPE units. F7 → Task budget → Custom limits or `--tool-result-tokens` changes it for future tasks. Before each provider request, the runtime counts the actual serialized active-schema array, whole raw prompt, and each included tool-related event using the pinned local tokenizer. Tool-event exposure is charged again when repeated in another request. Its durable reservation is committed with `model.started`; interrupted attempts retain the charge, and an oversized request pauses before contacting the provider. Snapshots and archived audits preserve those reservations. Older contracts without this field gain no retroactive limit, and older unmeasured attempts are marked unavailable.

These are exact counts for the declared normalized text encoding, not Claude/Grok/custom billing tokens, provider message framing, cached-token charges or universal model context sizes. Serialized tool-event metadata and inspection excerpts count too; the ledger measures exposed context, not the entire stored artifact. Provider-reported usage remains a separate metric.

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

For existing code, `workspace.patch` accepts up to 32 small exact replacements (64 KiB combined) instead of returning the entire file to write it back. Each old string must match exactly once; edits refer to disjoint ranges of the original file, not earlier replacements. Stale, missing, ambiguous or overlapping input fails before the worker claims an edit. `workspace.read` reports a content SHA256; supplying it as `expected_sha256` protects against edits based on an older inspected version. Replacement is atomic, preserves file permissions and shares the write grant and exact file scopes. A crash after claim still has an uncertain outcome and is never blindly replayed. External host edits racing the final replacement are not prevented by an OS file lock.

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

Read and large-log fixtures require both the expected final answer and matching successful-operation evidence. Repair is checked independently by Node assertions in a network-disabled Docker container; the selected image must already exist. Results distinguish execution time from acceptance-check time, record wrong-tool and invalid-argument counts, and report context overflow explicitly. New schema exposure is measured in UTF-8 bytes and declared `o200k_base` normalized token units, including repeated exposure; these are not universal provider billing tokens. Historical unmeasured requests are excluded from complete normalized-token comparisons. Supply `--model` for pinned comparisons; otherwise metadata explicitly records a provider-default model. Raw observations are not success-rate claims or uncertainty estimates.

Use `--restart-at operation.executing`, `operation.succeeded`, or `checkpoint.created` to terminate the supervised runner process tree after observing that durable event and launch a fresh runner. Results record whether the boundary was reached, the operation's persisted state after termination, recovery latency, and whether reconciliation is required. Fast operations may finish before termination; the recorded state distinguishes this from an interrupted execution. Unsafe interrupted effects remain paused, rather than being retried just to improve benchmark acceptance. Non-durable modes fail after interruption. Budgets and provider usage still apply, and unreached boundaries are not represented as successful forced restarts.

Every completed evaluation writes `paired-summary.json`, matching candidate runs to eager runs by registry size, task, restart condition, and repeat. Acceptance differences include distribution-free 95% bounds; numeric differences use a deterministic paired bootstrap, with no interval for a single pair. Missing pairs and metrics are counted explicitly. Cost differences include failures and context overflow, so they are not automatically improvements. Regenerate a summary from saved observations with `arun eval-report <results.jsonl>`.

## Remaining work

Optional developer runners for [native-Codex coding comparisons](benchmarks/coding/README.md) and [ARC-AGI-3 recordings](benchmarks/arc/README.md) default to preparation only. Their live modes require reviewed matching source/build hashes and completed implementation/functional/installed-UX verification. Independent graders and ARC protocol fixtures run without agents; no live ARC score or superiority over Codex is claimed. Current readiness gaps are recorded in `progress.txt`.

The broader `plan.txt` still needs a finalized multi-hour live demonstration. Historical forced-restart fixtures exercised committed reads/checkpoints/unsafe MCP outcomes; nested-CLI fixtures now need migration to independent transport before renewed full readiness. They are not live comparative performance results. Claude remains pending. Native bootstrap overhead motivated removing CLI execution rather than hiding/subtracting token counts. Direct saved-login protocol diagnostics returned genuine replies, but installed self-use and fresh owned login are still pending; no superiority claim is made.
### Reasoning controls

F6 selects a model and then its advertised reasoning effort in the same terminal. Owned ChatGPT/Grok accounts use direct discovery with a private account-bound cache and cancellable animated loading. Without an owned account, read-only external metadata is explicitly labeled as not an Aegis sign-in. No hard-coded availability claim or misleading CLI-default model option remains. Default reasoning leaves the setting unspecified. Custom endpoints can explicitly opt into `reasoning_effort`; compatibility is endpoint-dependent. The selection applies only to new turns, with saved task contracts unchanged. Advanced commands also accept `--reasoning <level>`; `/reasoning` opens just the effort picker.

Direct ChatGPT requests use `reasoning.effort`; direct Grok requests use `reasoning_effort`. They do not pass native CLI configuration flags. Provider support remains model/account dependent.
### Saved chats

F3 opens searchable saved conversations. Linked follow-up turns appear as one recent chat. Continue a chat to restore its conversation context without restarting old tools, or read its saved user/assistant messages (including earlier pages). Unfinished task recovery remains a separate explicit action. F5 starts a separate conversation; no startup modal blocks the composer.

F3's context view (or `/context`) separates recorded model input, output and reported cached-input tokens from local normalized prompt/schema/tool-result exposure. It also shows estimated and unaccounted model attempts; missing usage is not treated as free. Local prompt units are not provider billing tokens.

Older native-transport tasks offer **Continue work as new direct task** in F3. Choose the direct provider/model/reasoning and review the retained permissions, frozen guidance and fresh budget window before confirming. The original task is untouched; old operations and evidence are not copied. Cancelling sign-in leaves the confirmed new task saved and ready. Uncertain outcomes must be reviewed first. See [continuation behavior and checks](docs/saved-chat-continuation.md).

Long chats map the latest turn plus at most two older lexical matches into the model's context. Unrelated older chats stay available through search and saved-chat inspection rather than consuming every model request. Each request/reply preview is bounded and clipped previews are labeled. Full saved messages remain on disk; the existing inspection primitive can search the chat or retrieve a selected `chat:<run-id>` handle with ordinary slice/search controls. Retrieval is restricted to this run's same-workspace ancestor chain, never unrelated chats, and stops at a disclosed 512-turn scan horizon. F3's paged message browser remains available beyond that horizon. Automatic matching searches requests and saved reply previews, not every historic tool event; no semantic/perfect-recall claim is made. Explicit mapped retrieval is charged to the normalized tool-context budget; all assembled text is included in measured prompt units. History cannot become pinned instructions, permission or successful-operation evidence.
### Terminal interface

The home, composer and searchable selection lists use [Ratatui](https://ratatui.rs/)'s Rust widgets and responsive layout. Recent chats sit beside the mascot on wide terminals and stack on narrow ones. The composer shows the selected model, reasoning effort and file-access mode. Menus clean up their own rows instead of dumping complete option lists into scrollback. No alternate-screen takeover or separate application window is required; ordinary output and task history remain in your terminal.

`src/widgets.rs` exposes reusable home/composer/list building blocks. Existing data-only themes and Rust `Skin` customizations still supply palettes, mascot art/animation frames, block order and the prompt prefix. Reduced motion and `NO_COLOR` remain available.
