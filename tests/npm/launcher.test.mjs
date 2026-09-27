import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const launcher = fileURLToPath(new URL('../../bin/aegis.mjs', import.meta.url));

async function launch(binary, args, input = '') {
  const child = spawn(process.execPath, [launcher, ...args], {
    env: { ...process.env, ARUN_BINARY: binary },
    windowsHide: true,
    stdio: ['pipe', 'pipe', 'pipe'],
    signal: AbortSignal.timeout(10000),
  });
  let stdout = '';
  let stderr = '';
  child.stdout.setEncoding('utf8').on('data', chunk => { stdout += chunk; });
  child.stderr.setEncoding('utf8').on('data', chunk => { stderr += chunk; });
  child.stdin.end(input);
  const code = await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', resolve);
  });
  return { code, stdout, stderr };
}

test('launcher preserves terminal streams, literal arguments and native exit status', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'aegis-launcher-'));
  try {
    const fixture = path.join(root, 'native fixture.mjs');
    await writeFile(fixture, `
let input = '';
process.stdin.setEncoding('utf8');
for await (const chunk of process.stdin) input += chunk;
process.stdout.write(JSON.stringify({ args: process.argv.slice(2), input }));
process.stderr.write('native diagnostic');
process.exitCode = 37;
`);
    const args = ['model with spaces', '; not a shell command', 'unicode-λ'];
    const input = 'hello from the same terminal\nλ\n';
    const result = await launch(process.execPath, [fixture, ...args], input);
    assert.equal(result.code, 37);
    assert.deepEqual(JSON.parse(result.stdout), { args, input });
    assert.equal(result.stderr, 'native diagnostic');
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('missing native executable fails with a readable launcher error', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'aegis-launcher-'));
  try {
    const result = await launch(path.join(root, 'missing-native'), []);
    assert.equal(result.code, 1);
    assert.equal(result.stdout, '');
    assert.match(result.stderr, /^aegis: .*ENOENT/m);
    assert.doesNotMatch(result.stderr, /\n\s+at /);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
