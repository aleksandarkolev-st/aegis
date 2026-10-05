# Aegis 0.2.1

`0.2.0` → `0.2.1` — release tooling only. **No runtime behavior changes.**

## What changed

### npm publishing is now automatic
Tag pushes already built the native binaries and published the GitHub release. That flow now publishes to npm as well, so a green tag ships both.

The workflow uses **npm trusted publishing** (OIDC) instead of a stored npm token:

- `id-token: write` lets Actions mint the short-lived OIDC token npm authenticates with. No long-lived npm credential lives in repo secrets, and npm 2FA no longer blocks a publish — which is exactly why `0.2.0` needed a manual one-time password.
- Node 24 with npm ≥ 11.5.1, the minimum trusted publishing requires.
- `package-manager-cache` disabled for release builds.
- `npm publish` runs **after** the GitHub release is made public, because `postinstall` downloads the native runtime from that release. Publishing to npm first would leave every supported platform unable to install.

**Requires one manual step on npmjs.com** before the first automated publish: a trusted publisher for `aegis-arun` pointing at `aleksandarkolev-st` / `aegis` / workflow `release.yml`, with `npm publish` allowed.

## The `0.2.0` CI failure, resolved

`0.2.0`'s **Native release** workflow failed on `windows-2022` with two ConPTY interactive-terminal tests, which prevented CI from creating the release. Root cause was found and is **not in this codebase**.

The failing tests (`tests/terminal_pty.rs` — new in `0.2.0`, never previously green in CI) pass on real Windows here: 4/4 across repeated runs, including under CPU saturation. The bytes captured from CI show the runner's ConPTY mangling UTF-8:

```
app emitted   E6 97 A5  E6 9C AC  E8 AA 9E    →  日 本 語
CI log shows  E6 97 A5  E6 9C AC  E7 BF BF    →  日 本 翿
```

`🙂` (`F0 9F 99 82`) is absent entirely, replaced by another `翿翿`, with **zero** `U+FFFD` replacement characters — so this is upstream byte corruption, not a decode or width-tracking fault in the app or the test's screen emulator. Distinct codepoints collapsing onto the same byte sequence is something `UnicodeWidthChar` cannot produce; it operates on already-decoded `char`s.

### Consequence for `0.2.0`
- Linux x64 and arm64 binaries are the exact artifacts CI built and tested for that tag.
- The Windows binary was built locally from the same tagged commit (`93e8129`) and verified to report `aegis 0.2.0`, but was **not** covered by that run's test step.

That gap is why `0.2.1` re-publishes through CI rather than inheriting `0.2.0`'s mixed-provenance Windows binary.

## Upgrade notes

- Same package (`aegis-arun`), same `aegis` / `arun` commands, same supported platforms (Windows x64, Linux x64, Linux arm64; no macOS).
- Safe drop-in replacement for `0.2.0` — no migration steps.