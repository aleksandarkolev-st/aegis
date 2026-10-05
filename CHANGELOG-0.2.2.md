# Aegis 0.2.2

`0.2.1` → `0.2.2`. **Release tooling plus three verified test and CI fixes. No intended runtime behavior change.**

## What changed

### npm publishing is now automatic
Tag pushes already built the native binaries and published the GitHub release. That flow now publishes to npm as well, so a green tag ships both.

The workflow uses **npm trusted publishing** (OIDC) instead of a stored npm token:

- `id-token: write` lets Actions mint the short-lived OIDC token npm authenticates with. No long-lived npm credential sits in repo secrets, and npm 2FA no longer blocks a publish — which is why `0.2.0` needed a manual one-time password.
- Node 24 with npm ≥ 11.5.1, the minimum trusted publishing requires.
- `package-manager-cache` disabled for release builds.
- `npm publish` runs **after** the GitHub release is made public, because `postinstall` downloads the native runtime from that release. Publishing to npm first would leave every supported platform unable to install.

**Requires one manual step on npmjs.com** before the first automated publish: a trusted publisher for `aegis-arun` pointing at `aleksandarkolev-st` / `aegis` / workflow `release.yml`, with `npm publish` allowed. Until then, publish from a machine with `npm login`.

### npm keywords
Added `cli`, `mcp`, `rust`, and `automation`, dropped `runtime` as redundant with `terminal`, and added `whatsapp`. `mcp` matters because the package ships MCP tooling; `whatsapp` because the relay and `whatsapp-cli.mjs` ship in the tarball.

### Three fixes for failures that blocked `0.2.0`

**Windows CI runner** — the `windows-2022` image's ConPTY corrupts the UTF-8 the native terminal emits. The two `tests/terminal_pty.rs` cases failed identically on two separate runs of the unchanged `v0.2.0` workflow, always with the same bytes:

```
app emitted  E6 97 A5 E6 9C AC E8 AA 9E   日 本 語
CI recorded  E6 97 A5 E6 9C AC E7 BF BF   日 本 翿
```

The emoji `F0 9F 99 82` was absent entirely, with no `U+FFFD` anywhere. Distinct codepoints collapsing onto one byte sequence is upstream corruption, not a width-tracking fault in this code — and both tests pass on real Windows. Pinned to `windows-2025`, where the corrupt bytes drop from 6 to 0.

**Run-lock contention in the legacy-adoption test** — the test locks `run-{id}.lock` to simulate a live worker, then calls `adopt_legacy_contract`, which locks that same path. `fs2` uses `flock` on Linux and `LockFileEx` on Windows, and both report contention when a second file description locks an already-locked path, so the test's own `try_lock_exclusive` could fail with a bare `EAGAIN`. It now retries until it owns the lock and asserts on the rejection message, so it proves production rejected the call instead of passing because it never took the lock.

**Degenerate terminal sizes** — `terminal::size()` does not fail when stdout is not a terminal; it succeeds and reports `1x1`. `TerminalSize::current()` and `measured_terminal_resize` trusted that, `saturating_sub(8)` produced width 0, and the composer silently disappeared so nothing was ever dispatched. Both paths now ignore implausible measurements and keep the last good size.

## Provenance

Unlike `0.2.0`, every binary in this release is built and tested by CI on all three supported platforms: Windows x64, Linux x64, and Linux arm64. macOS remains unsupported.

## Known issues

- `tests/endpoint.rs::custom_endpoint_completes_a_kernel_run_without_persisting_its_key` is flaky on loaded runners. The fixture writes all of its scripted stdin in a single write before the child reads any prompt, so prompt and answer can desynchronize. The app itself is not implicated: running the real binary on Linux dispatches all scripted tasks with the key bound correctly.
- `src/oauth.rs::sign_out_waits_for_refresh_and_cannot_be_resurrected_by_its_completion` intermittently fails on Windows with `Could not atomically save Aegis sign-in (OS error 5)`. Sign-out and refresh both hold the same per-provider vault lock, so this is not a missing-lock bug; the mechanism is still unexplained and needs a local reproduction before any change to credential storage.