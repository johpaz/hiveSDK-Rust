import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { Agent, Objective, SessionStore } from '../index.mjs';

async function fixture(work) {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-queue-'));
  const store = await SessionStore.open({ path: directory, tenantId: 'first' });
  try { await work(store, directory); }
  finally { await store.closeAsync(); rmSync(directory, { recursive: true, force: true }); }
}
const input = lane => ({ kind: 'test', lane, payload: { text: 'Hello' } });

test('queue deduplication, lanes and tenant isolation are native and durable', async () => fixture(async store => {
  const queue = await store.openQueue('work', { max_concurrency: 2 });
  const spec = { ...input('same'), idempotency_key: 'key' };
  const rows = await Promise.all([queue.enqueue(spec), queue.enqueue(spec)]);
  assert.equal(rows[0].id, rows[1].id);
  await assert.rejects(queue.enqueue({ ...spec, payload: { changed: true } }), /IDEMPOTENCY_CONFLICT/);
  await queue.enqueue(input('same'));
  const other = await store.forTenantAsync('second');
  try { assert.equal(await (await other.openQueue('work')).get(rows[0].id), null); }
  finally { await other.closeAsync(); }
  const active = await queue.claim('worker', ['test']);
  assert.equal(await queue.claim('second', ['test']), null);
  assert.equal((await queue.cancel(active.id)).status, 'cancelling');
  const result = await queue.finish(active.id, active.execution_id, { type: 'completed', result: 'late' });
  assert.equal(result.status, 'cancelled'); assert.equal(result.result, null);
  await assert.rejects(queue.finish(active.id, active.execution_id, { type: 'completed', result: 'later' }), /FENCE/);
  assert.ok(await queue.claim('worker', ['test']));
}));

test('host callback batches renew leases, join work and respect lanes', async () => fixture(async store => {
  const queue = await store.openQueue('pool', { max_concurrency: 2, lease_ms: 300 });
  for (const lane of ['a', 'a', 'b', 'b']) await queue.enqueue(input(lane));
  let active = 0; let peak = 0; const lanes = new Set();
  const executors = { test: async job => {
    assert.equal(lanes.has(job.input.lane), false); lanes.add(job.input.lane);
    active++; peak = Math.max(peak, active);
    await delay(420);
    active--; lanes.delete(job.input.lane);
    return { type: 'completed', result: { ok: true } };
  } };
  for (let i = 0; i < 4; i++) await queue.runOnce(executors);
  assert.equal(peak, 2); assert.equal(active, 0);
  const { items } = await queue.list();
  assert.equal(items.length, 4);
  for (const job of items) { assert.equal(job.status, 'completed'); assert.equal(job.attempts, 1); assert.ok(job.revision > 3); }
}));

test('executor exceptions are uncertain and require a fenced reconciliation', async () => fixture(async store => {
  const queue = await store.openQueue('uncertain');
  await queue.enqueue(input('a')); await queue.enqueue(input('a'));
  let calls = 0;
  const [job] = await queue.runOnce({ test: async () => { calls++; throw new Error('unknown external outcome'); } });
  assert.equal(job.status, 'uncertain'); assert.equal(calls, 1);
  assert.deepEqual(await queue.runOnce({ test: () => { throw new Error('must not run'); } }), []);
  await assert.rejects(queue.reconcile(job.id, 'wrong', { type: 'cancelled' }), /FENCE/);
  await queue.reconcile(job.id, job.execution_id, { type: 'completed', result: { receipt: 'external-1' } });
  const [next] = await queue.runOnce({ test: async () => ({ type: 'completed', result: 'next' }) });
  assert.equal(next.status, 'completed');
}));

test('queued agents inherit the durable root budget without recreating it', async () => fixture(async store => {
  const objective = await Objective.open({ max_model_calls: 1, max_tool_calls: 8 }, { store, id: 'root' });
  const queue = await store.openQueue('agents', { max_concurrency: 1 });
  let calls = 0;
  const agent = await Agent.open({ store, objective, agentId: 'worker', sessionId: 'chat', model: 'fixture',
    provider: { complete() { calls++; return { content: 'done', usage: { input_tokens: 1, output_tokens: 1 } }; } } });
  try {
    const executor = async (_job, context) => {
      assert.equal(Object.isFrozen(context), true);
      assert.throws(() => { context.depth = 0; }, TypeError);
      try {
        const result = await context.runAgent(agent, 'Run');
        return result.status === 'completed' ? { type: 'completed', result } : { type: 'failed', error: result.stop_reason };
      } catch (error) { return { type: 'failed', error: error.message }; }
    };
    await queue.enqueue({ ...input('agent'), objective_id: 'root' });
    assert.equal((await queue.runOnce({ test: executor }))[0].status, 'completed');
    await queue.enqueue({ ...input('agent'), objective_id: 'root' });
    assert.equal((await queue.runOnce({ test: executor }))[0].status, 'failed');
    assert.equal(calls, 1);
    const root = await objective.inspectAsync(); assert.equal(root.model_calls, 1); assert.equal(root.tool_calls, 2);
  } finally { agent.close(); }
}));

