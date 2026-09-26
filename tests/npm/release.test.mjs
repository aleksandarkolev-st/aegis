import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { releaseTargets } from '../../scripts/platform.mjs';
import { stageNative, verifyVersion, writeChecksums } from '../../scripts/release.mjs';

test('release checksums require all five native platforms', async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'aegis-release-'));
  try {
    await assert.rejects(writeChecksums(directory), /ENOENT/);
    for (const info of releaseTargets()) await writeFile(path.join(directory, info.asset), info.target);
    await writeChecksums(directory);
    const manifest = await readFile(path.join(directory, 'SHA256SUMS'), 'utf8');
    assert.equal(manifest.trim().split('\n').length, 5);
    for (const info of releaseTargets()) {
      const hash = createHash('sha256').update(info.target).digest('hex');
      assert.ok(manifest.includes(hash + '  ' + info.asset + '\n'));
    }
    await assert.rejects(stageNative(directory, '../untrusted', directory), /Unsupported/);
  } finally {
    await rm(directory, { recursive: true, force: true });
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
