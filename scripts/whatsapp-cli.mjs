import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { realpath } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { platformTarget } from './platform.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const actions = new Set(['init', 'start', 'stop', 'status', 'create-instance', 'webhook', 'qr', 'self-account', 'pair', 'daemon']);

export function parseWhatsAppArgs(args) {
  const action = args[0] ?? 'help';
  if (['help', '--help', '-h'].includes(action)) return { action: 'help' };
  if (!actions.has(action)) throw new Error(`Unknown WhatsApp action: ${action}. Run aegis whatsapp help.`);
  let workspace = process.cwd();
  for (let index = 1; index < args.length; index++) {
    if (args[index] !== '--workspace' || !args[index + 1]) throw new Error('Usage: aegis whatsapp <action> [--workspace <directory>]');
    workspace = args[++index];
  }
  return { action, workspace };
}

export async function whatsappCommand(args, executable) {
  const options = parseWhatsAppArgs(args);
  if (options.action === 'help') {
    console.log(`Aegis WhatsApp for Windows (Docker Desktop required)

  aegis whatsapp <action> [--workspace <directory>]

  init              Create private local settings and TLS credentials
  start             Start the persistent gateway and relay
  create-instance   Create the WhatsApp linked-device instance
  qr                Save the QR image to scan from WhatsApp Linked devices
  self-account      After linking, restrict commands to your own account
  pair              Create a one-time Aegis pairing code
  daemon            Start Aegis task control for this workspace
  status            Show local service and linked-device status
  stop              Stop this workspace's local services
  webhook           Reapply the gateway webhook settings

Use self-account after scanning the QR, then send the pairing code in your
WhatsApp self-chat. Task groups are requested automatically; group creation
using only your own number still needs verification with the linked account.
Send /help to discover commands or /goal <task> to start work.`);
    return 0;
  }
  if (process.platform !== 'win32') throw new Error('The local WhatsApp controller requires Windows and Docker Desktop.');
  const workspace = await realpath(options.workspace);
  const script = path.join(root, 'relay', 'local', 'local-remote.ps1');
  if (!existsSync(script)) throw new Error('This installation is missing the WhatsApp controller. Reinstall the current Aegis package.');
  const powershell = path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe');
  const command = ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', script,
    '-Action', options.action, '-Workspace', workspace, '-AegisBinary', executable];
  const relay = path.join(root, 'vendor', platformTarget().target, 'aegis-relay.exe');
  if (existsSync(relay)) command.push('-RelayBinary', relay);
  const child = spawn(powershell, command, { stdio: 'inherit', windowsHide: true });
  const stop = signal => { if (!child.killed) child.kill(signal); };
  const onInterrupt = () => stop('SIGINT');
  const onTerminate = () => stop('SIGTERM');
  process.on('SIGINT', onInterrupt);
  process.on('SIGTERM', onTerminate);
  try {
    return await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('close', (code, signal) => resolve(signal ? 1 : code ?? 1));
    });
  } finally {
    process.removeListener('SIGINT', onInterrupt);
    process.removeListener('SIGTERM', onTerminate);
  }
}