test('aborting a batch waits for cooperative callbacks and does not expose a free lane early', async () => fixture(async store => {
  const queue = await store.openQueue('cancel', { lease_ms: 300 }); const original = await queue.enqueue(input('a'));
  const abort = new AbortController(); let entered; const started = new Promise(resolve => { entered = resolve; }); let joined = false;
  const batch = queue.runOnce({ test: async (_job, context) => {
    entered();
    if (!context.signal.aborted) await new Promise(resolve => context.signal.addEventListener('abort', resolve, { once: true }));
    await delay(420); joined = true; return { type: 'cancelled' };
  } }, { signal: abort.signal });
  await started; await assert.rejects(store.closeAsync(), /BUSY/); abort.abort();
  const [job] = await batch; assert.equal(job.id, original.id); assert.equal(job.status, 'cancelled'); assert.equal(joined, true);
}));

test('specialist groups bind assigned sessions and require durable evidence before fan-in', async () => fixture(async store => {
  const objective = await Objective.open({ max_model_calls: 3, max_tool_calls: 8 }, { store, id: 'root' });
  const queue = await store.openQueue('groups');
  const group = await queue.createGroup('turn', { objective_id: 'root', coordinator_agent_id: 'lead', require_verification: true,
    tasks: [{ id: 'a', specialist_id: 'research', prompt: 'Find evidence' }, { id: 'b', specialist_id: 'review', prompt: 'Review evidence' }] });
  const another = await queue.createGroup('another', { objective_id: 'root', coordinator_agent_id: 'lead',
    tasks: [{ id: 'c', specialist_id: 'research', prompt: 'Separate turn' }] });
  let calls = 0;
  const provider = { complete() { calls++; return { content: 'fixture answer', usage: { input_tokens: 1, output_tokens: 1 } }; } };
  const wrong = await Agent.open({ store, objective, agentId: 'wrong', sessionId: 'wrong', model: 'fixture', provider });
  try {
    const execute = async (job, context) => {
      await assert.rejects(context.runAgent(wrong, job.input.payload.prompt), /BINDING_MISMATCH/);
      const binding = job.input.payload.delegation;
      const agent = await Agent.open({ store, objective, agentId: binding.agent_id, sessionId: binding.session_id, model: 'fixture', provider });
      try { return { type: 'completed', result: await context.runAgent(agent, job.input.payload.prompt) }; }
      finally { agent.close(); }
    };
    const children = await queue.runOnce({ [group.specialist_kind]: execute });
    assert.equal(children.length, 2); assert.equal(calls, 2);
    assert.equal((await queue.get(another.job_ids[0])).status, 'pending');
    await assert.rejects(queue.synthesizeGroup('turn'), /NOT_READY/);
    for (const taskId of ['a', 'b']) {
      const input = await queue.groupVerificationInput('turn', taskId);
      await objective.verify(`check-${taskId}`, input, async () => ({ passed: true, evidence: ['fixture assertion'] }));
      const proof = await queue.recordGroupProof('turn', taskId, `check-${taskId}`);
      assert.equal(proof.met, true);
    }
    const synthesis = await queue.synthesizeGroup('turn');
    assert.equal(synthesis.input.depth, 0);
    assert.equal((await queue.synthesizeGroup('turn')).id, synthesis.id);
    const [done] = await queue.runOnce({ [group.synthesis_kind]: execute });
    assert.equal(done.status, 'completed'); assert.equal(calls, 3);
    const snapshot = await objective.inspectAsync(); assert.equal(snapshot.model_calls, 3); assert.equal(snapshot.tool_calls, 5);
    assert.equal((await queue.group('turn')).status, 'completed');
  } finally { wrong.close(); }
}));

test('group cancellation remains explicit and persisted', async () => fixture(async store => {
  await Objective.open({}, { store, id: 'root' });
  const queue = await store.openQueue('cancel-group');
  await queue.createGroup('turn', { objective_id: 'root', coordinator_agent_id: 'lead', tasks: [{ id: 'a', specialist_id: 'worker', prompt: 'Run' }] });
  const group = await queue.cancelGroup('turn');
  assert.equal(group.status, 'cancelled'); assert.equal(group.tasks[0].job.status, 'cancelled');
  await assert.rejects(queue.synthesizeGroup('turn'), /CANCELLED/);
}));
