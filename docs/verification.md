# Local verification — 2026-09-27

This is a development verification record, not a success-rate or performance-improvement claim.

## Terminal and package

- Built the release binary with `npm run build:native`.
- Packed `aegis-arun@0.1.0` and installed it offline into a workspace-local global prefix.
- Both installed `aegis` and `arun` commands launch without `ARUN_BINARY` overrides.
- Exercised the installed application in a real Windows terminal: editable natural-language input, animated progress, F3 task selection, evidence selection, text search, and Ctrl+D exit.
- A native ChatGPT/Codex-login task read `greeting.txt`, inspected its artifact, and completed with `AEGIS_TERMINAL_SMOKE_OK` and successful-operation evidence. It used four model turns, 49,502 provider-reported tokens, and 32 seconds of wall time. The provider-default model was not pinned.
- Local task ledger: `.arun/ui-smoke/.arun/`, run `2cb451a3-e1bc-40af-afb4-bd4f91793628`.
- Local Windows installation archive: `.arun/npm-smoke/aegis-arun-0.1.0.tgz`. This is not a public npm publication.
- Rebuilt and replaced the existing user-global npm installation after the UI changes. The installed launcher inherits terminal I/O and hides only newly allocated subprocess console windows. Native background helpers have a Windows no-console regression test.
- Exercised F2 provider selection, Claude alias filtering and Esc/back in the installed command's existing terminal. F6 shows visible Codex cached models; F7 opens focused settings without rerunning sign-in. A live Grok listing stalled, so model metadata cache support was added with an explicitly labeled timestamp and bounded command fallback. This is picker verification, not renewed Claude authentication.
- Rebuilt/reinstalled again after the Grok fallback fix, then confirmed the installed F2 flow displays its cached model immediately. Exercised F6 filtering and F1 help without entering shell commands; Ctrl+D returned to the same terminal. Installed native binary SHA256: `281a0a4756d13d5e54e334d9b0bbb1f669713fc460edf2e151b11a958fa23ba5`.
- Exercised the two-line live context/operation/evidence/checkpoint footer, F9 keep-running confirmation, and F8 checkpoint inspection in an installed Windows PTY using a delayed local decision fixture. Read results displayed measured bytes, elapsed time, and an evidence handle. This fixture is not Claude authentication evidence. Activity repaint was subsequently restricted to its two owned rows, preserving the surrounding viewport and scrollback.
- A fresh native ChatGPT login task, run `067d9612-9da8-4fcf-b7bd-d82b12899a81` under `.arun/ui-smoke/.arun/`, completed with pinned `gpt-5.5`, three model turns and 31,371 provider-reported tokens. It read the marker file without edits or process commands under the configured 8 MiB response capture budget; normal feedback showed the 24-byte result, elapsed time and evidence handle rather than JSON.

## Providers

- ChatGPT: the native login adapter completed the live terminal task described above.
- Grok: one independently checked read fixture passed using the existing native login and `grok 1.0.41 (4220f3b224a6) [stable]`. Registry size 50, durable mode, seven model turns, 148,658 provider-reported tokens, 102 seconds wall time, two fixture-defined wrong-tool choices, zero invalid arguments, and zero repeated dispatches. The provider-default model was not pinned; no paired comparison or reliability estimate follows from this single observation.
- Grok raw records: `.arun/evaluations/0e53d6b2-9bfa-479f-a010-d4571b3fdbe9/`, run `944e2af9-1e4c-4144-a7cc-bcc33e17049b`.
- Claude Code: the adapter and native sign-in path exist, but live completion remains unverified because the saved authentication expired. Browser reauthentication was intentionally left pending at the user's request.
- Custom endpoints: a local HTTP fixture completes advanced and guided runs with schema, JSON-object, and prompt-only response formats. It verifies authentication headers, continuation/reset behavior, and that a deliberately echoed session key is not stored or displayed. This does not certify an unspecified remote endpoint.

## Automated checks

