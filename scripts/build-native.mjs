import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { binaryPath, platformTarget } from './platform.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const info = platformTarget();
const localCargo = path.join(os.homedir(), '.cargo', 'bin', process.platform === 'win32' ? 'cargo.exe' : 'cargo');
const cargo = existsSync(localCargo) ? localCargo : 'cargo';
const build = spawnSync(cargo, ['build', '--release', '--locked'], { cwd: root, stdio: 'inherit' });
if (build.status !== 0) process.exit(build.status ?? 1);
if (process.platform === 'win32') {
  const relay = spawnSync(cargo, ['build', '--release', '--locked', '--manifest-path', 'relay/Cargo.toml', '--bin', 'aegis-relay'], { cwd: root, stdio: 'inherit' });
  if (relay.status !== 0) process.exit(relay.status ?? 1);
}
const destination = binaryPath(root);
mkdirSync(path.dirname(destination), { recursive: true });
copyFileSync(path.join(root, 'target', 'release', info.executable), destination);
if (process.platform === 'win32') {
  copyFileSync(path.join(root, 'relay', 'target', 'release', 'aegis-relay.exe'), path.join(path.dirname(destination), 'aegis-relay.exe'));
}
console.log('Bundled native runtime: ' + info.target);
