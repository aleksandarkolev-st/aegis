// Hosted Windows endurance check. Timed stages exercise orchestration, not coding quality.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

export function checkTrial(receipt, events, stages, elapsedMs) {
  assert.ok(events.some(event => event.kind === 'run.completed'), 'Aegis must actually complete');
  assert.equal(stages.length, receipt.stages, 'Missing completed stages');
  assert.ok(elapsedMs >= receipt.stages * receipt.seconds_per_stage * 1000, 'Actual supervised duration is too short');
  let previousEnd = 0;
  stages.forEach((stage, index) => {
    assert.equal(stage.stage, index + 1, 'Stage order or identity changed');
    assert.ok(stage.elapsed_ms >= receipt.seconds_per_stage * 1000, 'A stage skipped its timed work');
    const start = Date.parse(stage.started_at), end = Date.parse(stage.completed_at);
    assert.ok(Number.isFinite(start) && Number.isFinite(end) && start >= previousEnd && end >= start, 'Invalid or overlapping stage times');
    previousEnd = end;
  });
  const dispatches = events.filter(event => event.kind === 'operation.executing').map(event => event.payload.id);
  assert.equal(new Set(dispatches).size, dispatches.length, 'An operation was dispatched twice');
  assert.ok(!events.some(event => event.kind === 'operation.outcome_unknown'), 'Unknown effects require reconciliation');
  const routes = events.filter(event => event.kind === 'model.started').map(event => event.payload.route);
  assert.ok(routes.length && routes.every(route => route.model === receipt.model && route.reasoning_effort === receipt.reasoning_effort), 'Model route changed');
  const live = events.filter(event => event.kind === 'operation.output' && event.payload.text?.includes('SOAK_STAGE_'));
  const liveOperations = new Set(live.filter(event => events.some(success => success.kind === 'operation.succeeded'
    && success.payload.id === event.payload.id && success.seq > event.seq)).map(event => event.payload.id));
  assert.equal(liveOperations.size, receipt.stages, 'Each stage must produce live output before completion');
  if (receipt.seconds_per_stage >= 90) {
    for (const id of liveOperations) {
      const chunks = live.filter(event => event.payload.id === id);
      const heartbeatCount = chunks.map(event => event.payload.text).join('').match(/SOAK_STAGE_\d+_HEARTBEAT/g)?.length ?? 0;
      assert.ok(heartbeatCount >= Math.floor(receipt.seconds_per_stage / 30) - 1, 'Timed work must retain periodic heartbeats');
      for (let index = 1; index < chunks.length; index++) {
        assert.ok(chunks[index].created_at - chunks[index - 1].created_at <= 90, 'Native heartbeat observation gap exceeded 90 seconds');
      }
    }
  }
  assert.ok(events.some(event => event.kind === 'operation.output' && event.payload.text?.includes(`SOAK_VERIFIED stages=${receipt.stages}`)
    && events.some(success => success.kind === 'operation.succeeded' && success.payload.id === event.payload.id && success.seq > event.seq)),
  'Final stage verification must execute successfully');
  assert.ok(events.some(event => event.kind === 'checkpoint.created'), 'No durable checkpoint was saved');
  return { model_attempts: routes.length, stage_operations_with_live_output: liveOperations.size, repeated_dispatches: 0 };
}

