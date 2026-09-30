// Explicitly invoked hosted functional check; never runs as part of npm test.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const binaryArgument = args[args.indexOf('--binary') + 1];
assert.ok(args.includes('--live') && args.includes('--binary') && binaryArgument,
  'Use node scripts/live-control-smoke.mjs --live --binary <actual-installed-arun-binary>');
const binary = path.resolve(binaryArgument);
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const binaryHash = digest(await readFile(binary));
const outputRoot = path.join(repository, '.arun');
await mkdir(outputRoot, { recursive: true });
const continuation = args.includes('--continue') ? path.resolve(args[args.indexOf('--continue') + 1]) : null;
const directory = continuation ?? await mkdtemp(path.join(outputRoot, 'luna-control-live-'));
const priorReceipt = continuation ? JSON.parse(await readFile(path.join(directory, 'receipt.json'), 'utf8')) : null;
const workspace = path.join(directory, 'workspace');
if (!continuation) await mkdir(workspace);
const parser = `export function evaluate(expression) {
  const source = String(expression).trim();
  if (!/^\\d+(?:\\s*[+-]\\s*\\d+)*$/.test(source)) throw new SyntaxError('Invalid expression');
  const tokens = source.match(/\\d+|[+-]/g);
  let result = Number(tokens[0]);
  for (let i = 1; i < tokens.length; i += 2) result += (tokens[i] === '+' ? 1 : -1) * Number(tokens[i + 1]);
  return result;
}
`;
const originalTests = `import test from 'node:test';
import assert from 'node:assert/strict';
import { evaluate } from './parser.mjs';
test('flat arithmetic public API', () => {
  assert.equal(evaluate('2+3-1'), 4);
  assert.equal(evaluate(' 10 - 2 + 5 '), 13);
  assert.equal(evaluate('0'), 0);
});
test('invalid inputs remain errors', () => {
  for (const source of ['', '2+', 'word']) assert.throws(() => evaluate(source), SyntaxError);
});
`;
if (!continuation) {
  await writeFile(path.join(workspace, 'parser.mjs'), parser);
  await writeFile(path.join(workspace, 'parser.test.mjs'), originalTests);
}
const originalTestHash = digest(originalTests);
const guard = `const assert=require('node:assert/strict');const fs=require('node:fs');const crypto=require('node:crypto');const cp=require('node:child_process');
(async()=>{const api=await import('./parser.mjs');assert.deepEqual(Object.keys(api),['evaluate']);
for(const [source,expected] of [['2+(3-1)',4],['((10-2)+(5-1))',12],['1-(2-(3+4))',6],['(0)',0],[' 7 + ( 8 - ( 2 + 1 ) ) ',12]])assert.equal(api.evaluate(source),expected,source);
for(const source of ['(2+3','2+3)','()','1+(2+)','-1','+1','1++2','1--2'])assert.throws(()=>api.evaluate(source),SyntaxError);
assert.doesNotMatch(fs.readFileSync('parser.mjs','utf8'),/\\b(?:eval|Function)\\s*\\(/,'Forbidden dynamic evaluation');
assert.equal(crypto.createHash('sha256').update(fs.readFileSync('parser.test.mjs')).digest('hex'),'${originalTestHash}','Existing tests changed');
const regression=fs.readFileSync('nested.test.mjs','utf8');assert.ok(regression.includes('test(')&&regression.includes('evaluate('),'Missing regression tests');
const suite=cp.spawnSync(process.execPath,['--test'],{encoding:'utf8'});process.stdout.write(suite.stdout);process.stderr.write(suite.stderr);assert.equal(suite.status,0,'Full suite failed');console.log('INDEPENDENT_PARSER_ACCEPTANCE_PASSED');})().catch(error=>{console.error(error);process.exitCode=1;});`;
const acceptance = { name: 'Independent nested parser/API/regression/full-suite check', program: 'node', args: ['-e', guard], image: 'node:22-alpine', seconds: 60 };
if (priorReceipt) assert.equal(digest(JSON.stringify(acceptance)), priorReceipt.acceptance_sha256, 'Frozen acceptance changed; preserve the old trial');
const acceptanceFile = path.join(directory, 'acceptance.json');
await writeFile(acceptanceFile, JSON.stringify(acceptance, null, 2) + '\n');
const commandScopes = path.join(directory, 'command-scopes.json');
await writeFile(commandScopes, JSON.stringify({ commands: [{ program: 'node', args: ['--test'] }] }, null, 2) + '\n');
const task = `Refactor parser.mjs to support nested parentheses in integer addition/subtraction expressions. Preserve evaluate(expression) and its existing flat-expression behavior and SyntaxError behavior. Keep parser.test.mjs unchanged. Create nested.test.mjs with regression coverage. Use the approved node --test command to run all tests. Do not use eval or Function. The runtime runs an independent acceptance check after Finish.

Requirements:
- support nested expressions
- preserve public API compatibility
- add regression tests
- full test suite must pass`;
await writeFile(path.join(directory, 'task.txt'), task + '\n');
const receipt = priorReceipt ?? { directory, workspace, binary, binary_sha256: binaryHash, model: 'gpt-6-luna', reasoning_effort: 'low', acceptance_sha256: digest(JSON.stringify(acceptance)), phase: 'prepared', run_id: null };
if (receipt.binary_sha256 !== binaryHash) {
  assert.ok(continuation && args.includes('--upgrade-binary'),'Use --upgrade-binary to record a reviewed runtime fix while retaining this run');
  (receipt.binary_history ??= []).push({ binary:receipt.binary, sha256:receipt.binary_sha256 });
  receipt.binary = binary;
  receipt.binary_sha256 = binaryHash;
}
async function save() { await writeFile(path.join(directory, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n'); }
await save();

function start(program, argv, name, cwd = workspace) {
  const child = spawn(program, argv, { cwd, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  const log = createWriteStream(path.join(directory, `${name}.log`));
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', bytes => { stdout += bytes; log.write(bytes); });
  child.stderr.on('data', bytes => { stderr += bytes; log.write(bytes); });
  const finished = new Promise((resolve, reject) => {
    child.once('error', error => { log.end(); reject(error); });
    child.once('close', (code, signal) => { log.end(); resolve({ code, signal, stdout, stderr }); });
  });
  return { child, finished, text: () => stdout };
}
async function command(argv, name) {
  const result = await start(binary, argv, name).finished;
  assert.equal(result.code, 0, `${name}: ${result.stderr}`);
  return result.stdout;
}
if (!continuation) {
const baseline = await start('docker', ['run', '--rm', '--pull=never', '--network', 'none', '--read-only', '--mount', `type=bind,source=${workspace},target=/workspace,readonly`, '--workdir', '/workspace', 'node:22-alpine', 'node', '-e', guard], 'starter-acceptance').finished;
assert.notEqual(baseline.code, 0, 'The independent acceptance check must reject the starter');
assert.match(baseline.stderr, /Invalid expression|AssertionError/, 'Starter must fail for parser correctness, not an unavailable environment');
receipt.phase = 'starter-rejected'; await save();
console.log(`Prepared functional check: ${directory}`);

const running = start(binary, ['run', task, '--provider', 'chatgpt', '--model', receipt.model, '--reasoning', 'low', '--allow-write', '--allow-process', 'node', '--command-scopes', commandScopes, '--image', 'node:22-alpine', '--acceptance', acceptanceFile, '--actions', '60', '--model-tokens', '180000', '--wall-seconds', '14400', '--foreground'], 'first-run');
const started = Date.now();
while (!receipt.run_id) {
  const match = running.text().match(/run: ([0-9a-f-]{36})/);
  if (match) { receipt.run_id = match[1]; break; }
  assert.equal(running.child.exitCode, null, 'Run exited before returning its ID');
  assert.ok(Date.now() - started < 30000, 'Run ID observation timed out; inspect the live process before retrying');
  await new Promise(resolve => setTimeout(resolve, 100));
}
receipt.phase = 'inference-running'; await save();
console.log(`Actual Aegis run: ${receipt.run_id} / ${receipt.model} / low`);
let firstEvents;
while (true) {
  const text = await command(['replay', receipt.run_id], 'pause-observation');
  firstEvents = text.trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
  if (firstEvents.some(event => event.kind === 'model.started')) break;
  assert.equal(running.child.exitCode, null, 'Runner exited before its first hosted inference');
  assert.ok(Date.now() - started < 60000, 'Inference observation timed out; inspect this run before restarting');
  await new Promise(resolve => setTimeout(resolve, 100));
}
await command(['pause', receipt.run_id], 'pause-request');
const firstResult = await running.finished;
assert.equal(firstResult.code, 0, firstResult.stderr);
const listing = await command(['list'], 'paused-list');
assert.match(listing, new RegExp(`${receipt.run_id} paused `));
receipt.phase = 'paused'; await save();
}
const before = (await command(['replay', receipt.run_id], 'paused-replay')).trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
const inherited = JSON.parse(await command(['handoff', receipt.run_id], 'before-resume-handoff'));
assert.ok(inherited.process_programs.includes('node'),'The immutable trial must explicitly grant process:node');
assert.deepEqual(inherited.command_scopes,{ commands: [{ program:'node',args:['--test'] }] });
assert.equal(inherited.task,task);
assert.equal(inherited.current_route.model,receipt.model);
assert.equal(inherited.current_route.reasoning_effort,'low');
const contractHash = digest(JSON.stringify({ task:inherited.task,process_programs:inherited.process_programs,command_scopes:inherited.command_scopes,current_route:inherited.current_route }));
if (receipt.contract_sha256) assert.equal(contractHash,receipt.contract_sha256,'Continuation contract changed');
receipt.contract_sha256 = contractHash; await save();
assert.ok(before.some(event => ['model.response','model.failed'].includes(event.kind) && event.payload.usage?.source === 'provider'), 'A real hosted response, valid or rejected, must be accounted before pause');
assert.ok(before.some(event => event.kind === 'run.paused'));
assert.ok(before.some(event => event.kind === 'checkpoint.created'));
await command(['goal', receipt.run_id], 'paused-goal');
await command(['verify', receipt.run_id], 'paused-blockers');
console.log('Same run retains a recorded hosted response and durable pause/checkpoint. Resuming.');
const resumeNumber = (receipt.resume_count ?? 0) + 1;
receipt.resume_count = resumeNumber;
receipt.phase = 'resuming'; await save();
await command(['resume', receipt.run_id, '--foreground'], `resumed-run-${resumeNumber}`);
const listing = await command(['list'], 'completed-list');
if (receipt.terminal_listing && receipt.terminal_listing !== listing) {
  (receipt.previous_stop_listings ??= []).push(receipt.terminal_listing);
}
receipt.terminal_listing = listing;
if (!listing.includes(`${receipt.run_id} completed `)) {
  receipt.phase = 'stopped';
  await command(['budget', receipt.run_id], `stopped-budget-${resumeNumber}`);
  await command(['replay', receipt.run_id], `stopped-replay-${resumeNumber}`);
  await save();
}
assert.match(listing, new RegExp(`${receipt.run_id} completed `), 'Actual task did not complete; inspect the existing run and logs');
receipt.phase = 'completed'; await save();
const events = (await command(['replay', receipt.run_id], 'completed-replay')).trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
const completed = events.findLast(event => event.kind === 'run.completed');
assert.ok(events.some(event => event.kind === 'acceptance.passed'));
const explicit = completed.payload.completion.requirements.filter(item => item.id > 0);
assert.equal(explicit.length, 4);
assert.ok(explicit.every(item => item.state === 'verified' && item.revision === completed.payload.completion.workspace_revision));
assert.equal(digest(await readFile(path.join(workspace, 'parser.test.mjs'))), originalTestHash);
for (const [commandName, extra] of [['goal', []], ['status', []], ['why', []], ['evidence', ['O3']], ['verify', []], ['provider', ['history']], ['budget', []], ['handoff', []]]) {
  await command([commandName, receipt.run_id, ...extra], `final-${commandName}`);
}
const finalState = JSON.parse(await readFile(path.join(directory,'final-handoff.log'),'utf8'));
assert.equal(digest(JSON.stringify({ task:finalState.task,process_programs:finalState.process_programs,command_scopes:finalState.command_scopes,current_route:finalState.current_route })),contractHash);
receipt.actions = events.filter(event => event.kind === 'model.response').length;
receipt.recorded_tokens = events.filter(event => ['model.response', 'model.failed'].includes(event.kind)).reduce((total, event) => total + (event.payload.usage?.input_tokens ?? 0) + (event.payload.usage?.output_tokens ?? 0), 0);
receipt.workspace_revision = completed.payload.completion.workspace_revision;
receipt.acceptance_artifact = completed.payload.acceptance;
receipt.completion = completed.payload.completion;
receipt.phase = 'verified'; await save();
console.log(`Hosted functional verification passed: ${receipt.actions} responses, ${receipt.recorded_tokens} recorded tokens. Receipt: ${path.join(directory, 'receipt.json')}`);
