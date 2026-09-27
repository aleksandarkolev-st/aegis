import assert from 'node:assert/strict';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export async function grade(task, implementation) {
  const cases = [];
  const check = async (id, execute) => {
    try {
      await execute();
      cases.push({ id, passed: true });
    } catch (error) {
      cases.push({ id, passed: false, diagnostic: String(error.message ?? error).slice(0, 1200) });
    }
  };
  if (task === 'json-patch') {
    const { applyPatch } = implementation;
    await check('nested-escaped-pointers', () => assert.deepEqual(applyPatch({ 'a/b': { '~key': 1 } }, [{ op: 'replace', path: '/a~1b/~0key', value: 2 }]), { 'a/b': { '~key': 2 } }));
    await check('array-insert-append-remove', () => assert.deepEqual(applyPatch({ items: [1, 3] }, [{ op: 'add', path: '/items/1', value: 2 }, { op: 'add', path: '/items/-', value: 4 }, { op: 'remove', path: '/items/0' }]), { items: [2, 3, 4] }));
    await check('move-uses-post-removal-index', () => assert.deepEqual(applyPatch({ items: ['a', 'b', 'c'] }, [{ op: 'move', from: '/items/0', path: '/items/2' }]), { items: ['b', 'c', 'a'] }));
    await check('copy-and-result-do-not-alias-input', () => {
      const source = { original: { value: 1 } };
      const result = applyPatch(source, [{ op: 'copy', from: '/original', path: '/copy' }]);
      result.copy.value = 2;
      assert.equal(result.original.value, 1);
      assert.deepEqual(source, { original: { value: 1 } });
    });
    await check('root-and-key-order-test', () => assert.deepEqual(applyPatch({ one: 1 }, [{ op: 'replace', path: '', value: { a: 1, b: 2 } }, { op: 'test', path: '', value: { b: 2, a: 1 } }]), { a: 1, b: 2 }));
    await check('failed-sequence-leaves-input-unchanged', () => {
      const source = { value: 1 };
      assert.throws(() => applyPatch(source, [{ op: 'replace', path: '/value', value: 2 }, { op: 'test', path: '/value', value: 3 }]));
      assert.deepEqual(source, { value: 1 });
    });
    await check('reject-invalid-paths-and-operations', () => {
      for (const operation of [{ op: 'replace', path: '/missing', value: 1 }, { op: 'add', path: '/items/01', value: 1 }, { op: 'remove', path: '/items/-' }, { op: 'unknown', path: '/value' }, { op: 'add', path: '/value' }, { op: 'move', from: '/parent', path: '/parent/child' }, { op: 'add', path: '/__proto__/polluted', value: true }]) assert.throws(() => applyPatch({ items: [], parent: {}, value: 0 }, [operation]));
    });
  } else if (task === 'dag-scheduler') {
    const { runGraph } = implementation;
    await check('dependency-order-and-values', async () => {
      const calls = [];
      const result = await runGraph([{ id: 'leaf', deps: ['root'] }, { id: 'root', deps: [] }], 2, async (id, values) => { calls.push(id); return id === 'root' ? 7 : values.root + 1; });
      assert.deepEqual(calls, ['root', 'leaf']);
      assert.equal(result.leaf.value, 8);
    });
    await check('concurrency-and-ready-order', async () => {
      let active = 0;
      let peak = 0;
      const calls = [];
      const result = await runGraph(['a', 'b', 'c', 'd'].map(id => ({ id, deps: [] })), 2, async id => { calls.push(id); active += 1; peak = Math.max(peak, active); await new Promise(resolve => setTimeout(resolve, 10)); active -= 1; return id; });
      assert.deepEqual(calls, ['a', 'b', 'c', 'd']);
      assert.equal(peak, 2);
      assert.equal(Object.keys(result).length, 4);
    });
    await check('failure-blocks-descendants-not-independent-work', async () => {
      const calls = [];
      const result = await runGraph([{ id: 'bad', deps: [] }, { id: 'child', deps: ['bad'] }, { id: 'grandchild', deps: ['child'] }, { id: 'good', deps: [] }], 2, id => { calls.push(id); if (id === 'bad') throw new Error('failure'); return Promise.resolve(9); });
      assert.deepEqual(calls, ['bad', 'good']);
      assert.equal(result.bad.status, 'failed');
      assert.equal(typeof result.bad.error, 'string');
      assert.equal(result.child.status, 'blocked');
      assert.equal(result.grandchild.status, 'blocked');
      assert.equal(result.good.value, 9);
    });
    await check('all-validation-before-side-effects', async () => {
      const invalid = [[{ id: 'ok', deps: [] }, { id: 'cycle', deps: ['cycle'] }], [{ id: 'one', deps: [] }, { id: 'one', deps: [] }], [{ id: 'one', deps: ['missing'] }], [{ id: 'one', deps: ['two', 'two'] }, { id: 'two', deps: [] }]];
      for (const nodes of invalid) { let calls = 0; await assert.rejects(() => runGraph(nodes, 2, () => { calls += 1; return 1; })); assert.equal(calls, 0); }
      for (const limit of [0, -1, 1.5, NaN]) await assert.rejects(() => runGraph([], limit, () => 1));
    });
    await check('prototype-safe-and-input-preserving', async () => {
      const nodes = [{ id: '__proto__', deps: [] }, { id: 'child', deps: ['__proto__'] }];
      const snapshot = structuredClone(nodes);
      const result = await runGraph(nodes, 1, (id, values) => id === '__proto__' ? 5 : values.__proto__ + 1);
      assert.equal(result.child.value, 6);
      assert.equal(Object.hasOwn(result, '__proto__'), true);
      assert.deepEqual(nodes, snapshot);
    });
  } else if (task === 'sse-decoder') {
    const { Decoder } = implementation;
    const bytes = new TextEncoder().encode('\uFEFF: comment\r\nevent: update\r\nid: 7\r\nretry: 42\r\ndata: hi🌍\r\ndata: second\r\n\r\ndata:\r\n\r\n');
    const expected = [{ event: 'update', id: '7', data: 'hi🌍\nsecond' }, { event: 'message', id: '7', data: '' }];
    await check('every-two-chunk-boundary', () => {
      for (let split = 0; split <= bytes.length; split += 1) {
        const decoder = new Decoder();
        assert.deepEqual([...decoder.push(bytes.slice(0, split)), ...decoder.push(bytes.slice(split)), ...decoder.end()], expected);
        assert.equal(decoder.retry, 42);
      }
    });
    await check('single-byte-chunks', () => {
      const decoder = new Decoder();
      assert.deepEqual([...bytes].flatMap(byte => decoder.push(Uint8Array.of(byte))), expected);
    });
    await check('cr-only-fields-and-invalid-retry', () => {
      const decoder = new Decoder();
      assert.deepEqual(decoder.push(new TextEncoder().encode('id: stable\rretry: 10\rdata:  spaced\r\rid: bad\u0000id\rretry: -1\rdata: next\r\r')), [{ event: 'message', id: 'stable', data: ' spaced' }, { event: 'message', id: 'stable', data: 'next' }]);
      assert.equal(decoder.retry, 10);
    });
    await check('discard-unfinished-and-ignore-empty-comment-records', () => {
      const decoder = new Decoder();
      assert.deepEqual(decoder.push(new TextEncoder().encode(': ignored\n\nunknown: ignored\n\ndata: unfinished')), []);
      assert.deepEqual(decoder.end(), []);
    });
    await check('pending-raw-byte-bound-and-validation', () => {
      for (const limit of [0, -1, 1.5]) assert.throws(() => new Decoder(limit));
      assert.throws(() => new Decoder(8).push(new TextEncoder().encode(':123456789')));
      const decoder = new Decoder(12);
      assert.deepEqual(decoder.push(new TextEncoder().encode('data: a\n\ndata: b\n\n')).map(event => event.data), ['a', 'b']);
    });
  } else if (task === 'interval-overlay') {
    const { overlay } = implementation;
    await check('split-and-preserve-gaps', () => assert.deepEqual(overlay([{ start: 0, end: 10, value: 'a' }, { start: 20, end: 30, value: 'b' }], [{ start: 3, end: 25, value: 'c' }]), [{ start: 0, end: 3, value: 'a' }, { start: 3, end: 25, value: 'c' }, { start: 25, end: 30, value: 'b' }]));
    await check('later-update-wins-and-adjacent-coalescing', () => assert.deepEqual(overlay([{ start: 0, end: 10, value: 1 }], [{ start: 2, end: 8, value: 2 }, { start: 4, end: 6, value: 1 }]), [{ start: 0, end: 2, value: 1 }, { start: 2, end: 4, value: 2 }, { start: 4, end: 6, value: 1 }, { start: 6, end: 8, value: 2 }, { start: 8, end: 10, value: 1 }]));
    await check('unsorted-immutable-input', () => {
      const existing = [{ start: 5, end: 10, value: null }, { start: 0, end: 5, value: null }];
      const snapshot = structuredClone(existing);
      assert.deepEqual(overlay(existing, []), [{ start: 0, end: 10, value: null }]);
      assert.deepEqual(existing, snapshot);
    });
    await check('large-sparse-safe-integer-ranges', () => assert.deepEqual(overlay([{ start: 0, end: Number.MAX_SAFE_INTEGER, value: 'a' }], [{ start: Number.MAX_SAFE_INTEGER - 2, end: Number.MAX_SAFE_INTEGER, value: 'b' }]), [{ start: 0, end: Number.MAX_SAFE_INTEGER - 2, value: 'a' }, { start: Number.MAX_SAFE_INTEGER - 2, end: Number.MAX_SAFE_INTEGER, value: 'b' }]));
    await check('invalid-endpoints-and-overlapping-existing', () => {
      for (const range of [{ start: -1, end: 2, value: 1 }, { start: 1.5, end: 2, value: 1 }, { start: 2, end: 2, value: 1 }, { start: 0, end: Infinity, value: 1 }]) assert.throws(() => overlay([], [range]));
      assert.throws(() => overlay([{ start: 0, end: 3, value: 1 }, { start: 2, end: 4, value: 2 }], []));
    });
    await check('deterministic-pointwise-oracle', () => {
      for (let seed = 1; seed <= 25; seed += 1) {
        const updates = Array.from({ length: 5 }, (_, index) => { const start = (seed * (index + 3)) % 18; return { start, end: start + 1 + (index % 4), value: index % 3 }; });
        const result = overlay([], updates);
        for (let position = 0; position < 24; position += 1) {
          const expectedRange = updates.findLast(range => position >= range.start && position < range.end);
          const actualRange = result.find(range => position >= range.start && position < range.end);
          assert.equal(actualRange?.value, expectedRange?.value);
        }
        for (let index = 1; index < result.length; index += 1) assert.ok(result[index - 1].end <= result[index].start);
      }
    });
  } else {
    throw new Error('Unknown coding fixture');
  }
  return { task, passed: cases.every(result => result.passed), passed_cases: cases.filter(result => result.passed).length, total_cases: cases.length, cases };
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  const [task, source] = process.argv.slice(2);
  const result = await grade(task, await import(pathToFileURL(path.resolve(source)).href));
  console.log(JSON.stringify(result));
  process.exitCode = result.passed ? 0 : 1;
}
