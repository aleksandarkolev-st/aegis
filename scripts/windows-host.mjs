#!/usr/bin/env node
// Explicitly trusted local MCP server. No dependency on another agent or CLI.
import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';

const helper = fileURLToPath(new URL('./windows-host.ps1', import.meta.url));
const MAX_MESSAGE = 2 * 1024 * 1024;
const string = (description, maxLength = 32768) => ({ type: 'string', description, maxLength, minLength: 1 });
const integer = (minimum, maximum, description) => ({ type: 'integer', minimum, maximum, description });
const schema = (properties, required = []) => ({ type: 'object', properties, required, additionalProperties: false });
const definition = (name, description, properties, required, readOnly = false) => ({
  name, description, inputSchema: schema(properties, required),
  annotations: { readOnlyHint: readOnly, destructiveHint: !readOnly, openWorldHint: true },
});
export const tools = [
  definition('file_read', 'Read a bounded UTF-8 excerpt from any regular host file. Absolute paths allowed under trusted-host authorization.', {
    path: string('File path; relative paths resolve from the MCP working directory.'),
    offset: integer(0, Number.MAX_SAFE_INTEGER, 'Byte offset, default 0.'),
    max_bytes: integer(1, 65536, 'Maximum bytes, default 32768.'),
  }, ['path'], true),
  definition('file_write', 'Write UTF-8 to a host file. Existing files require overwrite:true. Creates parent directories.', {
    path: string('File path.'), content: { type: 'string', maxLength: 1048576 }, overwrite: { type: 'boolean' },
  }, ['path', 'content']),
  definition('directory_list', 'List at most 200 directory entries, without recursion or following child links.', {
    path: string('Directory path; default current directory.'), limit: integer(1, 200, 'Default 100.'),
  }, [], true),
  definition('powershell', 'Run native Windows PowerShell on the trusted host. This is not a sandbox. Output is bounded; all child processes terminate with the invocation. Use app_launch for an app that must remain open.', {
    script: string('PowerShell script.', 65536), cwd: string('Working directory; default MCP directory.'),
    timeout_seconds: integer(1, 600, 'Execution limit including startup, default 60; Aegis also applies the task command deadline.'),
    max_output_bytes: integer(256, 131072, 'Bytes retained per stdout/stderr stream, default 32768.'),
  }, ['script']),
  definition('desktop_windows', 'List visible top-level Windows applications and foreground window, including handles, process IDs and screen rectangles.', {}, [], true),
  definition('desktop_focus', 'Restore and focus an existing desktop window by its handle. Windows may refuse foreground activation.', {
    handle: string('Window handle returned by desktop_windows, decimal integer string.', 32),
  }, ['handle']),
  definition('app_launch', 'Launch an explicitly specified executable without shell interpolation. Defaults to hidden; visible:true opens a user-visible interactive application.', {
    executable: string('Absolute executable path.'), args: { type: 'array', items: { type: 'string', maxLength: 32768 }, maxItems: 64 },
    cwd: string('Working directory.'), visible: { type: 'boolean' },
  }, ['executable']),
  definition('desktop_screenshot', 'Capture the Windows desktop or an absolute screen region as a bounded PNG image and saved local file. Return metadata maps scaled image coordinates to original screen pixels.', {
    x: integer(-32768, 32768, 'Region origin X; all x/y/width/height must be supplied together.'),
    y: integer(-32768, 32768, 'Region origin Y.'), width: integer(1, 16384, 'Region width.'), height: integer(1, 16384, 'Region height.'),
    max_width: integer(64, 1920, 'Scaled output maximum width, default 1280.'),
    max_height: integer(64, 1080, 'Scaled output maximum height, default 1024.'),
    output_path: string('PNG destination; defaults to .arun/windows-host/capture-<id>.png. Never overwrites a file.'),
  }, [], true),
  definition('desktop_mouse', 'Move/click the cursor or scroll at absolute screen coordinates. Coordinates refer to original pixels, not scaled screenshots.', {
    action: { type: 'string', enum: ['move', 'click', 'scroll'] },
    x: integer(-32768, 32768, 'Screen X.'), y: integer(-32768, 32768, 'Screen Y.'),
    button: { type: 'string', enum: ['left', 'right', 'middle'] },
    count: integer(1, 2, 'Click count, default 1.'), delta: integer(-12000, 12000, 'Vertical scroll wheel delta; 120 is one notch.'),
  }, ['action', 'x', 'y']),
  definition('desktop_keyboard', 'Type Unicode text or press one simultaneous key chord in the focused app. Key names include CTRL, SHIFT, ALT, WIN, ENTER, TAB, ESC, arrows, HOME, END, DELETE, F1-F24 and ASCII letters/digits. Supply exactly one of text or keys.', {
    text: string('Literal text to type, including Unicode.', 8192),
    keys: { type: 'array', items: string('Key name.', 16), minItems: 1, maxItems: 8 },
  }, []),
];

