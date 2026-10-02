// Explicit hosted E2E: actual Aegis binary, real provider, delayed container output.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(name);
  if (index < 0) return fallback;
  assert.ok(args[index + 1] && !args[index + 1].startsWith('--'), `${name} needs a value`);
  return args[index + 1];
};
assert.ok(args.includes('--live'), 'Use --live --binary <arun.exe> to authorize hosted verification');
const binary = path.resolve(option('--binary', 'target/debug/arun.exe'));
const model = option('--model', 'gpt-6.1-sol');
const reasoning = option('--reasoning', 'high');
const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
await mkdir(path.join(repository, '.arun'), { recursive: true });
const directory = await mkdtemp(path.join(repository, '.arun', 'hosted-output-live-'));
const workspace = path.join(directory, 'workspace');
await mkdir(workspace);
const source = "console.log('AEGIS_LIVE_START'); await new Promise(r=>setTimeout(r,4000)); console.log('x'.repeat(70000)); await new Promise(r=>setTimeout(r,4000)); console.log('AEGIS_LIVE_END');";
const commandArgs = ['--input-type=module', '-e', source];
const scopeFile = path.join(directory, 'command-scopes.json');
await writeFile(scopeFile, JSON.stringify({ commands: [{ program: 'node', args: commandArgs }] }));
const task = `Run process.run once with program node and exactly these args: ${JSON.stringify(commandArgs)}. This approved container command prints AEGIS_LIVE_START, a large preview payload, and AEGIS_LIVE_END. Keep the workspace unchanged. Use the successful command's output artifact as evidence and finish; do not repeat the command.\nRequirements:\n- execute the approved marker command successfully`;
const receipt = { directory, workspace, binary, binary_sha256: createHash('sha256').update(await readFile(binary)).digest('hex'), model, reasoning_effort: reasoning, phase: 'prepared' };
const save = () => writeFile(path.join(directory, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n');
await save();
function start(argv) {
  const child = spawn(binary, argv, { cwd: workspace, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '', stderr = '';
  child.stdout.on('data', chunk => { stdout += chunk; });
  child.stderr.on('data', chunk => { stderr += chunk; });
  const finished = new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', code => resolve({ code, stdout, stderr }));
  });
  return { child, finished, text: () => stdout };
}
async function command(argv) {
  const result = await start(argv).finished;
  assert.equal(result.code, 0, result.stderr);
  return result.stdout;
}
const runner = start(['run', task, '--provider', 'chatgpt', '--model', model, '--reasoning', reasoning, '--mode', 'eager', '--allow-process', 'node', '--command-scopes', scopeFile, '--image', 'node:22-alpine', '--process-seconds', '30', '--wall-seconds', '600', '--foreground']);
console.log(`Hosted stream verification: ${directory}`);
const deadline = Date.now() + 660000;
let events = [];
while (Date.now() < deadline) {
  if (!receipt.run_id) {
    receipt.run_id = runner.text().match(/run: ([0-9a-f-]{36})/)?.[1];
    if (receipt.run_id) { receipt.phase = 'running'; await save(); }
  }
  if (receipt.run_id) {
    events = (await command(['replay', receipt.run_id])).trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
    const chunk = events.find(event => event.kind === 'operation.output' && event.payload.text?.includes('AEGIS_LIVE_START'));
    if (chunk && !events.some(event => event.kind === 'operation.succeeded' && event.payload.id === chunk.payload.id)) {
      receipt.live_output_observed_before_exit = true;
      receipt.first_live_output_at = chunk.created_at;
    }
    if (events.some(event => ['run.completed', 'run.failed', 'run.cancelled', 'run.paused', 'run.answered'].includes(event.kind))) break;
  }
  if (runner.child.exitCode !== null) break;
  await new Promise(resolve => setTimeout(resolve, 200));
}
if (runner.child.exitCode === null && Date.now() >= deadline) {
  if (receipt.run_id) await command(['pause', receipt.run_id]);
  receipt.phase = 'timed-out'; await save();
  throw new Error('Verification timed out; retained task and receipt for inspection');
}
const result = await runner.finished;
await writeFile(path.join(directory, 'runner.log'), result.stdout + result.stderr);
await writeFile(path.join(directory, 'replay.json'), JSON.stringify(events, null, 2));
assert.equal(result.code, 0, result.stderr);
assert.ok(receipt.live_output_observed_before_exit, 'First marker was not observed before process success');
assert.ok(events.some(event => event.kind === 'operation.output' && event.payload.truncated === true), 'Missing bounded live preview notice');
const routes = events.filter(event => event.kind === 'model.started').map(event => event.payload.route);
assert.ok(routes.length && routes.every(route => route.model === model && route.reasoning_effort === reasoning), 'Recorded route differs from requested model/effort');
const completed = events.find(event => event.kind === 'run.completed');
assert.ok(completed, 'The actual hosted task did not complete');
const processResults = events.filter(event => event.kind === 'operation.succeeded' && event.payload.detail?.capability === 'process.run');
assert.equal(processResults.length, 1, 'Marker process must succeed exactly once');
const output = await command(['inspect', processResults[0].payload.artifact]);
assert.ok(output.includes('AEGIS_LIVE_START') && output.includes('AEGIS_LIVE_END'), 'Full output artifact lost markers beyond preview cap');
assert.ok(output.includes('x'.repeat(70000)), 'Full saved command output was truncated');
receipt.phase = 'verified';
receipt.model_calls = routes.length;
receipt.completed_at = completed.created_at;
receipt.output_artifact = processResults[0].payload.artifact;
await save();
console.log(`Live output, preview cap, full artifact and ${model}/${reasoning} verified: ${path.join(directory, 'receipt.json')}`);
