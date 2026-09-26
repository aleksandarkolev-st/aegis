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
const destination = binaryPath(root);
mkdirSync(path.dirname(destination), { recursive: true });
copyFileSync(path.join(root, 'target', 'release', info.executable), destination);
console.log('Bundled native runtime: ' + info.target);
