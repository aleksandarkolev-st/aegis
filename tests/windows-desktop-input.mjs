// Explicit manual check: this opens one owned scratch window and briefly uses input.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { callTool } from '../scripts/windows-host.mjs';

if (process.platform !== 'win32' || !process.argv.includes('--visible-test-confirmed')) {
  throw new Error('Run on Windows with --visible-test-confirmed after announcing the scratch-window test.');
}
const root = fileURLToPath(new URL('../', import.meta.url));
const directory = path.join(root, '.arun', `desktop-input-${randomUUID()}`);
await fs.mkdir(directory, { recursive: true });
const stateFile = path.join(directory, 'input.json');
const script = path.join(directory, 'window.ps1');
const title = `Aegis input test ${randomUUID()}`;
const value = result => JSON.parse(result.content.find(item => item.type === 'text').text);
const windows = async () => value(await callTool('desktop_windows', {}, root)).windows;
const before = (await windows()).find(window => window.foreground);
const pointer = JSON.parse(value(await callTool('powershell', {
  script: '$taskPoint=[Windows.Forms.Cursor]::Position; [Console]::WriteLine((@{x=$taskPoint.X;y=$taskPoint.Y}|ConvertTo-Json -Compress))',
}, root)).stdout.trim());
await fs.writeFile(script, `param([string]$OutputFile,[string]$Title)
$ErrorActionPreference='Stop'
trap { [IO.File]::WriteAllText($OutputFile+'.error',($_|Out-String)); exit 1 }
[IO.File]::WriteAllText($OutputFile+'.started','started')
Add-Type -AssemblyName System.Windows.Forms
$form = [Windows.Forms.Form]::new()
$form.Text=$Title; $form.Width=480; $form.Height=180; $form.TopMost=$true
$form.StartPosition='CenterScreen'
$box=[Windows.Forms.TextBox]::new(); $box.Multiline=$true; $box.Dock='Fill'; $box.Font=[Drawing.Font]::new('Segoe UI',14)
$form.Controls.Add($box); $box.Text='replace this text'
$script:clicked=$false; $script:chord=$false
$box.add_MouseDown({$script:clicked=$true})
$box.add_KeyDown({param($sender,$event) if($event.Control -and $event.KeyCode -eq 'A'){$script:chord=$true;$box.SelectAll();$event.SuppressKeyPress=$true}})
$timer=[Windows.Forms.Timer]::new(); $timer.Interval=100
$started=[DateTime]::UtcNow
$timer.add_Tick({
  $data=@{text=$box.Text;clicked=$script:clicked;chord=$script:chord}
  [IO.File]::WriteAllText($OutputFile,($data|ConvertTo-Json -Compress),[Text.UTF8Encoding]::new($false))
  if(([DateTime]::UtcNow-$started).TotalSeconds -gt 60){$form.Close()}
})
$form.add_Shown({$box.Focus();$timer.Start()})
try{$null=$form.ShowDialog()}finally{$timer.Stop();$timer.Dispose();$form.Dispose()}
`, 'utf8');
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
let pid;
try {
  const quote = text => "'" + text.replaceAll("'", "''") + "'";
  const launch = `& ${quote(script)} -OutputFile ${quote(stateFile)} -Title ${quote(title)}`;
  const launched = value(await callTool('app_launch', {
    executable: path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe'),
    args: ['-NoProfile', '-STA', '-ExecutionPolicy', 'Bypass', '-WindowStyle', 'Hidden', '-EncodedCommand', Buffer.from(launch, 'utf16le').toString('base64')],
    visible: true,
  }, root));
  pid = launched.pid;
  console.log(`Owned scratch launch PID: ${pid}`);
  let own;
  for (let attempt = 0; attempt < 40; attempt++) {
    own = (await windows()).find(window => window.process_id === pid && window.title === title);
    if (own) break;
    await pause(100);
  }
  const launchError = await fs.readFile(stateFile + '.error', 'utf8').catch(() => '');
  assert.ok(own, `The launched scratch app must expose its own visible window: ${launchError}`);
  const rectangle = own.rectangle;
  await callTool('desktop_mouse', { action: 'click', x: Math.round((rectangle.Left + rectangle.Right) / 2), y: Math.round((rectangle.Top + rectangle.Bottom) / 2) }, root);
  assert.equal((await windows()).find(window => window.foreground)?.handle, own.handle);
  await callTool('desktop_focus', { handle: own.handle }, root);
  assert.equal((await windows()).find(window => window.foreground)?.handle, own.handle);
  await callTool('desktop_keyboard', { keys: ['CTRL', 'A'] }, root);
  const expected = 'Aegis desktop 日本語 e\u0301 🙂';
  await callTool('desktop_keyboard', { text: expected }, root);
  let observed;
  for (let attempt = 0; attempt < 30; attempt++) {
    try { observed = JSON.parse(await fs.readFile(stateFile, 'utf8')); } catch {}
    if (observed?.text === expected && observed.clicked && observed.chord) break;
    await pause(100);
  }
  assert.deepEqual(observed, { text: expected, clicked: true, chord: true });
  await fs.writeFile(path.join(directory, 'receipt.json'), JSON.stringify({ passed: true, checks: ['actual app launch', 'window focus', 'mouse click', 'Ctrl+A chord', 'Unicode keyboard input'], pid }, null, 2));
  console.log(`Native desktop input passed. Receipt: ${path.join(directory, 'receipt.json')}`);
} finally {
  if (pid) {
    // The PID was created by this test; kill only while its unique window is still present.
    if ((await windows()).some(window => window.process_id === pid && window.title === title)) {
      await callTool('powershell', { script: `Stop-Process -Id ${pid} -ErrorAction SilentlyContinue` }, root);
    }
  }
  if (before && (await windows()).some(window => window.handle === before.handle)) {
    await callTool('desktop_focus', { handle: before.handle }, root).catch(error => console.error(`Focus restore: ${error.message}`));
  }
  await callTool('desktop_mouse', { action: 'move', x: pointer.x, y: pointer.y }, root);
}
