// Explicit hosted diagnostic, isolated from the user's README and saved tasks.
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import path from 'node:path';
const root = process.cwd();
const binary = path.resolve(process.argv[2] ?? 'target/debug/arun.exe');
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
assert.ok(process.argv.includes('--live'), 'Explicit --live is required for hosted inference');
const directory = await mkdtemp(path.join(root, '.arun', 'readme-edit-smoke-'));
const workspace = path.join(directory, 'workspace');
await mkdir(workspace);
const original = JSON.parse(await readFile(path.join(root, '.arun/artifacts/2f8f601e09db81159347c3fc5295dce30b25015ea171c8f3a4133e38ae8abb2f'), 'utf8')).content;
assert.equal(typeof original, 'string');
await writeFile(path.join(workspace, 'README.md'), original);
const fixtures = {};
for (const file of ['Cargo.toml', 'package.json']) {
  const bytes = await readFile(path.join(root, file));
  fixtures[file] = sha(bytes);
  await writeFile(path.join(workspace, file), bytes);
}
await writeFile(path.join(workspace, 'scopes.json'), JSON.stringify({read:['README.md','Cargo.toml','package.json'],write:['README.md']}));
const receipt = {started_at:new Date().toISOString(),phase:'running',directory,binary,binary_sha256:sha(await readFile(binary)),source_readme_sha256:sha(Buffer.from(original)),model:'gpt-6-luna',reasoning_effort:'high',fixtures};
await writeFile(path.join(directory,'receipt.json'), JSON.stringify(receipt,null,2));
console.log('Hosted README edit diagnostic: ' + directory);
const task = 'Rewrite the README in this project since today it is mostly notes. Make it a concise, accurate user-facing project introduction and setup/usage guide. Preserve supported behavior and limitations; keep implementation audit notes out of the main guide. Edit README.md only. Read relevant available manifests if needed.';
const child = spawn(binary,['run',task,'--provider','chatgpt','--model',receipt.model,'--reasoning','high','--mode','durable','--allow-write','--filesystem-scopes','scopes.json','--wall-seconds','360','--foreground'],{cwd:workspace,windowsHide:true,stdio:['ignore','pipe','pipe']});
let stdout='',stderr='';
child.stdout.on('data',chunk=>{stdout+=chunk;}); child.stderr.on('data',chunk=>{stderr+=chunk;});
const timer=setTimeout(()=>child.kill(),390000);
try {
  receipt.runner_exit_code=await new Promise((resolve,reject)=>{child.once('error',reject);child.once('close',resolve);});
} finally {clearTimeout(timer);}
await writeFile(path.join(directory,'stdout.log'),stdout); await writeFile(path.join(directory,'stderr.log'),stderr);
const runId=stdout.match(/run ([0-9a-f-]{36})/)?.[1];
receipt.run_id=runId;
try {
  assert.equal(receipt.runner_exit_code,0);
  assert.ok(runId,'Missing run identity');
  const replay=execFileSync(binary,['replay',runId],{cwd:workspace,windowsHide:true,encoding:'utf8',maxBuffer:32*1024*1024});
  const events=replay.trim().split(/\r?\n/).map(line=>JSON.parse(line));
  await writeFile(path.join(directory,'events.json'),JSON.stringify(events,null,2));
  receipt.model_attempts=events.filter(e=>e.kind==='model.started').length;
  receipt.rejected_actions=events.filter(e=>e.kind==='action.rejected').length;
  receipt.reported_tokens=events.filter(e=>e.kind==='model.response'||e.kind==='model.failed').reduce((n,e)=>n+(e.payload.usage?.input_tokens??0)+(e.payload.usage?.output_tokens??0),0);
  assert.ok(events.some(e=>e.kind==='run.completed'),'Task did not actually complete');
  assert.equal(receipt.rejected_actions,0,'Read/protocol rejections remain');
  const rewritten=await readFile(path.join(workspace,'README.md'),'utf8');
  assert.notEqual(rewritten,original,'No README edit');
  assert.ok(rewritten.length>500&&rewritten.length<original.length,'Expected a usable, shorter guide');
  assert.match(rewritten,/aegis/i); assert.match(rewritten,/npm|cargo/i);
  assert.match(rewritten,/publish|publication/i, 'Preserve the original publication limitation');
  for (const [file,digest] of Object.entries(fixtures)) assert.equal(sha(await readFile(path.join(workspace,file))),digest,'Manifest changed');
  assert.equal(sha(await readFile(binary)),receipt.binary_sha256,'Runtime changed');
  receipt.phase='completed-isolated-readme-diagnostic';
  receipt.final_readme_sha256=sha(Buffer.from(rewritten));
  receipt.final_readme_characters=Array.from(rewritten).length;
  receipt.metrics=execFileSync(binary,['metrics',runId],{cwd:workspace,windowsHide:true,encoding:'utf8'});
} catch(error) {receipt.phase='failed-diagnostic';receipt.error=error.message;process.exitCode=1;}
receipt.finished_at=new Date().toISOString();
await writeFile(path.join(directory,'receipt.json'),JSON.stringify(receipt,null,2));
console.log(JSON.stringify(receipt));
