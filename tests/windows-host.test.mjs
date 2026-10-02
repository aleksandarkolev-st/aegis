import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { callTool, tools } from '../scripts/windows-host.mjs';

const server = fileURLToPath(new URL('../scripts/windows-host.mjs', import.meta.url));
const install = fileURLToPath(new URL('../scripts/windows-host-install.mjs', import.meta.url));
const windows = process.platform === 'win32';
const data = result => JSON.parse(result.content.find(item => item.type === 'text').text);
const closeClients = new Map();

async function scratch(t) {
  const parent = fileURLToPath(new URL('../.arun/windows-host-tests/', import.meta.url));
  await fs.mkdir(parent, { recursive: true });
  const directory = await fs.mkdtemp(path.join(parent, 'host-'));
  t.after(async () => {
    await closeClients.get(directory)?.();
    await fs.rm(directory, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  });
  return directory;
}

async function run(executable, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(executable, args, { windowsHide: true, ...options });
    let stdout = '', stderr = '';
    child.stdout.on('data', chunk => stdout += chunk);
    child.stderr.on('data', chunk => stderr += chunk);
    child.on('error', reject);
    child.on('close', code => resolve({ code, stdout, stderr }));
  });
}

async function client(t, cwd) {
  const child = spawn(process.execPath, [server, '--trusted-host'], { cwd, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] });
  const responses = new Map(); let next = 0, stderr = '';
  child.stderr.on('data', chunk => stderr += chunk);
  readline.createInterface({ input: child.stdout }).on('line', line => {
    const response = JSON.parse(line), entry = responses.get(response.id);
    if (entry) { responses.delete(response.id); clearTimeout(entry.timer); entry.resolve(response); }
  });
  child.on('exit', code => {
    for (const entry of responses.values()) { clearTimeout(entry.timer); entry.reject(new Error(`Server exited ${code}: ${stderr}`)); }
    responses.clear();
  });
  const close = async () => {
    child.stdin.end();
    if (child.exitCode === null) await new Promise(resolve => child.once('exit', resolve));
  };
  closeClients.set(cwd, close);
  t.after(close);
  const request = (method, params) => new Promise((resolve, reject) => {
    const id = ++next;
    const timer = setTimeout(() => { responses.delete(id); child.kill(); reject(new Error(`RPC ${method} timed out`)); }, 25000);
    responses.set(id, { resolve, reject, timer });
    child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
  });
  await request('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'aegis-test', version: '1' } });
  return { request, close, call: async (name, args = {}) => (await request('tools/call', { name, arguments: args })).result };
}

test('Windows host refuses implicit trust before exposing tools', async () => {
  const result = await run(process.execPath, [server]);
  assert.equal(result.code, 2);
  assert.match(result.stderr, /explicit --trusted-host/);
});

test('bounded host files preserve Unicode and require explicit overwrite', async t => {
  const cwd = await scratch(t);
  const content = '日本語 e\u0301 🙂\n'.repeat(20);
  await callTool('file_write', { path: 'nested/test.txt', content }, cwd);
  assert.equal(await fs.readFile(path.join(cwd, 'nested/test.txt'), 'utf8'), content);
  await assert.rejects(callTool('file_write', { path: 'nested/test.txt', content: 'replace' }, cwd), /EEXIST/);
  const read = data(await callTool('file_read', { path: 'nested/test.txt', max_bytes: 16 }, cwd));
  assert.equal(read.bytes_read, 16); assert.equal(read.truncated, true);
  const list = data(await callTool('directory_list', { path: 'nested', limit: 1 }, cwd));
  assert.equal(list.entries[0].name, 'test.txt');
  await callTool('file_write', { path: 'nested/test.txt', content: 'new', overwrite: true }, cwd);
  assert.equal(await fs.readFile(path.join(cwd, 'nested/test.txt'), 'utf8'), 'new');
  await assert.rejects(callTool('file_read', { path: '.', max_bytes: 2 }, cwd));
  await assert.rejects(callTool('powershell', { script: 'nothing', timeout_seconds: 601 }, cwd), /Out of range/);
  await assert.rejects(callTool('file_read', { path: 'nested/test.txt', extra: true }, cwd), /Unknown argument/);
  await assert.rejects(callTool('desktop_screenshot', { width: 50 }, cwd), /all region/);
  await assert.rejects(callTool('desktop_keyboard', { text: 'x', keys: ['A'] }, cwd), /exactly one/);
});

