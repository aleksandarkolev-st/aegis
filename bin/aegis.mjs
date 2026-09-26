#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { ensureNative } from '../scripts/install.mjs';

try {
  const executable = process.env.ARUN_BINARY || await ensureNative();
  const child = spawn(executable, process.argv.slice(2), { stdio: 'inherit', windowsHide: false });
  child.on('error', error => {
    console.error('aegis: ' + error.message);
    process.exitCode = 1;
  });
  child.on('exit', (code, signal) => {
    if (signal) process.exitCode = 1;
    else process.exitCode = code ?? 1;
  });
  for (const signal of ['SIGINT', 'SIGTERM']) {
    process.on(signal, () => { if (!child.killed) child.kill(signal); });
  }
} catch (error) {
  console.error('aegis: ' + error.message);
  process.exitCode = 1;
}