- `cargo test --locked`: 71 unit tests and 29 integration tests passed; four Docker tests are ignored by default. Six focused terminal tests also passed after the final owned-row repaint change.
- `cargo test --test docker --test docker_acceptance --test docker_mcp -- --ignored`: all four passed separately with Docker running. They verify multi-megabyte output virtualization, hidden runtime state, fresh read-only workspaces without metadata directories, immutable/read-only acceptance checks, and MCP filesystem/network/credential isolation.
- `npm test`: four package/platform/checksum/version tests passed.
- Process-tree tests verify that explicit kill and drop cleanup stop a descendant heartbeat, not merely its launcher.
- A Windows regression forcibly kills a separate supervising process and verifies that its managed child's heartbeat stops without requiring Rust drop cleanup. The same ownership behavior passed a live native-login task interruption; orphan Docker containers are separately removed during runtime recovery.
- Guided setup tests verify Standard, Quick, and custom budgets, including a four-hour task preset and bounded two-hour commands. Capability tests evict and reactivate schemas across snapshots/restarts without deleting earlier operation evidence, and bound legacy working sets during context assembly.
- Accounting tests distinguish failed/missing-usage attempts from zero cost and exclude incomplete or estimated paired token totals. Historical discovery events without timing receipts are explicitly unaccounted.
- Setup fixtures verify install consent, refusal without installation, private provider paths, and image download consent.
- Recovery fixtures verify required receipts, no repeated write, and that reconciliation never revives a cancelled task.
- Forced-restart fixtures terminate actual supervised process trees: durable recovery consumes committed read evidence once; all three non-durable modes fail on interruption; an unsafe executing MCP call pauses with an unknown outcome and is not replayed. The decision provider is mocked in these fixtures.
- Paired-report tests cover repeat matching, missing/duplicate conditions, deterministic bootstrap intervals, conservative acceptance bounds, and refusal to pair different experiments. They do not constitute measured live comparisons.
- Guided and advanced acceptance setup fixtures verify frozen configurations and that the private verifier is not exposed as a model tool.
- A stalled HTTP fixture verifies that an active model request respects the remaining task wall-time budget.
- A natural-exit regression covers a Windows completion-port double-wait hang discovered during Docker testing and fixed before the successful rerun.
- Snapshot tests archive a 3,000-turn synthetic history, recover the original accounting and an executing write from fewer than 32 tail events, and reconstruct the complete ordered audit. Corrupt cold archives fail audit reads without entering normal recovery; corrupt snapshots fail recovery.
- The forced-read restart fixture now preserves a checkpoint across 1,100 archived detail events, fresh provider processes, and an actual runner restart. This is continuity evidence, not a multi-hour live demonstration.
- Scoped-interruption fixtures stop an actual in-flight HTTP model request without cancelling its task, resume a new turn without inheriting the old request, and interrupt an effectful MCP call only after its external file write. The latter remains unknown and is not replayed. A direct-adapter test rejects an interrupted write before claim. Keyboard timing is tested separately; this phase does not claim a new manual terminal-key demonstration.
- `git diff --check`: passed.
- Model-picker fixtures verify authenticated `/models` requests, hidden-model filtering, manual fallback, preservation of permissions/budgets/history, and removal of container command access when choosing review-only mode. Native error fixtures verify readable sign-in hints in both guided and advanced flows without dumping provider JSON; raw durable diagnostics remain available separately.
- Response capture tests reject oversized native stdout, stderr, their combined size, Codex reply files, and fast-exiting providers before applying any action. HTTP tests cover declared and chunked response bodies. Poll-based native capture can briefly overshoot between checks; it is not an operating-system disk quota.
- Checkpoint/cancellation fixtures preserve an uncertain write without replay, distinguish keep-running from confirmed cancellation, and verify that cancellation does not falsely reconcile unknown effects. Result metadata tests cover UTF-8 byte counts, exit status, search counts and elapsed time; footer counts are explicitly characters, not tokenizer-exact tokens.

## Not yet verified or delivered

- Public npm publication and GitHub release assets.
- Actual Linux/macOS release builds; the checked-in release matrix still needs to run in CI.
- Live Claude completion after reauthentication.
- A successful multi-hour paced endurance completion remains unverified: its final attempt exceeded the worker deadline and initially paused with an unknown outcome. A later host-verified failed receipt reconciled that operation; resume honored the expired wall budget without replay or another model call. No final acceptance passed. The 48-case pinned-model matrix and eight-case forced-read restart experiment completed; see [live observations](benchmarks.md) for failures and measured overhead rather than an improvement claim. A later repair pilot encountered the provider's usage limit, not a successful two-repeat comparison.