test('actual stdio MCP discovers all tools and reports per-call errors without exiting', { skip: !windows }, async t => {
  const cwd = await scratch(t), rpc = await client(t, cwd);
  const discovery = await rpc.request('tools/list', {});
  assert.equal(discovery.result.tools.length, tools.length);
  assert.equal((await rpc.call('file_write', { path: 'protocol.txt', content: 'stdio✓' })).isError, false);
  const failedCommand = await rpc.call('powershell', { script: 'exit 7' });
  assert.equal(failedCommand.isError, true, 'Failed commands must never become successful MCP evidence');
  assert.equal(data(failedCommand).exit_code, 7);
  assert.equal(data(await rpc.call('file_read', { path: 'protocol.txt' })).text, 'stdio✓');
  assert.equal((await rpc.call('desktop_keyboard', { keys: ['INVALID_KEY'] })).isError, true);
  assert.equal((await rpc.call('desktop_mouse', { action: 'move', x: 32768, y: 32768 })).isError, true);
  assert.equal((await rpc.request('ping', {})).result.constructor, Object);
});

test('actual native PowerShell has Unicode, bounded streams and native failure exit codes', { skip: !windows }, async t => {
  const cwd = await scratch(t), rpc = await client(t, cwd);
  const unicode = data(await rpc.call('powershell', { script: "[Console]::WriteLine('native 日本語 🙂'); [Console]::Error.WriteLine('diagnostic')" }));
  assert.equal(unicode.exit_code, 0); assert.match(unicode.stdout, /native 日本語 🙂/); assert.match(unicode.stderr, /diagnostic/);
  const bounded = data(await rpc.call('powershell', { script: "[Console]::Write(('x' * 20000))", max_output_bytes: 256 }));
  assert.equal(bounded.stdout_truncated, true); assert.equal(bounded.stdout.length, 256); assert.equal(bounded.stdout_bytes, 20000);
  const failure = data(await rpc.call('powershell', { script: 'cmd.exe /c exit 7' }));
  assert.equal(failure.exit_code, 7);
});

