import assert from 'node:assert/strict';
import { test } from 'node:test';
import { checkTrial } from '../../scripts/native-soak.mjs';

test('native endurance gate rejects short, incomplete, repeated and unverified work', () => {
  const receipt = { stages: 2, seconds_per_stage: 10, model: 'fixture', reasoning_effort: 'medium' };
  const stages = [1, 2].map((stage, index) => ({ stage, elapsed_ms: 10000,
    started_at: new Date(index * 10000).toISOString(), completed_at: new Date((index + 1) * 10000).toISOString() }));
  const events = [
    { kind: 'checkpoint.created' },
    { kind: 'model.started', payload: { route: { model: 'fixture', reasoning_effort: 'medium' } } },
    ...[1, 2].flatMap(stage => [
      { seq: stage * 3, kind: 'operation.executing', payload: { id: String(stage) } },
      { seq: stage * 3 + 1, kind: 'operation.output', payload: { id: String(stage), text: `SOAK_STAGE_${stage}_START` } },
      { seq: stage * 3 + 2, kind: 'operation.succeeded', payload: { id: String(stage) } },
    ]),
    { seq: 10, kind: 'operation.output', payload: { id: 'verify', text: 'SOAK_VERIFIED stages=2' } },
    { seq: 11, kind: 'operation.succeeded', payload: { id: 'verify' } },
    { kind: 'run.completed' },
  ];
  assert.equal(checkTrial(receipt, events, stages, 20000).stage_operations_with_live_output, 2);
  assert.throws(() => checkTrial(receipt, events, stages, 19999), /duration is too short/);
  assert.throws(() => checkTrial(receipt, events.filter(event => event.kind !== 'run.completed'), stages, 20000), /actually complete/);
  assert.throws(() => checkTrial(receipt, events, stages.slice(0, 1), 20000), /Missing completed stages/);
  assert.throws(() => checkTrial(receipt, events, [{ ...stages[0], elapsed_ms: 9999 }, stages[1]], 20000), /skipped its timed work/);
  assert.throws(() => checkTrial(receipt, [...events, events[2]], stages, 20000), /dispatched twice/);
  assert.throws(() => checkTrial(receipt, [...events, { kind: 'operation.outcome_unknown' }], stages, 20000), /reconciliation/);
  assert.throws(() => checkTrial({ ...receipt, model: 'different' }, events, stages, 20000), /route changed/);
  assert.throws(() => checkTrial(receipt, events.filter(event => event.payload?.id !== 'verify'), stages, 20000), /Final stage verification/);
  assert.throws(() => checkTrial(receipt, events.filter(event => event.kind !== 'checkpoint.created'), stages, 20000), /checkpoint/);
  assert.throws(() => checkTrial(receipt, events.filter(event => !(event.kind === 'operation.output' && event.payload.id === '2')), stages, 20000), /live output/);
  const longStages = stages.map((stage, index) => ({ ...stage, elapsed_ms: 90000,
    started_at: new Date(index * 90000).toISOString(), completed_at: new Date((index + 1) * 90000).toISOString() }));
  assert.throws(() => checkTrial({ ...receipt, seconds_per_stage: 90 }, events, longStages, 180000), /periodic heartbeats/);
});
