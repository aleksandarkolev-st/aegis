# Publishing Aegis on npm

The npm package is `aegis-arun`; it installs the `aegis` and `arun` commands. npm delivers the launcher, and its postinstall step downloads the current platform's native runtime from the matching GitHub release when the tarball does not already contain it. The download is checked against that release's `SHA256SUMS` file.

## Release the native binaries first

The GitHub release workflow supports Windows x64 and Linux x64 and arm64. macOS is unsupported. It checks that the tag, `package.json`, `package-lock.json`, and `Cargo.toml` all have the same version, then attaches the three binaries, checksums, and npm tarball to a GitHub release.

1. Choose the intended package license. `package.json` currently says `UNLICENSED`.
2. Check that the npm package name is still available and that the version has not been released. At the time of this check, `aegis-arun` returned 404 from npm.
3. Run the local checks:

   ```powershell
   npm run build:native
   npm test
   npm run test:package
   npm pack --dry-run --json
   ```

4. Commit the release changes and create a tag matching the manifests. For the current `0.1.0` manifests:

   ```powershell
   git tag v0.1.0
   git push origin v0.1.0
   ```

5. Wait for the **Native release** GitHub Actions workflow to finish successfully. Confirm the published GitHub release contains all three supported platform binaries and `SHA256SUMS` before publishing to npm. Publishing npm first would leave supported platforms unable to download their native runtime.

Manual workflow dispatch builds artifacts but does not create a GitHub release. A pushed `v*` tag runs the full release flow.

## Publish to npm

Use an npm account with publishing access. Direct npm publishing requires either account 2FA or a granular access token configured to bypass 2FA. The current package name is unscoped, so a successful publish is public; it does not need `--access public`. See npm's [unscoped package publishing guide](https://docs.npmjs.com/creating-and-publishing-unscoped-public-packages/) and [2FA publishing requirements](https://docs.npmjs.com/requiring-2fa-for-package-publishing-and-settings-modification/).

From the repository root, after the matching GitHub release is available:

```powershell
npm login
npm whoami
npm publish
```

Review the dry-run package file list first. `npm run test:package` checks an offline install, the installed `aegis --help` shim, and the current platform binary. `npm pack --dry-run --json` shows the package contents that npm will publish. npm also supports publishing a reviewed tarball directly with `npm publish <tarball>`.

Once the package is published, users can install and launch it with:

```powershell
npm install --global aegis-arun
aegis
```

The GitHub native release and the npm package use the same version. Each npm version can only be published once, so bump all four version sources together for later releases.
