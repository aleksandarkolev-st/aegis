# Local verification — 2026-09-26

This is a development verification record, not a success-rate or performance-improvement claim.

## Terminal and package

- Built the release binary with `npm run build:native`.
- Packed `aegis-arun@0.1.0` and installed it offline into a workspace-local global prefix.
- Both installed `aegis` and `arun` commands launch without `ARUN_BINARY` overrides.
- Exercised the installed application in a real Windows terminal: editable natural-language input, animated progress, F3 task selection, evidence selection, text search, and Ctrl+D exit.
- A native ChatGPT/Codex-login task read `greeting.txt`, inspected its artifact, and completed with `AEGIS_TERMINAL_SMOKE_OK` and successful-operation evidence. It used four model turns, 49,502 provider-reported tokens, and 32 seconds of wall time. The provider-default model was not pinned.
- Local task ledger: `.arun/ui-smoke/.arun/`, run `2cb451a3-e1bc-40af-afb4-bd4f91793628`.
- Local Windows installation archive: `.arun/npm-smoke/aegis-arun-0.1.0.tgz`. This is not a public npm publication.

## Providers

- ChatGPT: the native login adapter completed the live terminal task described above.
- Grok: one independently checked read fixture passed using the existing native login and `grok 1.0.41 (4220f3b224a6) [stable]`. Registry size 50, durable mode, seven model turns, 148,658 provider-reported tokens, 102 seconds wall time, two fixture-defined wrong-tool choices, zero invalid arguments, and zero repeated dispatches. The provider-default model was not pinned; no paired comparison or reliability estimate follows from this single observation.
- Grok raw records: `.arun/evaluations/0e53d6b2-9bfa-479f-a010-d4571b3fdbe9/`, run `944e2af9-1e4c-4144-a7cc-bcc33e17049b`.
- Claude Code: the adapter and native sign-in path exist, but live completion remains unverified because the saved authentication expired. Browser reauthentication was intentionally left pending at the user's request.
- Custom endpoints: a local HTTP fixture completes advanced and guided runs with schema, JSON-object, and prompt-only response formats. It verifies authentication headers, continuation/reset behavior, and that a deliberately echoed session key is not stored or displayed. This does not certify an unspecified remote endpoint.

## Automated checks

- `cargo test --locked`: 53 unit tests and 18 integration tests passed after the snapshot/archive and scoped-interruption changes; three Docker tests are ignored by default.
- `cargo test --test docker --test docker_acceptance --test docker_mcp -- --ignored`: all three passed separately with Docker running. They verify multi-megabyte output virtualization, hidden runtime state, immutable/read-only acceptance checks, and MCP filesystem/network/credential isolation.
- `npm test`: four package/platform/checksum/version tests passed.
- Process-tree tests verify that explicit kill and drop cleanup stop a descendant heartbeat, not merely its launcher.
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

## Not yet verified or delivered

- Public npm publication and GitHub release assets.
- Actual Linux/macOS release builds; the checked-in release matrix still needs to run in CI.
- Live Claude completion after reauthentication.
- Multi-hour live demonstrations and live paired/restart comparisons with pinned models.
