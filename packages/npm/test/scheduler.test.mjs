import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, Objective, SessionStore } from '../index.mjs';
async function fixture(work) {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-scheduler-'));
  const store = await SessionStore.open({ path: directory });
  try { await work(store); } finally { await store.closeAsync(); rmSync(directory, { recursive: true, force: true }); }
}
test('scheduled agents publish atomically, inherit the root and do not replay on reopening', async () => fixture(async store => {
  const root = await Objective.open({ max_model_calls: 1, max_tool_calls: 1 }, { store, id: 'root' });
  const queue = await store.openQueue('calendar');
  const spec = { schedule: { kind: 'at', unix_ms: 0 }, misfire_policy: 'fire_once', max_runs: 1,
    job: { kind: 'scheduled', lane: 'agent', payload: { prompt: 'Run fixture' }, objective_id: 'root' } };
  assert.equal((await queue.createSchedule('one', spec)).enabled, false);
  assert.equal((await queue.tickSchedule('one')).outcome, 'idle');
  await queue.enableSchedule('one');
  const ticks = await Promise.all([queue.tickSchedule('one'), queue.tickSchedule('one')]);
  assert.equal(ticks.filter(value => value.outcome === 'published').length, 1);
  assert.equal((await root.inspectAsync()).tool_calls, 0);
  let calls = 0;
  const agent = await Agent.open({ store, objective: root, agentId: 'worker', sessionId: 'scheduled', model: 'fixture',
    provider: { complete() { calls++; return { content: 'fixture', usage: { input_tokens: 1, output_tokens: 1 } }; } } });
  try {
    const [done] = await queue.runOnce({ scheduled: async (job, context) => {
      assert.equal(job.input.payload.schedule.scheduled_for, 0);
      return { type: 'completed', result: await context.runAgent(agent, job.input.payload.prompt) };
    } });
    assert.equal(done.status, 'completed'); assert.equal(calls, 1);
    assert.equal((await queue.schedule('one')).status, 'completed');
    assert.equal((await queue.createSchedule('one', spec)).published, 1);
    assert.equal((await queue.tickSchedule('one')).outcome, 'idle');
    assert.equal((await root.inspectAsync()).tool_calls, 1);
  } finally { agent.close(); }
}));
test('calendar calculations share timezone/seconds semantics and scheduling scopes', async () => fixture(async store => {
  const queue = await store.openQueue('calendar');
  const next = await queue.nextOccurrence({ kind: 'cron', expression: '*/15 * * * * *', timezone: 'UTC' }, Date.parse('2026-01-01T00:00:00Z'));
  assert.equal(next, Date.parse('2026-01-01T00:00:15Z'));
  await assert.rejects(queue.nextOccurrence({ kind: 'cron', expression: '* * * * *', timezone: 'invalid' }, 0), /timezone/);
  for (const id of ['a', 'b', 'c']) await queue.createSchedule(id, { schedule: { kind: 'at', unix_ms: 0 }, job: { kind: 'fixture', lane: id, payload: {} } });
  await assert.rejects(queue.tickSchedule('a', NaN), /integer/);
  await assert.rejects(queue.createSchedule('unsafe', { schedule: { kind: 'at', unix_ms: 0 }, max_runs: Infinity,
    job: { kind: 'fixture', lane: 'a', payload: {} } }), /finite/);
  assert.equal(await queue.schedule('unsafe'), null);
  const first = await queue.listSchedules({ limit: 2 });
  assert.equal(first.items.length, 2); assert.equal((await queue.listSchedules({ limit: 2, cursor: first.next_cursor })).items.length, 1);
  const other = await store.openQueue('other'); assert.equal(await other.schedule('a'), null);
  await assert.rejects(other.listSchedules({ cursor: first.next_cursor }), /scope/);
  await assert.rejects(queue.createSchedule('a', { schedule: { kind: 'at', unix_ms: 1 }, job: { kind: 'fixture', lane: 'a', payload: {} } }), /IDEMPOTENCY/);
}));
test('opt-in producer stops cleanly and never invokes executors implicitly', async () => fixture(async store => {
  const queue = await store.openQueue('watch');
  await queue.createSchedule('one', { schedule: { kind: 'at', unix_ms: 0 }, misfire_policy: 'fire_once',
    job: { kind: 'fixture', lane: 'one', payload: {} } });
  await queue.enableSchedule('one');
  const stop = new AbortController();
  const producer = queue.runScheduler({ pollMs: 10, signal: stop.signal });
  try {
    const deadline = Date.now() + 2000;
    while ((await queue.schedule('one')).published !== 1) {
      assert.ok(Date.now() < deadline);
      await new Promise(resolve => setTimeout(resolve, 5));
    }
    await assert.rejects(store.closeAsync(), /BUSY/);
  } finally { stop.abort(); await producer; }
  const [job] = (await queue.list()).items;
  assert.equal(job.status, 'pending'); assert.equal(job.attempts, 0);
}));
