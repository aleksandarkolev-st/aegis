import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { releaseTargets } from '../../scripts/platform.mjs';
import { bundleNative, releaseAssets, stageNative, verifyVersion, writeChecksums } from '../../scripts/release.mjs';

test('release checksums require all native runtimes and the Windows relay', async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'aegis-release-'));
  try {
    await assert.rejects(writeChecksums(directory), /ENOENT/);
    for (const info of releaseTargets()) await writeFile(path.join(directory, info.asset), info.target);
    const relayAsset = 'aegis-relay-x86_64-pc-windows-msvc.exe';
    await writeFile(path.join(directory, relayAsset), 'windows-relay');
    await writeChecksums(directory);
    const manifest = await readFile(path.join(directory, 'SHA256SUMS'), 'utf8');
    assert.equal(manifest.trim().split('\n').length, 4);
    for (const info of releaseTargets()) {
      const hash = createHash('sha256').update(info.target).digest('hex');
      assert.ok(manifest.includes(hash + '  ' + info.asset + '\n'));
    }
    const relayHash = createHash('sha256').update('windows-relay').digest('hex');
    assert.ok(manifest.includes(relayHash + '  ' + relayAsset + '\n'));
    await assert.rejects(stageNative(directory, '../untrusted', directory), /Unsupported/);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test('release stages and bundles every runtime plus the Windows relay', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'aegis-bundle-'));
  const artifacts = path.join(root, 'artifacts');
  try {
    for (const info of releaseTargets()) {
      const source = path.join(root, 'target', info.target, 'release', info.executable);
      await mkdir(path.dirname(source), { recursive: true });
      await writeFile(source, 'runtime:' + info.target);
    }
    const relay = path.join(root, 'relay', 'target', 'x86_64-pc-windows-msvc', 'release', 'aegis-relay.exe');
    await mkdir(path.dirname(relay), { recursive: true });
    await writeFile(relay, 'windows-relay');

    for (const info of releaseTargets()) await stageNative(root, info.target, artifacts);
    assert.deepEqual(releaseAssets(), [
      'aegis-relay-x86_64-pc-windows-msvc.exe',
      ...releaseTargets().map(info => info.asset),
    ].sort());
    await writeChecksums(artifacts);
    assert.equal((await readFile(path.join(artifacts, 'SHA256SUMS'), 'utf8')).trim().split('\n').length, 4);

    await bundleNative(root, artifacts);
    for (const info of releaseTargets()) {
      assert.equal(await readFile(path.join(root, 'vendor', info.target, info.executable), 'utf8'), 'runtime:' + info.target);
    }
    assert.equal(await readFile(path.join(root, 'vendor', 'x86_64-pc-windows-msvc', 'aegis-relay.exe'), 'utf8'), 'windows-relay');
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('release version refuses a tag or manifest mismatch', async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'aegis-version-'));
  try {
    await writeFile(path.join(directory, 'package.json'), JSON.stringify({ version: '0.1.0' }));
    await writeFile(path.join(directory, 'package-lock.json'), JSON.stringify({ version: '0.1.0', packages: { '': { version: '0.1.0' } } }));
    await writeFile(path.join(directory, 'Cargo.toml'), '[package]\nname = "arun"\nversion = "0.1.0"\n');
    assert.equal(await verifyVersion(directory, 'v0.1.0'), '0.1.0');
    await assert.rejects(verifyVersion(directory, 'v0.2.0'), /must agree/);
    await writeFile(path.join(directory, 'Cargo.toml'), '[package]\nversion = "0.2.0"\n');
    await assert.rejects(verifyVersion(directory, 'v0.1.0'), /must agree/);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
