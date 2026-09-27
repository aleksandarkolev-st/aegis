import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmod, mkdir, mkdtemp, readFile, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { binaryPath, platformTarget } from './platform.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const npmCli = process.env.npm_execpath;
assert.ok(npmCli && path.isAbsolute(npmCli), 'Run this check with npm run test:package');
const bundled = binaryPath(root);
assert.ok((await stat(bundled)).isFile(), 'Build the current native runtime before checking the package');
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const bundledHash = digest(await readFile(bundled));
assert.equal(digest(await readFile(path.join(root, 'target', 'release', platformTarget().executable))), bundledHash,
  'Bundled runtime differs from the local release build; run npm run build:native');
const outputRoot = path.join(root, '.arun');
await mkdir(outputRoot, { recursive: true });
const directory = await mkdtemp(path.join(outputRoot, 'package-smoke-'));
const home = path.join(directory, 'home');
const prefix = path.join(directory, 'install');
const workspace = path.join(directory, 'workspace');
const traps = path.join(directory, 'traps');
for (const folder of [home, prefix, workspace, traps]) await mkdir(folder);
for (const name of ['codex', 'grok', 'claude', 'npm']) {
  const filename = path.join(traps, name + (process.platform === 'win32' ? '.cmd' : ''));
  await writeFile(filename, process.platform === 'win32'
    ? '@echo NATIVE_PROVIDER_WAS_STARTED\r\n@echo bad > native-started.txt\r\n@exit /b 97\r\n'
    : '#!/bin/sh\nprintf NATIVE_PROVIDER_WAS_STARTED\nprintf bad > native-started.txt\nexit 97\n');
  await chmod(filename, 0o755);
}
const env = {
  HOME: home, USERPROFILE: home, CODEX_HOME: path.join(home, '.codex'),
  AEGIS_PROVIDER_HOME: path.join(home, 'managed'),
  PATH: [traps, path.dirname(process.execPath)].join(path.delimiter),
  NO_COLOR: '1', AEGIS_REDUCED_MOTION: '1',
  npm_config_cache: path.join(directory, 'npm-cache'),
};
for (const key of ['SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT', 'TEMP', 'TMP']) {
  if (process.env[key]) env[key] = process.env[key];
}

async function command(args, { cwd = directory, input = '', timeout = 120000 } = {}) {
  const child = spawn(process.execPath, args, {
    cwd, env, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'],
    signal: AbortSignal.timeout(timeout),
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
  assert.equal(code, 0, `Command failed: ${args.join(' ')}\n${stdout}\n${stderr}`);
  return { stdout, stderr };
}

const packed = await command([npmCli, 'pack', '--offline', '--json', '--pack-destination', directory], { cwd: root });
const [manifest] = JSON.parse(packed.stdout);
assert.equal(manifest.name, 'aegis-arun');
for (const entry of manifest.files) {
  assert.match(entry.path, /^(?:package\.json|README\.md|bin\/[^/]+|scripts\/(?:install|platform)\.mjs|vendor\/[^/]+\/arun(?:\.exe)?)$/,
    `Unexpected packaged file: ${entry.path}`);
}
assert.ok(manifest.files.some(entry => entry.path === path.relative(root, bundled).split(path.sep).join('/')));
const tarball = path.join(directory, manifest.filename);
const installed = await command([npmCli, 'install', '--offline', '--no-audit', '--no-fund', '--foreground-scripts', '--prefix', prefix, tarball]);
assert.match(installed.stdout, /node scripts\/install\.mjs/);
const installedRoot = path.join(prefix, 'node_modules', 'aegis-arun');
assert.equal(digest(await readFile(binaryPath(installedRoot))), bundledHash);
const help = await command([npmCli, 'exec', '--offline', '--prefix', prefix, '--', 'aegis', '--help'], { cwd: prefix });
assert.match(help.stdout, /launch with no arguments for guided terminal tasks/);
assert.match(help.stdout, /login\|probe <provider>/);
const launcher = path.join(installedRoot, 'bin', 'aegis.mjs');
const onboarding = await command([launcher], { cwd: workspace, input: '3\n2\n', timeout: 10000 });
assert.match(onboarding.stdout, /Choose your provider/);
assert.match(onboarding.stdout, /Custom OpenAI-compatible endpoint/);
const login = await command([launcher, 'login', 'grok'], { cwd: workspace, input: '2\n', timeout: 10000 });
assert.match(login.stdout, /no provider CLI required/);
const tasks = await command([launcher, 'list'], { cwd: workspace, timeout: 10000 });
assert.equal(tasks.stdout.trim(), '', 'Declining setup must not create a task');
for (const folder of [directory, prefix, workspace, root]) {
  assert.equal(await stat(path.join(folder, 'native-started.txt')).then(() => true).catch(() => false), false);
}
assert.equal(await stat(path.join(home, '.aegis', 'auth')).then(() => true).catch(() => false), false);
const receipt = {
  package: `${manifest.name}@${manifest.version}`, platform: platformTarget().target,
  binary_sha256: bundledHash, tarball_sha256: digest(await readFile(tarball)),
  checks: ['offline pack file allowlist', 'offline private install with postinstall',
    'installed binary equality', 'npm command shim help', 'same-terminal onboarding decline',
    'owned sign-in decline', 'no provider CLI execution', 'no owned credentials created',
    'no task created when setup is declined'],
  not_verified: ['global installation', 'public release', 'other platforms', 'live inference', 'visual terminal quality'],
};
await writeFile(path.join(directory, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n');
await writeFile(path.join(directory, 'terminal.txt'), onboarding.stdout + login.stdout);
console.log(`Private package checks passed. Receipt: ${path.join(directory, 'receipt.json')}`);
