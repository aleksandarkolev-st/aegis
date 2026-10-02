#!/usr/bin/env node
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { tools } from './windows-host.mjs';

const argv = process.argv.slice(2), options = {};
try {
  for (let index = 0; index < argv.length; index++) {
    const flag = argv[index];
    if (flag === '--trusted-host') options.trusted = true;
    else if (['--workspace', '--aegis'].includes(flag) && argv[index + 1]) options[flag.slice(2)] = argv[++index];
    else throw new Error(`Unknown or incomplete option: ${flag}`);
  }
  if (!options.trusted) throw new Error('Requires --trusted-host: explicitly grants all listed Windows host tools to NEW workspace tasks.');
  if (process.platform !== 'win32') throw new Error('Windows host requires Windows');
  const workspace = await fs.realpath(path.resolve(options.workspace ?? '.'));
  if (!(await fs.stat(workspace)).isDirectory()) throw new Error('Workspace must be an existing directory');
  const profilePath = path.join(workspace, '.arun', 'profile.json');
  const readProfile = async () => fs.readFile(profilePath, 'utf8').catch(error => {
    if (error.code === 'ENOENT') return null;
    throw error;
  });
  const before = await readProfile();
  const profile = before === null ? { provider: 'chatgpt', model: null, endpoint: null, write: false, image: null }
    : JSON.parse(before.replace(/^\uFEFF/, ''));
  if (!profile || typeof profile !== 'object' || Array.isArray(profile) ||
      (profile.mcp_grants !== undefined && (!Array.isArray(profile.mcp_grants) || profile.mcp_grants.some(grant => typeof grant !== 'string')))) {
    throw new Error('Existing profile has invalid MCP grants; refusing to replace it');
  }
  const server = fileURLToPath(new URL('./windows-host.mjs', import.meta.url));
  const launcher = fileURLToPath(new URL('../bin/aegis.mjs', import.meta.url));
  const args = ['mcp', 'add', 'windows-host', '--trusted-host', process.execPath, server, '--trusted-host'];
  if (options.aegis && !path.isAbsolute(options.aegis)) throw new Error('--aegis must be an absolute native executable path');
  const executable = options.aegis ?? process.execPath;
  const nativeArgs = options.aegis ? args : [launcher, ...args];
  await new Promise((resolve, reject) => {
    const child = spawn(executable, nativeArgs, { cwd: workspace, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let stderr = '';
    child.stdout.on('data', chunk => process.stdout.write(chunk));
    child.stderr.on('data', chunk => { stderr = (stderr + chunk.toString('utf8')).slice(-8192); });
    const timer = setTimeout(() => { child.kill(); reject(new Error('Aegis MCP registration timed out')); }, 25000);
    child.on('error', error => { clearTimeout(timer); reject(error); });
    child.on('close', code => { clearTimeout(timer); code === 0 ? resolve() : reject(new Error(`MCP registration failed (${code}): ${stderr}`)); });
  });
  if (await readProfile() !== before) throw new Error('Profile changed during registration; registry saved but grants were not changed. Rerun after settings are saved.');
  profile.mcp_grants = [...new Set([...(profile.mcp_grants ?? []), ...tools.map(tool => `mcp:windows-host:${tool.name}`)])];
  await fs.mkdir(path.dirname(profilePath), { recursive: true });
  const temporary = `${profilePath}.windows-host-${randomUUID()}.tmp`;
  try {
    await fs.writeFile(temporary, `${JSON.stringify(profile, null, 2)}\n`, { flag: 'wx', mode: 0o600 });
    await fs.rename(temporary, profilePath);
  } finally { await fs.unlink(temporary).catch(() => {}); }
  console.log(`Registered ${tools.length} trusted Windows host tools for new tasks in ${workspace}. Existing task grants remain frozen.`);
} catch (error) {
  console.error(`windows-host: ${error.message}`);
  process.exitCode = 1;
}
