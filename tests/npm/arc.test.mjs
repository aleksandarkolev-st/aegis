import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { Arcade, mergeCookies, observation, save, validateFrame, request } from '../../benchmarks/arc/bridge.mjs';

const frame = (state = 'NOT_FINISHED') => ({ game_id: 'test-012345678abc', guid: 'session-one', state, levels_completed: state === 'WIN' ? 1 : 0, win_levels: 1, available_actions: [1, 6], frame: [Array.from({ length: 64 }, () => Array(64).fill(0))] });

async function fixture(callback) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'aegis-arc-'));
  save(path.join(directory, 'state.json'), { schema: 1, moves: 0, move_limit: 3, calls: 0, bytes: 0, byte_limit: 30 * 1024 * 1024, pending: null, played: [], closed: false });
  try { await callback(directory); } finally { fs.rmSync(directory, { recursive: true, force: true }); }
}

test('ARC frames preserve exact grid coordinates and reject malformed input', () => {
  const data = frame();
  data.frame[0][4][7] = 15;
  validateFrame(data, data.game_id);
  assert.equal(observation({ frame: data, game_id: data.game_id, moves: 0 }).rows[4][7], 'f');
  assert.throws(() => validateFrame(data, 'another-game'));
  data.frame[0][4][7] = 16;
  assert.throws(() => validateFrame(data, data.game_id));
  assert.throws(() => validateFrame({ ...frame(), available_actions: [8] }, data.game_id));
});

test('ARC session affinity updates are bounded and are never sent to other URLs', () => {
  assert.deepEqual(mergeCookies(['AWSALB=old'], ['AWSALB=new; Path=/; Secure', 'AWSALBCORS=second; Path=/']), ['AWSALB=new', 'AWSALBCORS=second']);
  assert.deepEqual(mergeCookies(['AWSALB=old'], ['AWSALB=; Max-Age=0']), []);
  assert.throws(() => mergeCookies([], ['bad=hello\r\nHeader: stolen']));
  assert.throws(() => request('https://example.com', undefined, { key: 'secret', cookies: [] }));
});

test('ARC restart uses saved guid/cookies and preserves official score, without real API calls', async () => fixture(async (directory) => {
  const calls = [];
  const transport = async (route, body, credentials) => {
    calls.push({ route, body, credentials });
    const data = route.endsWith('anonkey') ? { api_key: 'fixture-private-key' }
      : route === '/api/games' ? [{ game_id: frame().game_id, title: 'Fixture' }]
      : route.endsWith('/open') ? { card_id: 'fixture-card' }
      : route.endsWith('/close') ? { card_id: 'fixture-card', score: 12.345, environments: [{ actions: 1 }] }
      : frame(route.endsWith('ACTION6') ? 'WIN' : 'NOT_FINISHED');
    return { data, bytes: JSON.stringify(data).length, cookies: ['AWSALB=fixture-affinity; Path=/'] };
  };
  const previous = process.env.ARC_API_KEY;
  delete process.env.ARC_API_KEY;
  try { await new Arcade(directory, transport).initialize(); } finally { if (previous !== undefined) process.env.ARC_API_KEY = previous; }
  await new Arcade(directory, transport).start(frame().game_id);
  await assert.rejects(() => new Arcade(directory, transport).act({ action: 'ACTION6', x: 64, y: 0 }));
  await assert.rejects(() => new Arcade(directory, transport).act({ action: 'RESET' }));
  assert.equal(calls.length, 4);
  const result = await new Arcade(directory, transport).act({ action: 'ACTION6', x: 7, y: 4 });
  assert.equal(result.state, 'WIN');
  assert.equal(calls[4].body.guid, 'session-one');
  assert.equal(calls[4].credentials.cookies[0], 'AWSALB=fixture-affinity');
  await assert.rejects(() => new Arcade(directory, transport).act({ action: 'ACTION1' }));
  const scorecard = await new Arcade(directory, transport).close();
  assert.equal(scorecard.score, 12.345);
  assert.equal(JSON.parse(fs.readFileSync(path.join(directory, 'scorecard.json'))).score, 12.345);
  const logs = fs.readdirSync(directory).filter((name) => name !== 'credentials.json').map((name) => fs.readFileSync(path.join(directory, name), 'utf8')).join('');
  assert.ok(!logs.includes('fixture-private-key'));
  assert.ok(!logs.includes('fixture-affinity'));
}));

test('ARC uncertain response keeps durable intent and is never automatically replayed', async () => fixture(async (directory) => {
  const current = JSON.parse(fs.readFileSync(path.join(directory, 'state.json')));
  Object.assign(current, { game_id: frame().game_id, guid: frame().guid, frame: frame(), card_id: 'card', games: [{ game_id: frame().game_id }] });
  save(path.join(directory, 'state.json'), current);
  let requests = 0;
  const transport = async () => { requests += 1; throw new Error('lost reply after server action'); };
  await assert.rejects(() => new Arcade(directory, transport).act({ action: 'ACTION1' }));
  const saved = JSON.parse(fs.readFileSync(path.join(directory, 'state.json')));
  assert.equal(saved.pending.route, '/api/cmd/ACTION1');
  assert.equal(saved.moves, 1);
  assert.equal((await new Arcade(directory, transport).observe()).uncertain, true);
  await assert.rejects(() => new Arcade(directory, transport).act({ action: 'ACTION1' }));
  assert.equal(requests, 1);
  assert.equal(JSON.parse(fs.readFileSync(path.join(directory, 'state.json'))).moves, 1);
}));

test('ARC stale process locks and body budgets fail closed before transport', async () => fixture(async (directory) => {
  fs.mkdirSync(path.join(directory, 'request.lock'));
  const arcade = new Arcade(directory, async () => { throw new Error('must not execute'); });
  await assert.rejects(() => arcade.observe(), /locked/);
  fs.rmdirSync(path.join(directory, 'request.lock'));
  const current = JSON.parse(fs.readFileSync(path.join(directory, 'state.json')));
  current.byte_limit = 1;
  save(path.join(directory, 'state.json'), current);
  await assert.rejects(() => arcade.initialize(), /budget/);
  assert.equal(JSON.parse(fs.readFileSync(path.join(directory, 'state.json'))).calls, 0);
}));

test('ARC malformed replies retain redacted failure evidence and do not clear intent', async () => fixture(async (directory) => {
  const current = JSON.parse(fs.readFileSync(path.join(directory, 'state.json')));
  Object.assign(current, { game_id: frame().game_id, guid: frame().guid, frame: frame(), card_id: 'card' });
  save(path.join(directory, 'state.json'), current);
  save(path.join(directory, 'credentials.json'), { key: 'fixture-"private-key', cookies: ['AWSALB=private-cookie'] });
  const transport = async () => ({ data: { diagnostic: 'fixture-"private-key private-cookie', frame: [] }, bytes: 100, cookies: [] });
  await assert.rejects(() => new Arcade(directory, transport).act({ action: 'ACTION1' }));
  const evidence = fs.readFileSync(path.join(directory, 'response-1.json'), 'utf8');
  assert.ok(!evidence.includes('private-key'));
  assert.ok(!evidence.includes('private-cookie'));
  assert.ok(JSON.parse(evidence).diagnostic.includes('[redacted]'));
  assert.equal(JSON.parse(fs.readFileSync(path.join(directory, 'state.json'))).pending.route, '/api/cmd/ACTION1');
}));