test('PowerShell timeout kills its hidden native descendant via owning job', { skip: !windows }, async t => {
  const cwd = await scratch(t), rpc = await client(t, cwd);
  const started = Date.now();
  const output = data(await rpc.call('powershell', {
    script: "$hostTestChild = Start-Process -FilePath powershell.exe -WindowStyle Hidden -ArgumentList @('-NoProfile','-Command','Start-Sleep -Seconds 30') -PassThru; [Console]::WriteLine('CHILD_PID=' + $hostTestChild.Id); Start-Sleep -Seconds 15",
    timeout_seconds: 2,
  }));
  assert.equal(output.timed_out, true); assert.ok(Date.now() - started < 7000);
  assert.equal(output.exit_code, 124, 'Independent native watchdog must fire before the Node backup timeout');
  const pid = Number(output.stdout.match(/CHILD_PID=(\d+)/)?.[1]);
  assert.ok(pid > 0, `Expected a real child process; got ${JSON.stringify(output)}`);
  let alive = true;
  for (let attempt = 0; attempt < 20; attempt++) {
    try { process.kill(pid, 0); } catch { alive = false; break; }
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  assert.equal(alive, false, 'PowerShell invocation must not leave its native child alive');
});

test('app launch executes and survives the short-lived MCP server', { skip: !windows }, async t => {
  const cwd = await scratch(t), rpc = await client(t, cwd);
  const output = path.join(cwd, 'launch.json');
  const quote = value => "'" + value.replaceAll("'", "''") + "'";
  const command = `Start-Sleep -Seconds 2; [IO.File]::WriteAllText(${quote(output)},'executed')`;
  const result = await rpc.call('app_launch', {
    executable: path.join(process.env.SystemRoot, 'System32/WindowsPowerShell/v1.0/powershell.exe'),
    args: ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', Buffer.from(command, 'utf16le').toString('base64')],
  });
  assert.equal(result.isError, false);
  assert.ok(data(result).pid > 0);
  await rpc.close();
  let observed;
  for (let attempt = 0; attempt < 50; attempt++) {
    observed = await fs.readFile(output, 'utf8').catch(() => null);
    if (observed) break;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.equal(observed, 'executed', 'Spawn success alone does not prove execution or lifetime');
});

test('app launch preserves literal arguments through native Windows quoting', { skip: !windows }, async t => {
  const cwd = await scratch(t), rpc = await client(t, cwd);
  const output = path.join(cwd, 'argv.json');
  const expected = ['', 'two words', 'quote"slash\\', 'trailing\\', "literal'", '$(whoami)&echo', '日本語🙂', 'line\nbreak'];
  const script = `require('fs').writeFileSync(${JSON.stringify(output)}, JSON.stringify(process.argv.slice(1)))`;
  const result = await rpc.call('app_launch', { executable: process.execPath, args: ['-e', script, ...expected] });
  assert.equal(result.isError, false, JSON.stringify(result));
  let observed;
  for (let attempt = 0; attempt < 50; attempt++) {
    try { observed = JSON.parse(await fs.readFile(output, 'utf8')); } catch {}
    if (observed) break;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.deepEqual(observed, expected);
});

test('actual desktop discovery and bounded screenshot return valid PNG with coordinate mapping', { skip: !windows }, async t => {
  const cwd = await scratch(t), rpc = await client(t, cwd);
  const windowsResult = await rpc.call('desktop_windows');
  assert.equal(windowsResult.isError, false, JSON.stringify(windowsResult));
  assert.ok(Array.isArray(data(windowsResult).windows));
  const result = await rpc.call('desktop_screenshot', { max_width: 64, max_height: 64 });
  assert.equal(result.isError, false, JSON.stringify(result));
  const metadata = data(result), block = result.content.find(item => item.type === 'image');
  assert.equal(block.mimeType, 'image/png');
  const bytes = Buffer.from(block.data, 'base64');
  assert.equal(bytes.subarray(0, 8).toString('hex'), '89504e470d0a1a0a');
  assert.ok(metadata.width <= 64 && metadata.height <= 64);
  const expectedScale = Math.min(1, 64 / metadata.screen_region.width, 64 / metadata.screen_region.height);
  assert.equal(metadata.width, Math.max(1, Math.round(metadata.screen_region.width * expectedScale)));
  assert.equal(metadata.height, Math.max(1, Math.round(metadata.screen_region.height * expectedScale)));
  assert.ok(metadata.width > 1 && metadata.height > 1, 'A real full-screen capture must not collapse to a one-pixel image');
  assert.equal(bytes.readUInt32BE(16), metadata.width); assert.equal(bytes.readUInt32BE(20), metadata.height);
  assert.deepEqual(await fs.readFile(metadata.path), bytes);
  assert.equal(Math.round(metadata.screen_pixels_per_image_pixel.x * metadata.width), metadata.screen_region.width);
  const overwritten = await rpc.call('desktop_screenshot', { output_path: metadata.path });
  assert.equal(overwritten.isError, true);
});

test('workspace installer uses actual Aegis binary and preserves selection while registering exact grants', { skip: !windows }, async t => {
  const cwd = await scratch(t), native = fileURLToPath(new URL('../target/debug/arun.exe', import.meta.url));
  await fs.mkdir(path.join(cwd, '.arun'));
  const profile = { provider: 'custom', model: 'fixture', endpoint: { base_url: 'http://localhost:1/v1', response_format: 'schema', api_key_env: null, allow_insecure: false }, write: false, image: null, mcp_grants: ['mcp:existing:tool'] };
  await fs.writeFile(path.join(cwd, '.arun/profile.json'), JSON.stringify(profile));
  const result = await run(process.execPath, [install, '--trusted-host', '--workspace', cwd, '--aegis', native]);
  assert.equal(result.code, 0, result.stderr);
  const saved = JSON.parse(await fs.readFile(path.join(cwd, '.arun/profile.json'), 'utf8'));
  assert.equal(saved.model, profile.model); assert.deepEqual(saved.endpoint, profile.endpoint);
  assert.deepEqual(new Set(saved.mcp_grants), new Set(['mcp:existing:tool', ...tools.map(tool => `mcp:windows-host:${tool.name}`)]));
  assert.ok((await fs.stat(path.join(cwd, '.arun/runs.sqlite'))).isFile());
  assert.match(result.stdout, /mcp\.windows-host\.powershell/);
  assert.match(result.stdout, /mcp\.windows-host\.desktop_screenshot/);
});
