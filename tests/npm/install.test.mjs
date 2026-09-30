import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { ensureNative } from '../../scripts/install.mjs';
import { binaryPath, platformTarget } from '../../scripts/platform.mjs';

test('maps only supported platform architectures', () => {
  assert.equal(platformTarget('win32', 'x64').executable, 'arun.exe');
  assert.equal(platformTarget('linux', 'arm64').target, 'aarch64-unknown-linux-gnu');
  assert.throws(() => platformTarget('darwin', 'x64'), /Unsupported platform/);
  assert.throws(() => platformTarget('darwin', 'arm64'), /Unsupported platform/);
  assert.throws(() => platformTarget('unknown', 'x64'), /Unsupported platform/);
});

test('installs verified bytes and rejects checksum mismatches', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'aegis-npm-'));
  const bytes = Buffer.from('local-native-fixture');
  const asset = platformTarget('win32', 'x64').asset;
  let corrupt = false;
  const server = createServer((request, response) => {
    if (request.url.endsWith('/SHA256SUMS')) {
      response.end(createHash('sha256').update(bytes).digest('hex') + '  ' + asset + '\n');
    } else response.end(corrupt ? Buffer.from('wrong-bytes') : bytes);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    await writeFile(path.join(root, 'package.json'), JSON.stringify({ version: '0.1.0' }));
    const baseUrl = 'http://127.0.0.1:' + server.address().port;
    const output = await ensureNative({ packageRoot: root, baseUrl, platform: 'win32', architecture: 'x64' });
    assert.equal(output, binaryPath(root, 'win32', 'x64'));
    assert.deepEqual(await readFile(output), bytes);
    await rm(output);
    corrupt = true;
    await assert.rejects(ensureNative({ packageRoot: root, baseUrl, platform: 'win32', architecture: 'x64' }), /checksum/);
  } finally {
    await new Promise(resolve => server.close(resolve));
    await rm(root, { recursive: true, force: true });
  }
});
