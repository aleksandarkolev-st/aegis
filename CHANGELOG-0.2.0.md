# Aegis 0.2.0

`0.1.0` → `0.2.0` — 148 commits, 127 files changed, +41,757 / −1,156 lines.

`0.2.0` is the release where Aegis stopped being a single-machine terminal agent and became a runtime you can drive from your phone, while the native terminal, live-edit streaming, and long-task durability were reworked underneath it.

## Highlights

### Remote control over WhatsApp and the relay
The largest addition. A new `relay` service plus a client half in `src/remote/` turns a phone into a first-class control surface for tasks already running on your PC.

- New relay crate with NATS JetStream transport, Postgres-backed repositories, and a channel-neutral HTTP layer (`relay/`).
- Paired devices create profile-bound remote Aegis tasks, watch them, steer them, and answer their questions; slash commands are forwarded to the local agent.
- Evolution gateway support, including linked owner self-chat and task groups.
- Local-first revocation: pairing codes and WhatsApp bindings can be revoked from the machine that owns them, with scoped, installation-scoped remote permissions.
- Durable safety around the remote path: approvals must be exact for side effects, pairing codes expire after five minutes, sensitive URLs are hidden from approval prompts, credentials are redacted from result exports, and remote workspace reads are credential-protected.
- Reliability fixes for the paths that actually broke in practice — relay event delivery after disconnects, command retries across NATS failures, reconnecting the remote daemon, and hardened local NATS bootstrap (including generating local TLS certs without requiring host `openssl`).

### Windows host and WhatsApp tooling
- Explicitly trusted Windows file-shell and desktop tools, registered and listed for installed-PC status.
- Persistent WhatsApp linked-device setup and a bundled `whatsapp-cli` in the npm package.
- Windows console, threading, pipe, and filesystem APIs added to `windows-sys`; launched Windows apps now survive MCP shutdown.

### Live output and live edits
- Foreground task output streams while the runner works, and process output streams during operation dispatch.
- Live additions and removals stream during workspace edits, continuing through large diffs with bounded complete chunks.
- Exact code replacements are encoded without nested JSON strings, so patches apply as written.
- Full process output stays reachable through its saved artifact reference when the on-screen preview hits its display limit.

### Terminal UI and input
- Owned VT input reader preserves Windows terminal paste.
- Multiline prompt editing survives wrapping and terminal resize; the home panel reflows on resize.
- Live task steering with goal timing; slash command menu expanded and searchable; steering submission routes to the active task.
- Task steering and completion timing are shown, and displayed execution budgets match the durable pause clock.
- Structured operation output previews render readably, including code and tool output transcript blocks.

### Durability, obligations, and correctness
- Task turn and token caps removed; long-task context is bounded by durable memory and lossless recall instead.
- Bounded MCP result previews in context.
- Detects equivalent failure loops across changed retry arguments, and durable no-progress loops, without capping exploration.
- Finish evidence must be current: completed milestones require fresh evidence, claims are required before successful operation receipts, canonical operation lifecycle is enforced, and failed process receipts are rejected as completion evidence.
- Durable safe pauses are excluded from execution deadlines, and obligation edits are blocked/guarded while tasks start.
- Owner instructions and task-owner messages persist beyond delivery and event archival, and stay ahead of terminal completion.
- Capability intent is ranked ahead of repeated discovery filler, and structured file tools are discovered early.

### Verification work
Substantial effort went into proving behavior on real runtimes rather than in unit tests alone: real ConPTY interaction, installed Windows terminal acceptance, hosted foreground and live-edit streaming against the actual binary, relay gateway and metadata boundaries, phone approval lifecycle, remote task controls with frozen permissions, and a cross-process remote E2E harness. 16 of the commits are test-only, including relay flake fixes.

## Compatibility notes

- Same package name (`aegis-arun`) and same `aegis` / `arun` commands.
- macOS remains unsupported; Windows x64, Linux x64, and Linux arm64 are shipped.
- Version is synchronized across `package.json`, `package-lock.json`, and `Cargo.toml`.
- `postinstall` still downloads the platform's native runtime from the matching GitHub release and verifies it against that release's `SHA256SUMS`. Install `0.2.0` only after the `v0.2.0` release is visible.

## Known issues

- The `v0.2.0` **Native release** workflow failed its Test step on the `windows-2022` runner for two interactive-desktop tests (`actual_windows_follow_keeps_output_live_inside_steering_and_slash_picker` and `actual_windows_terminal_preserves_multiline_drafts_and_resizes_command_picker`), which need a real interactive desktop that the runner does not provide. The Linux x64 and Linux arm64 jobs passed all tests and built successfully. Because the release job depends on all native jobs, no release was created by CI.
  - Consequence for this release: the Linux x64 and Linux arm64 binaries are the exact artifacts built and tested by CI for this tag. The Windows binary was built locally from the same tagged commit (`93e8129`) and verified to report `aegis 0.2.0`; it was not covered by that CI run's test step.