async function main() {
  assert.equal(process.platform, 'win32', 'This check requires native Windows');
  const args = process.argv.slice(2);
  const allowed = new Set(['--live', '--binary', '--model', '--reasoning', '--stages', '--seconds-per-stage']);
  const options = {};
  for (let index = 0; index < args.length; index++) {
    const key = args[index];
    assert.ok(allowed.has(key) && !(key in options), `Unknown or duplicate option: ${key}`);
    if (key === '--live') options[key] = true;
    else { assert.ok(args[index + 1] && !args[index + 1].startsWith('--'), `${key} needs a value`); options[key] = args[++index]; }
  }
  assert.ok(options['--live'] && options['--binary'], 'Use --live --binary <installed arun.exe>');
  const count = Number(options['--stages'] ?? 16), seconds = Number(options['--seconds-per-stage'] ?? 450);
  assert.ok(Number.isInteger(count) && count >= 1 && count <= 32, 'Stages must be 1..32');
  assert.ok(Number.isInteger(seconds) && seconds >= 1 && seconds <= 540, 'Seconds per stage must be 1..540');
  const binary = path.resolve(options['--binary']);
  const helper = path.resolve(path.dirname(binary), '..', '..', 'scripts', 'windows-host.mjs');
  const helperPs = path.join(path.dirname(helper), 'windows-host.ps1');
  const model = options['--model'] ?? 'gpt-6-luna', reasoning = options['--reasoning'] ?? 'medium';
  assert.ok(['low', 'medium', 'high'].includes(reasoning), 'Unsupported reasoning level');
  await mkdir(path.join(repository, '.arun'), { recursive: true });
  const directory = await mkdtemp(path.join(repository, '.arun', 'native-soak-'));
  const workspace = path.join(directory, 'workspace');
  await mkdir(workspace);
  const wallSeconds = count * seconds + 3600;
  const receipt = { directory, workspace, binary, binary_sha256: sha(await readFile(binary)),
    node: process.execPath, node_sha256: sha(await readFile(process.execPath)), helper,
    helper_sha256: sha(await readFile(helper)), helper_ps_sha256: sha(await readFile(helperPs)),
    model, reasoning_effort: reasoning, stages: count, seconds_per_stage: seconds,
    minimum_duration_seconds: count * seconds, wall_seconds: wallSeconds, phase: 'prepared',
    production_readiness_verified: false,
    validation_scope: 'Native hosted orchestration endurance with deterministic timed stages; not a complex coding task, adversarial isolation check or production certificate.' };
  const controllerSource = await readFile(fileURLToPath(import.meta.url));
  receipt.controller_source_sha256 = sha(controllerSource);
  await writeFile(path.join(directory, 'controller-snapshot.mjs'), controllerSource);
  const save = () => writeFile(path.join(directory, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n');
  const stageScript = `param([int]$Stage)
$ErrorActionPreference = 'Stop'
$config = Get-Content './stage-config.json' -Raw | ConvertFrom-Json
if ($Stage -lt 1 -or $Stage -gt $config.stages) { throw 'Invalid stage' }
$startFile = ('started-{0:D2}.json' -f $Stage)
$doneFile = ('completed-{0:D2}.json' -f $Stage)
if ((Test-Path $startFile) -or (Test-Path $doneFile)) { throw 'Duplicate stage execution' }
if ($Stage -gt 1 -and !(Test-Path ('completed-{0:D2}.json' -f ($Stage - 1)))) { throw 'Stages must run in order' }
$started = [DateTime]::UtcNow.ToString('o')
$clock = [Diagnostics.Stopwatch]::StartNew()
$record = @{stage=$Stage; started_at=$started}
function Save-Receipt($Name, $Value) {
  $stream = [IO.File]::Open((Join-Path (Get-Location).Path $Name), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
  try {
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes(($Value | ConvertTo-Json -Compress))
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush($true)
  } finally { $stream.Dispose() }
}
Save-Receipt $startFile $record
[Console]::WriteLine(('SOAK_STAGE_{0:D2}_START' -f $Stage))
$heartbeat = 30
while ($clock.Elapsed.TotalSeconds -lt $config.seconds_per_stage) {
  Start-Sleep -Milliseconds 250
  if ($clock.Elapsed.TotalSeconds -ge $heartbeat) {
    [Console]::WriteLine(('SOAK_STAGE_{0:D2}_HEARTBEAT elapsed={1}' -f $Stage, [int]$clock.Elapsed.TotalSeconds))
    $heartbeat += 30
  }
}
$record.completed_at = [DateTime]::UtcNow.ToString('o')
$record.elapsed_ms = $clock.ElapsedMilliseconds
Save-Receipt $doneFile $record
[Console]::WriteLine(('SOAK_STAGE_{0:D2}_DONE elapsed_ms={1}' -f $Stage, $record.elapsed_ms))
`;
  const verifyScript = `$ErrorActionPreference = 'Stop'
$config = Get-Content './stage-config.json' -Raw | ConvertFrom-Json
$finished = @(Get-ChildItem -LiteralPath . -Filter 'completed-*.json')
$started = @(Get-ChildItem -LiteralPath . -Filter 'started-*.json')
if ($finished.Count -ne $config.stages -or $started.Count -ne $config.stages) { throw 'Wrong stage receipt count' }
for ($stage=1; $stage -le $config.stages; $stage++) {
  $result = Get-Content ('completed-{0:D2}.json' -f $stage) -Raw | ConvertFrom-Json
  if ($result.stage -ne $stage -or $result.elapsed_ms -lt ($config.seconds_per_stage * 1000)) { throw 'Invalid stage duration or identity' }
}
[Console]::WriteLine(('SOAK_VERIFIED stages={0}' -f $config.stages))
`;
  const fixtures = { 'stage.ps1': stageScript, 'verify.ps1': verifyScript,
    'stage-config.json': JSON.stringify({ stages: count, seconds_per_stage: seconds }) + '\n' };
  const task = `Perform an unattended native Windows endurance job in this workspace.
Run all ${count} stages in order. Each stage performs ${seconds} seconds of timed work and emits heartbeats.
Use mcp.windows-host.powershell with script "& './stage.ps1' -Stage N" and timeout_seconds ${seconds + 30}.
Invoke EACH stage in a separate tool call. Never batch stages in one PowerShell call: the command deadline would expire.
Do not repeat a stage; duplicate invocation is rejected. Do not alter stage.ps1, verify.ps1 or stage-config.json.
Save a checkpoint before starting, after every four completed stages, and before final verification. Keep completed stages in the handoff so context recovery cannot repeat work.
After every stage succeeds, run & './verify.ps1' with the same native tool. Finish using its current successful evidence, updating your milestones and original obligations as required. Do not claim completion from progress messages.
Requirements:
- complete every timed stage exactly once and in order
- preserve the supplied fixture files
- verify all persisted stage receipts before finishing
`;
  fixtures['TASK.txt'] = task;
  receipt.fixtures = {};
  for (const [name, text] of Object.entries(fixtures)) {
    await writeFile(path.join(workspace, name), text);
    receipt.fixtures[name] = sha(Buffer.from(text));
  }
  await save();
  const command = (argv, deadline = 20000) => new Promise((resolve, reject) => {
    const child = spawn(binary, argv, { cwd: workspace, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'], signal: AbortSignal.timeout(deadline) });
    let stdout = '', stderr = '';
    child.stdout.on('data', chunk => { stdout += chunk; if (stdout.length > 16 * 1024 * 1024) child.kill(); });
    child.stderr.on('data', chunk => { if (stderr.length < 65536) stderr += chunk; });
    child.once('error', reject);
    child.once('close', code => code === 0 ? resolve(stdout) : reject(new Error(`Command exited ${code}: ${stderr}`)));
  });
  await command(['mcp', 'add', 'windows-host', '--trusted-host', process.execPath, helper, '--trusted-host']);
  const started = performance.now();
  receipt.started_at = new Date().toISOString();
  const controller = new AbortController();
  const runner = spawn(binary, ['run', task, '--provider', 'chatgpt', '--model', model, '--reasoning', reasoning,
    '--mode', 'durable', '--allow-write', '--allow-mcp', 'windows-host:powershell',
    '--process-seconds', String(seconds + 30), '--wall-seconds', String(wallSeconds), '--foreground'],
  { cwd: workspace, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'], signal: controller.signal });
  const output = createWriteStream(path.join(directory, 'runner.stdout.log'));
  const errors = createWriteStream(path.join(directory, 'runner.stderr.log'));
  runner.stdout.pipe(output); runner.stderr.pipe(errors);
  let header = '';
  runner.stdout.on('data', bytes => {
    if (!receipt.run_id) {
      header = (header + bytes.toString('utf8')).slice(-4096);
      receipt.run_id = header.match(/run: ([0-9a-f-]{36})/)?.[1];
    }
  });
  let ended = false;
  const finish = new Promise(resolve => {
    runner.once('error', error => { receipt.runner_error = error.message; });
    runner.once('close', code => { ended = true; resolve(code); });
  });
  receipt.phase = 'running'; receipt.controller_pid = process.pid; receipt.runner_pid = runner.pid;
  await save();
  console.log(`Native hosted soak: ${directory}`);
  let events = [], observationFailures = 0;
  try {
    while (!ended) {
      if (performance.now() - started >= (wallSeconds + 30) * 1000) {
        receipt.phase = 'supervisor-deadline'; controller.abort(); break;
      }
      if (receipt.run_id) {
        try {
          events = (await command(['replay', receipt.run_id])).trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
          receipt.completed_stages = events.filter(event => event.kind === 'operation.output' && /SOAK_STAGE_\d+_DONE/.test(event.payload.text ?? '')).length;
          receipt.elapsed_seconds = Math.floor((performance.now() - started) / 1000);
          receipt.last_observed_at = new Date().toISOString(); receipt.observation_failures = observationFailures;
          await save();
        } catch (error) { observationFailures++; receipt.observation_error = error.message; await save(); }
      }
      await Promise.race([finish, new Promise(resolve => setTimeout(resolve, 10000))]);
    }
    const code = await finish;
    receipt.runner_exit_code = code;
    assert.equal(code, 0, 'The supervised Aegis runner failed');
    assert.ok(receipt.run_id, 'No run identity was observed');
    events = (await command(['replay', receipt.run_id])).trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
    await writeFile(path.join(directory, 'events.json'), JSON.stringify(events, null, 2));
    assert.ok(events.some(event => event.kind === 'run.completed'),
      `Aegis did not complete: ${events.filter(event => event.kind.startsWith('run.')).at(-1)?.kind ?? 'unknown state'}`);
    for (const [name, hash] of Object.entries(receipt.fixtures)) assert.equal(sha(await readFile(path.join(workspace, name))), hash, `Fixture changed: ${name}`);
    assert.equal(sha(await readFile(binary)), receipt.binary_sha256, 'Runtime changed during trial');
    assert.equal(sha(await readFile(helper)), receipt.helper_sha256, 'Native helper changed during trial');
    assert.equal(sha(await readFile(helperPs)), receipt.helper_ps_sha256, 'PowerShell helper changed during trial');
    const stages = [];
    const names = await readdir(workspace);
    assert.equal(names.filter(name => /^started-\d+\.json$/.test(name)).length, count, 'Unexpected started stage receipts');
    assert.equal(names.filter(name => /^completed-\d+\.json$/.test(name)).length, count, 'Unexpected completed stage receipts');
    for (let stage = 1; stage <= count; stage++) {
      stages.push(JSON.parse(await readFile(path.join(workspace, `completed-${String(stage).padStart(2, '0')}.json`), 'utf8')));
      const began = JSON.parse(await readFile(path.join(workspace, `started-${String(stage).padStart(2, '0')}.json`), 'utf8'));
      assert.equal(began.stage, stage); assert.equal(began.started_at, stages.at(-1).started_at);
    }
    receipt.metrics = checkTrial(receipt, events, stages, performance.now() - started);
    receipt.phase = count * seconds >= 7200 ? 'two-hour-endurance-verified' : 'short-diagnostic-verified';
    receipt.finished_at = new Date().toISOString(); receipt.elapsed_ms = Math.round(performance.now() - started);
    await save();
    console.log(`Verified ${count} stages: ${path.join(directory, 'receipt.json')}`);
  } catch (error) {
    if (!ended) { controller.abort(); await finish; }
    receipt.phase = 'failed'; receipt.failure = error.message; receipt.elapsed_ms = Math.round(performance.now() - started);
    await save(); throw error;
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
