# Contributing to Aegis

Thanks for taking the time. This guide covers what the project expects from a
change and how to verify one.

## Before you start

Aegis is a terminal agent that runs tools, reads and writes files, and talks to
model providers on your behalf. Most of the code is safety-relevant: permissions,
evidence for completion, credential storage, and the relay that accepts remote
commands. Changes there are held to a higher bar than ordinary bug fixes, and
the reasoning behind existing behaviour is usually recorded in `docs/` — read the
relevant document before changing a contract.

If you are unsure whether a change alters an intended guarantee, open an issue
and ask before writing code.

## Getting set up

You need Rust (edition 2024), Node 20 or newer, and a Windows, Linux x64, or
Linux arm64 machine. macOS is not a supported runtime.

```bash
git clone https://github.com/aleksandarkolev-st/aegis.git
cd aegis
cargo build --release
npm test
```

`npm install` runs a `postinstall` step that downloads the native runtime for
your platform from the matching GitHub release. Use `npm test` for the Node-side
tests and `cargo test` for everything else.

## Verifying a change

Run the checks that match what you touched:

```bash
cargo test                                     # Rust unit and integration tests
cargo test --all-targets                       # what CI runs before a release
npm test                                       # launcher, packaging, release tooling
npm run build:native && npm run test:package   # package install and shim check
```

`cargo test --locked` is what the release workflow uses. If you change
`Cargo.toml`, run `cargo update --package arun --offline` and commit
`Cargo.lock` alongside it, or the release build fails.

Some tests are `#[ignore]`d because they need a running Docker daemon or the
local `node:22-alpine` image. Run them deliberately when your change touches
process execution or filesystem scopes:

```bash
cargo test --test docker -- --ignored
```

### What the tests do and do not establish

Passing tests are a floor, not proof. The suite does not establish live model
inference, hosted account sign-in, or interactive terminal quality — those
require real credentials and a real console, and are recorded separately in
[verification notes](docs/verification.md). If your change affects interactive
rendering or input, say what you actually observed and on which platform. Do not
describe a unit test as evidence of interactive behaviour.

## Coding conventions

- Match the surrounding code. Run `cargo fmt` and `cargo clippy` before pushing.
- Tests live in `tests/` for integration behaviour and beside the code in `#[cfg(test)]` modules for units. Existing tests carry explanatory comments about *why* an assertion holds; keep that style rather than restating the code.
- Error messages are user-facing and are asserted on. Keep them specific and actionable.
- Do not weaken an assertion to make a test pass. If a test looks wrong, change
  the behaviour deliberately and explain why, rather than widening a tolerance or
  adding an ignore.

## Commits and pull requests

- One logical change per commit. Write messages in the imperative mood and
  explain the reasoning in the body, especially for anything touching
  permissions, credentials, or completion evidence.
- Reference the issue a pull request closes.
- Describe how you verified the change, including what you could not verify.

## Reporting bugs

Include the platform, the Aegis version, the provider and route in use, and the
relevant trace. `docs/control-surface.md` and `docs/terminal-ui.md` describe
where that information lives. Scrub API keys, tokens, and private endpoint URLs
before posting anything.

## Security

Do not open a public issue for a vulnerability. Report it privately to the
maintainer first, and give us a reasonable window to ship a fix before disclosing.

## Licence

Contributions are accepted under the [MIT licence](LICENSE). By opening a pull
request you confirm you have the right to license your work under it.