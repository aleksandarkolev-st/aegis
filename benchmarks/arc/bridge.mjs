import https from 'node:https';
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { randomUUID, createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const origin = 'https://three.arcprize.org';
const maximumReply = 2 * 1024 * 1024;
const states = new Set(['NOT_PLAYED', 'NOT_FINISHED', 'WIN', 'GAME_OVER']);
const identifier = (value) => typeof value === 'string' && /^[a-zA-Z0-9_-]{1,128}$/.test(value);

export function save(filename, value) {
  const temporary = `${filename}.${randomUUID()}.tmp`;
  const descriptor = fs.openSync(temporary, 'wx', 0o600);
  try {
    fs.writeFileSync(descriptor, JSON.stringify(value));
    fs.fsyncSync(descriptor);
  } finally {
    fs.closeSync(descriptor);
  }
  fs.renameSync(temporary, filename);
  if (process.platform !== 'win32') {
    const directory = fs.openSync(path.dirname(filename), 'r');
    try { fs.fsyncSync(directory); } finally { fs.closeSync(directory); }
  }
}

function append(filename, value) {
  const descriptor = fs.openSync(filename, 'a', 0o600);
  try {
    fs.writeFileSync(descriptor, `${JSON.stringify(value)}\n`);
    fs.fsyncSync(descriptor);
  } finally {
    fs.closeSync(descriptor);
  }
}

export function request(route, body, credentials) {
  if (!/^\/api\/(games(?:\/anonkey)?|scorecard\/(?:open|close)|cmd\/(?:RESET|ACTION[1-7]))$/.test(route)) {
    throw new Error('ARC route is not approved');
  }
  return new Promise((resolve, reject) => {
    const headers = { Accept: 'application/json', 'Accept-Encoding': 'identity' };
    if (credentials.key) headers['X-API-Key'] = credentials.key;
    if (credentials.cookies.length) headers.Cookie = credentials.cookies.join('; ');
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    const operation = https.request(`${origin}${route}`, { method: body === undefined ? 'GET' : 'POST', headers }, (response) => {
      const chunks = [];
      let length = 0;
      if (response.headers['content-encoding'] && response.headers['content-encoding'] !== 'identity') {
        operation.destroy(new Error('ARC compression is not accepted'));
        return;
      }
      response.on('data', (chunk) => {
        length += chunk.length;
        if (length > maximumReply) operation.destroy(new Error('ARC reply exceeds limit'));
        else chunks.push(chunk);
      });
      response.on('error', reject);
      response.on('end', () => {
        if (response.statusCode !== 200) return reject(new Error(`ARC HTTP ${response.statusCode}; no automatic retry`));
        try {
          resolve({ data: JSON.parse(Buffer.concat(chunks).toString('utf8')), bytes: length, cookies: response.headers['set-cookie'] ?? [] });
        } catch {
          reject(new Error('ARC response is not valid JSON; no automatic retry'));
        }
      });
    });
    const deadline = setTimeout(() => operation.destroy(new Error('ARC request deadline; no automatic retry')), 15_000);
    operation.once('close', () => clearTimeout(deadline));
    operation.on('error', reject);
    operation.end(body === undefined ? undefined : JSON.stringify(body));
  });
}

export function mergeCookies(previous, incoming) {
  const cookies = new Map(previous.map((cookie) => cookie.split('=').slice(0, 1).concat(cookie.slice(cookie.indexOf('=') + 1))));
  for (const cookie of incoming) {
    if (typeof cookie !== 'string' || cookie.length > 8192 || /[\r\n]/.test(cookie)) throw new Error('Invalid ARC cookie');
    const pair = cookie.split(';')[0];
    const separator = pair.indexOf('=');
    const name = pair.slice(0, separator);
    const value = pair.slice(separator + 1);
    if (separator < 1 || !/^[a-zA-Z0-9_-]+$/.test(name) || /[;\s]/.test(value)) throw new Error('Invalid ARC cookie');
    if (/;\s*max-age=0(?:;|$)/i.test(cookie)) cookies.delete(name);
    else cookies.set(name, value);
  }
  if (cookies.size > 32) throw new Error('ARC cookie limit');
  return [...cookies].map(([name, value]) => `${name}=${value}`);
}

export function validateFrame(frame, expectedGame, expectedGuid) {
  if (!frame || frame.game_id !== expectedGame || !identifier(frame.guid) || (expectedGuid && frame.guid !== expectedGuid)
    || !states.has(frame.state) || !Number.isInteger(frame.levels_completed) || frame.levels_completed < 0
    || !Number.isInteger(frame.win_levels) || frame.win_levels < frame.levels_completed
    || !Array.isArray(frame.available_actions) || frame.available_actions.length > 7
    || frame.available_actions.some((action) => !Number.isInteger(action) || action < 1 || action > 7)
    || !Array.isArray(frame.frame) || frame.frame.length < 1 || frame.frame.length > 128) {
    throw new Error('ARC frame metadata is invalid');
  }
  for (const grid of frame.frame) {
    if (!Array.isArray(grid) || grid.length !== 64 || grid.some((row) => !Array.isArray(row) || row.length !== 64
      || row.some((pixel) => !Number.isInteger(pixel) || pixel < 0 || pixel > 15))) throw new Error('ARC grid is invalid');
  }
  return frame;
}

export function observation(state) {
  if (!state.frame) return { state: 'NOT_PLAYED', uncertain: Boolean(state.pending), moves_reserved: state.moves };
  return {
    game_id: state.game_id, state: state.frame.state, levels_completed: state.frame.levels_completed,
    win_levels: state.frame.win_levels, available_actions: state.frame.available_actions,
    moves_reserved: state.moves, move_limit: state.move_limit, uncertain: Boolean(state.pending),
    encoding: '64 rows of 64 hexadecimal palette indices; x=column, y=row; latest animation frame only',
    rows: state.frame.frame.at(-1).map((row) => row.map((pixel) => pixel.toString(16)).join('')),
  };
}

export class Arcade {
  constructor(directory, transport = request) {
    this.directory = directory;
    this.transport = transport;
  }

  async locked(callback) {
    const lock = path.join(this.directory, 'request.lock');
    try { fs.mkdirSync(lock); } catch { throw new Error('ARC session locked; inspect crash evidence, do not replay moves'); }
    try {
      const state = JSON.parse(fs.readFileSync(path.join(this.directory, 'state.json'), 'utf8'));
      return await callback(state);
    } finally {
      fs.rmdirSync(lock);
    }
  }

  persist(state) { save(path.join(this.directory, 'state.json'), state); }

  async exchange(state, route, body, validator, apply = () => {}) {
    if (state.pending || state.closed) throw new Error('ARC session closed or move outcome uncertain; no automatic retry');
    if (state.calls >= 10_000 || state.bytes + maximumReply > state.byte_limit) throw new Error('ARC request or retained-body budget exhausted');
    state.calls += 1;
    state.pending = { sequence: state.calls, route, body, started_at: Date.now() };
    this.persist(state);
    append(path.join(this.directory, 'requests.jsonl'), { kind: 'request.started', ...state.pending });
    let credentials = { key: '', cookies: [] };
    if (fs.existsSync(path.join(this.directory, 'credentials.json'))) {
      credentials = JSON.parse(fs.readFileSync(path.join(this.directory, 'credentials.json'), 'utf8'));
    }
    const reply = await this.transport(route, body, credentials);
    if (!Number.isInteger(reply.bytes) || reply.bytes < 0 || reply.bytes > maximumReply) throw new Error('ARC reply exceeds limit');
    const data = validator(reply.data);
    credentials.cookies = mergeCookies(credentials.cookies, reply.cookies);
    if (route === '/api/games/anonkey') {
      credentials.key = data.api_key;
      save(path.join(this.directory, 'credentials.json'), credentials);
    } else {
      save(path.join(this.directory, 'credentials.json'), credentials);
      const secrets = [credentials.key, ...credentials.cookies.map((cookie) => cookie.slice(cookie.indexOf('=') + 1))].filter(Boolean);
      let serialized = JSON.stringify(data);
      for (const secret of secrets) serialized = serialized.replaceAll(secret, '[redacted]');
      save(path.join(this.directory, `response-${state.calls}.json`), JSON.parse(serialized));
    }
    apply(data);
    state.bytes += reply.bytes;
    state.pending = null;
    this.persist(state);
    append(path.join(this.directory, 'requests.jsonl'), { kind: 'request.finished', sequence: state.calls, route, bytes: reply.bytes, finished_at: Date.now() });
    return data;
  }

  async initialize(competition = false) {
    return this.locked(async (state) => {
      if (state.card_id) throw new Error('ARC scorecard already opened');
      const key = process.env.ARC_API_KEY;
      if (key) {
        if (key.length > 4096 || /[\r\n]/.test(key)) throw new Error('Invalid ARC credential');
        save(path.join(this.directory, 'credentials.json'), { key, cookies: [] });
      } else {
        await this.exchange(state, '/api/games/anonkey', undefined, (data) => {
          if (typeof data?.api_key !== 'string' || !data.api_key.length || data.api_key.length > 4096 || /[\r\n]/.test(data.api_key)) throw new Error('ARC anonymous key missing');
          return data;
        });
      }
      const games = await this.exchange(state, '/api/games', undefined, (data) => {
        if (!Array.isArray(data) || data.length < 1 || data.length > 1000 || data.some((game) => !identifier(game.game_id) || !game.game_id.includes('-'))
          || new Set(data.map((game) => game.game_id)).size !== data.length) throw new Error('Invalid versioned ARC game registry');
        return data;
      }, (data) => { state.games = data; });
      const opened = await this.exchange(state, '/api/scorecard/open', {
        tags: ['aegis-recorded-evaluation'], opaque: { kind: 'aegis-arc-agi-3', competition_mode: competition }, ...(competition ? { competition_mode: true } : {}),
      }, (data) => {
        if (!identifier(data?.card_id)) throw new Error('ARC scorecard ID missing');
        return data;
      }, (data) => { state.card_id = data.card_id; state.competition = competition; });
      state.competition = competition;
      this.persist(state);
      return games;
    });
  }

  async start(game) {
    return this.locked(async (state) => {
      if (state.pending || state.closed || !state.card_id || !state.games.some((entry) => entry.game_id === game) || state.played.includes(game)) throw new Error('ARC game unavailable or already played');
      state.played.push(game);
      state.game_id = game;
      state.guid = null;
      state.frame = null;
      state.moves = 0;
      this.persist(state);
      await this.exchange(state, '/api/cmd/RESET', { game_id: game, card_id: state.card_id }, (data) => validateFrame(data, game), (data) => { state.guid = data.guid; state.frame = data; });
      return observation(state);
    });
  }

  async act(arguments_) {
    return this.locked(async (state) => {
      const action = arguments_?.action;
      if (state.pending || state.closed || !state.guid || state.frame.state === 'WIN' || state.moves >= state.move_limit
        || !/^ACTION[1-7]$|^RESET$/.test(action ?? '') || Object.keys(arguments_).some((name) => !['action', 'x', 'y'].includes(name))) throw new Error('ARC move is unavailable or exhausted');
      if (action === 'RESET') {
        if (state.frame.state !== 'GAME_OVER') throw new Error('ARC reset only allowed after GAME_OVER');
      } else if (!state.frame.available_actions.includes(Number(action.slice(-1))) || state.frame.state === 'GAME_OVER') {
        throw new Error('ARC action not currently available');
      }
      if (action === 'ACTION6') {
        if (![arguments_.x, arguments_.y].every((coordinate) => Number.isInteger(coordinate) && coordinate >= 0 && coordinate < 64)) throw new Error('ARC click requires x/y in 0..63');
      } else if (arguments_.x !== undefined || arguments_.y !== undefined) throw new Error('Coordinates only belong to ACTION6');
      state.moves += 1;
      this.persist(state);
      await this.exchange(state, `/api/cmd/${action}`, {
        game_id: state.game_id, guid: state.guid,
        ...(action === 'RESET' ? { card_id: state.card_id } : {}),
        ...(action === 'ACTION6' ? { x: arguments_.x, y: arguments_.y } : {}),
      }, (data) => validateFrame(data, state.game_id, state.guid), (data) => { state.frame = data; });
      return observation(state);
    });
  }

  async observe() { return this.locked(async (state) => observation(state)); }

  async close() {
    return this.locked(async (state) => {
      if (!state.card_id) throw new Error('ARC scorecard not opened');
      const scorecard = await this.exchange(state, '/api/scorecard/close', { card_id: state.card_id }, (data) => {
        if (!data || typeof data !== 'object' || Array.isArray(data) || data.card_id !== state.card_id || !Number.isFinite(data.score) || data.score < 0 || data.score > 100) throw new Error('ARC scorecard score missing or invalid');
        return data;
      }, () => { state.closed = true; });
      save(path.join(this.directory, 'scorecard.json'), scorecard);
      state.closed = true;
      this.persist(state);
      return scorecard;
    });
  }
}

const tools = [
  { name: 'observe', description: 'Inspect the latest cached ARC-AGI-3 frame. No network or extra move; hexadecimal rows are lossless palette indices.', inputSchema: { type: 'object', properties: {}, additionalProperties: false } },
  { name: 'act', description: 'Take one available ARC-AGI-3 action. ACTION6 needs x/y coordinates 0..63. RESET only after GAME_OVER. Uncertain moves are never replayed.', inputSchema: { type: 'object', properties: { action: { type: 'string', enum: ['ACTION1', 'ACTION2', 'ACTION3', 'ACTION4', 'ACTION5', 'ACTION6', 'ACTION7', 'RESET'] }, x: { type: 'integer', minimum: 0, maximum: 63 }, y: { type: 'integer', minimum: 0, maximum: 63 } }, required: ['action'], additionalProperties: false } },
];

export function serve(arcade, input = process.stdin, output = process.stdout) {
  let lineBytes = 0;
  input.on('data', (chunk) => {
    for (const byte of Buffer.from(chunk)) {
      lineBytes = byte === 10 ? 0 : lineBytes + 1;
      if (lineBytes > 32768) { input.destroy(); break; }
    }
  });
  const lines = readline.createInterface({ input, crlfDelay: Infinity });
  let queued = Promise.resolve();
  let buffered = 0;
  lines.on('line', (line) => {
    buffered += Buffer.byteLength(line);
    if (Buffer.byteLength(line) > 32768 || buffered > 65536) { lines.close(); input.destroy(); return; }
    queued = queued.then(async () => {
      let message;
      try { message = JSON.parse(line); } catch { return; }
      if (message.id === undefined) return;
      const send = (result) => output.write(`${JSON.stringify({ jsonrpc: '2.0', id: message.id, result })}\n`);
      if (message.method === 'initialize') send({ protocolVersion: message.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: 'aegis-arc-agi-3', version: '1.0.0' } });
      else if (message.method === 'tools/list') send({ tools });
      else if (message.method === 'tools/call') {
        try {
          const result = message.params.name === 'observe' ? await arcade.observe()
            : message.params.name === 'act' ? await arcade.act(message.params.arguments) : null;
          if (!result) throw new Error('Unknown ARC tool');
          send({ content: [{ type: 'text', text: JSON.stringify(result) }], isError: false });
        } catch {
          send({ content: [{ type: 'text', text: 'ARC action unavailable, budget exhausted or outcome uncertain. Inspect recording; never retry an uncertain move.' }], isError: true });
        }
      } else output.write(`${JSON.stringify({ jsonrpc: '2.0', id: message.id, error: { code: -32601, message: 'Method not found' } })}\n`);
    }).catch(() => {}).finally(() => { buffered -= Buffer.byteLength(line); });
  });
  return lines;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const directory = process.argv[2];
  if (!directory || !fs.existsSync(path.join(directory, 'authorized.json'))) throw new Error('ARC evaluation has not passed the readiness gate');
  const authorization = JSON.parse(fs.readFileSync(path.join(directory, 'authorized.json'), 'utf8'));
  if (authorization.bridge_sha256 !== createHash('sha256').update(fs.readFileSync(fileURLToPath(import.meta.url))).digest('hex')) throw new Error('ARC bridge changed after readiness review');
  const arcade = new Arcade(directory);
  if (process.argv[3] === '--controller') {
    const command = process.argv[4];
    const result = command === 'initialize' ? await arcade.initialize(process.argv[5] === 'competition')
      : command === 'start' ? await arcade.start(process.argv[5])
      : command === 'close' ? await arcade.close() : null;
    if (!result) throw new Error('Unknown ARC controller operation');
    process.stdout.write(`${JSON.stringify(result)}\n`);
  } else serve(arcade);
}
