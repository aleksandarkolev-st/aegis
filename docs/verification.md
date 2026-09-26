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

- `cargo test --locked`: 35 unit tests and nine integration tests passed; the Docker test is ignored by default.
- `cargo test --test docker -- --ignored`: passed separately after starting Docker Desktop. It verifies multi-megabyte output virtualization and that the runtime database is hidden from the container.
- `npm test`: four package/platform/checksum/version tests passed.
- Process-tree tests verify that explicit kill and drop cleanup stop a descendant heartbeat, not merely its launcher.
- Setup fixtures verify install consent, refusal without installation, private provider paths, and image download consent.
- Recovery fixtures verify required receipts, no repeated write, and that reconciliation never revives a cancelled task.
- `git diff --check`: passed.

## Not yet verified or delivered

- Public npm publication and GitHub release assets.
- Actual Linux/macOS release builds; the checked-in release matrix still needs to run in CI.
- Live Claude completion after reauthentication.
- The remaining broader-plan work listed in the README: forced-restart comparisons, paired uncertainty reporting, full untrusted-MCP isolation, and configurable external acceptance for ordinary tasks.
