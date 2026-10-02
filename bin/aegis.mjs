#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { ensureNative } from '../scripts/install.mjs';
import { fileURLToPath } from 'node:url';
import { whatsappCommand } from '../scripts/whatsapp-cli.mjs';

async function run(executable, args) {
  const child = spawn(executable, args, { stdio: 'inherit', windowsHide: true });
  const handlers = new Map(['SIGINT', 'SIGTERM'].map(signal => [signal, () => { if (!child.killed) child.kill(signal); }]));
  for (const [signal, handler] of handlers) process.on(signal, handler);
  try {
    return await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('close', (code, signal) => resolve(signal ? 1 : code ?? 1));
    });
  } finally {
    for (const [signal, handler] of handlers) process.removeListener(signal, handler);
  }
}

try {
  const executable = process.env.ARUN_BINARY || await ensureNative();
  const args = process.argv.slice(2);
  if (args[0] === 'whatsapp') {
    process.exitCode = await whatsappCommand(args.slice(1), executable);
  } else if (args[0] === 'pc') {
    if (!args[1] || ['help', '--help', '-h'].includes(args[1])) {
      console.log('aegis pc enable --trusted-host [--workspace <directory>]\nGrants files, PowerShell and desktop tools to new tasks in that workspace.\naegis pc status');
    } else if (args[1] === 'status' && args.length === 2) {
      process.exitCode = await run(executable, ['mcp', 'list']);
    } else if (args[1] === 'enable') {
      process.exitCode = await run(process.execPath, [fileURLToPath(new URL('../scripts/windows-host-install.mjs', import.meta.url)), '--aegis', executable, ...args.slice(2)]);
    } else {
      throw new Error('Usage: aegis pc enable --trusted-host [--workspace <directory>] | status');
    }
  } else {
    process.exitCode = await run(executable, args);
  }
} catch (error) {
  console.error('aegis: ' + error.message);
  process.exitCode = 1;
}