function validate(name, args) {
  const tool = tools.find(tool => tool.name === name);
  if (!tool) throw new Error(`Unknown tool: ${name}`);
  if (!args || typeof args !== 'object' || Array.isArray(args)) throw new Error('Arguments must be an object');
  for (const key of Object.keys(args)) if (!(key in tool.inputSchema.properties)) throw new Error(`Unknown argument: ${key}`);
  for (const key of tool.inputSchema.required) if (!(key in args)) throw new Error(`Missing argument: ${key}`);
  function check(value, spec, key) {
    const type = Array.isArray(value) ? 'array' : typeof value;
    if (spec.type === 'integer' ? !Number.isSafeInteger(value) : type !== spec.type) throw new Error(`Invalid type: ${key}`);
    if (spec.enum && !spec.enum.includes(value)) throw new Error(`Unsupported ${key}`);
    if (type === 'string' && (value.includes('\0') || value.length < (spec.minLength ?? 0) || value.length > spec.maxLength)) throw new Error(`Invalid string: ${key}`);
    if (spec.type === 'integer' && (value < spec.minimum || value > spec.maximum)) throw new Error(`Out of range: ${key}`);
    if (type === 'array') {
      if (value.length < (spec.minItems ?? 0) || value.length > spec.maxItems) throw new Error(`Invalid length: ${key}`);
      value.forEach(item => check(item, spec.items, key));
    }
  }
  for (const [key, value] of Object.entries(args)) check(value, tool.inputSchema.properties[key], key);
  if (name === 'desktop_keyboard' && (('text' in args) === ('keys' in args))) throw new Error('Supply exactly one of text or keys');
  if (name === 'desktop_screenshot') {
    const region = ['x', 'y', 'width', 'height'].filter(key => key in args).length;
    if (region !== 0 && region !== 4) throw new Error('Supply all region coordinates');
    if (region === 4 && args.width * args.height > 32 * 1024 * 1024) throw new Error('Capture region exceeds pixel limit');
  }
  if (name === 'desktop_mouse' && args.action === 'scroll' && !('delta' in args)) throw new Error('Scroll requires delta');
  return args;
}

const children = new Set();
function killTree(child) {
  if (!child.pid || child.exitCode !== null) return;
  const killer = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' });
  killer.on('error', () => child.kill());
}

async function native(action, args, workspace) {
  const seconds = args.timeout_seconds ?? (action === 'powershell' ? 60 : 10);
  const limit = args.max_output_bytes ?? (action === 'powershell' ? 32768 : 131072);
  return new Promise((resolve, reject) => {
    const executable = path.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe');
    const child = spawn(executable, ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', helper, '-TrustedHost'], {
      cwd: workspace, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'],
    });
    children.add(child);
    let stdout = Buffer.alloc(0), stderr = Buffer.alloc(0), stdoutBytes = 0, stderrBytes = 0, timedOut = false;
    const append = (current, chunk) => Buffer.concat([current, chunk.subarray(0, Math.max(0, limit - current.length))]);
    child.stdout.on('data', bytes => { stdoutBytes += bytes.length; stdout = append(stdout, bytes); });
    child.stderr.on('data', bytes => { stderrBytes += bytes.length; stderr = append(stderr, bytes); });
    const timer = setTimeout(() => { timedOut = true; killTree(child); }, (seconds + 2) * 1000);
    child.stdin.on('error', () => {});
    child.on('error', error => { clearTimeout(timer); children.delete(child); reject(error); });
    child.on('close', code => {
      clearTimeout(timer); children.delete(child);
      if (action === 'powershell') resolve({ exit_code: code, timed_out: timedOut || code === 124,
        stdout: stdout.toString('utf8'), stderr: stderr.toString('utf8'),
        stdout_bytes: stdoutBytes, stderr_bytes: stderrBytes,
        stdout_truncated: stdoutBytes > limit, stderr_truncated: stderrBytes > limit });
      else if (code !== 0) reject(new Error(stderr.toString('utf8').trim() || `Native helper exited ${code}`));
      else { try { resolve(JSON.parse(stdout.toString('utf8'))); } catch { reject(new Error('Invalid native helper response')); } }
    });
    child.stdin.end(JSON.stringify({ action, args, timeout_seconds: seconds }));
  });
}

