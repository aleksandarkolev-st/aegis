# Aegis 0.2.3

`0.2.2` → `0.2.3`. Documentation and licensing. **No runtime behaviour change.**

## MIT licence

The project was published as `UNLICENSED` with no licence text. It is now MIT.

- `LICENSE` carries the MIT text and a short retroactive note: the grant covers
  every earlier version, including `0.1.0`, `0.2.0`, and `0.2.2`, which were
  published before the licence was chosen. npm does not allow a published
  version's licence to be changed, so those versions still display `UNLICENSED`
  on the registry even though the grant applies to them.
- `package.json`, `package-lock.json`, `Cargo.toml`, and `relay/Cargo.toml` all
  declare `MIT`.
- `LICENSE` is listed in `files[]`. npm only auto-includes `package.json`,
  `README`, and the main entry point, so an unlisted licence file would be
  dropped from the tarball.

## Contributing

- `CONTRIBUTING.md` covers prerequisites, the checks CI runs, and the bar for
  changes that touch permissions, completion evidence, credential storage, or
  the relay. It states plainly that passing tests do not establish interactive
  rendering or live provider behaviour, and asks contributors to say what they
  could not verify.
- `.github/ISSUE_TEMPLATE/bug_report.yml` asks for platform, version, provider
  and route, and warns against posting credentials.
- `.github/PULL_REQUEST_TEMPLATE.md` asks for how a change was verified and for a
  risk and rollback note on safety-relevant areas, with a checklist that
  includes not weakening assertions to get a test passing.

## Verification

`cargo test --all-targets` and `npm test` pass. The tarball contains
`package/LICENSE`, and the packaged manifest reports version `0.2.3` with
`license: MIT`.

## Known issues

Tracked as issues, with the evidence and the ruled-out theories:

- [#1 - `oauth::sign_out_waits_for_refresh` fails on Windows with OS error 5](https://github.com/aleksandarkolev-st/aegis/issues/1)
- [#2 - `endpoint::custom_endpoint_completes_a_kernel_run_without_persisting_its_key` is flaky on loaded runners](https://github.com/aleksandarkolev-st/aegis/issues/2)