const textResult = result => ({ content: [{ type: 'text', text: JSON.stringify(result) }], isError: false });
export async function callTool(name, input = {}, workspace = process.cwd()) {
  const args = validate(name, input);
  const absolute = name => path.resolve(workspace, name);
  if (name === 'file_read') {
    const file = await fs.open(absolute(args.path), 'r');
    try {
      const stat = await file.stat();
      if (!stat.isFile()) throw new Error('Not a regular file');
      const offset = args.offset ?? 0, buffer = Buffer.alloc(args.max_bytes ?? 32768);
      const { bytesRead } = await file.read(buffer, 0, buffer.length, offset);
      return textResult({ path: absolute(args.path), offset, bytes_read: bytesRead, total_bytes: stat.size,
        truncated: offset + bytesRead < stat.size, text: buffer.subarray(0, bytesRead).toString('utf8') });
    } finally { await file.close(); }
  }
  if (name === 'file_write') {
    const output = absolute(args.path), bytes = Buffer.from(args.content, 'utf8');
    if (bytes.length > 1048576) throw new Error('Content exceeds 1 MiB');
    await fs.mkdir(path.dirname(output), { recursive: true });
    await fs.writeFile(output, bytes, { flag: args.overwrite ? 'w' : 'wx' });
    return textResult({ path: output, bytes: bytes.length });
  }
  if (name === 'directory_list') {
    const directory = absolute(args.path ?? '.'), entries = [], limit = args.limit ?? 100;
    let truncated = false;
    const handle = await fs.opendir(directory);
    for await (const entry of handle) {
      if (entries.length === limit) { truncated = true; break; }
      entries.push({ name: entry.name, type: entry.isSymbolicLink() ? 'link' : entry.isDirectory() ? 'directory' : entry.isFile() ? 'file' : 'other' });
    }
    return textResult({ path: directory, entries, truncated });
  }
  if (name === 'app_launch') {
    if (!path.isAbsolute(args.executable)) throw new Error('Executable must be an absolute path');
    if (path.extname(args.executable).toLowerCase() !== '.exe' || !(await fs.stat(args.executable)).isFile()) throw new Error('Launch requires an existing .exe file');
    return textResult(await native(name, { ...args, cwd: absolute(args.cwd ?? '.') }, workspace));
  }
  const nativeArgs = { ...args };
  if (name === 'desktop_screenshot') {
    const output = absolute(args.output_path ?? `.arun/windows-host/capture-${randomUUID()}.png`);
    await fs.mkdir(path.dirname(output), { recursive: true });
    // Reserve exclusively so capture can never silently overwrite user files.
    const reservation = await fs.open(output, 'wx'); await reservation.close();
    nativeArgs.output_path = output;
    try {
      const metadata = await native(name, nativeArgs, workspace);
      const stat = await fs.stat(output);
      if (stat.size > 4 * 1024 * 1024) throw new Error('PNG exceeds 4 MiB');
      const bytes = await fs.readFile(output);
      return { content: [{ type: 'text', text: JSON.stringify(metadata) },
        { type: 'image', mimeType: 'image/png', data: bytes.toString('base64') }], isError: false };
    } catch (error) { await fs.unlink(output).catch(() => {}); throw error; }
  }
  const result = await native(name, nativeArgs, absolute(args.cwd ?? '.'));
  const reply = textResult(result);
  if (name === 'powershell') reply.isError = result.exit_code !== 0 || result.timed_out;
  return reply;
}

export async function serve() {
  if (!process.argv.slice(2).includes('--trusted-host')) throw new Error('Requires explicit --trusted-host authorization (full Windows PC access)');
  if (process.platform !== 'win32') throw new Error('Windows host MCP requires Windows');
  let buffer = Buffer.alloc(0), queue = Promise.resolve(), pending = 0;
  const send = value => process.stdout.write(`${JSON.stringify(value)}\n`);
  const dispatch = async request => {
    if (!request || request.jsonrpc !== '2.0' || typeof request.method !== 'string') {
      send({ jsonrpc: '2.0', id: request?.id ?? null, error: { code: -32600, message: 'Invalid request' } }); return;
    }
    if (!('id' in request)) return;
    let result;
    if (request.method === 'initialize') result = { protocolVersion: request.params?.protocolVersion ?? '2024-11-05',
      capabilities: { tools: { listChanged: false } }, serverInfo: { name: 'aegis-windows-host', version: '1.0.0' } };
    else if (request.method === 'ping') result = {};
    else if (request.method === 'tools/list') result = { tools };
    else if (request.method === 'tools/call') {
      try { result = await callTool(request.params?.name, request.params?.arguments ?? {}); }
      catch (error) { result = { content: [{ type: 'text', text: error.message }], isError: true }; }
    } else { send({ jsonrpc: '2.0', id: request.id, error: { code: -32601, message: 'Method not found' } }); return; }
    send({ jsonrpc: '2.0', id: request.id, result });
  };
  process.stdin.on('data', chunk => {
    buffer = Buffer.concat([buffer, chunk]);
    while (true) {
      const end = buffer.indexOf(10);
      if (end === -1) break;
      if (end > MAX_MESSAGE || pending >= 16) { process.stderr.write('MCP input limits exceeded\n'); process.exit(2); }
      const line = buffer.subarray(0, end).toString('utf8'); buffer = buffer.subarray(end + 1);
      if (!line.trim()) continue;
      pending++;
      queue = queue.then(async () => {
        try { await dispatch(JSON.parse(line)); }
        catch { send({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'Parse error' } }); }
        finally { pending--; }
      });
    }
    if (buffer.length > MAX_MESSAGE) { process.stderr.write('MCP message too large\n'); process.exit(2); }
  });
  process.stdin.on('end', () => { for (const child of children) killTree(child); queue.finally(() => process.exit(0)); });
  process.stdout.on('error', () => { for (const child of children) killTree(child); process.exit(0); });
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  serve().catch(error => { process.stderr.write(`${error.message}\n`); process.exitCode = 2; });
}
