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

test('a goal runs plan, delegation, synthesis and review to acceptance and replans on rejection', async () => fixture(async store => {
  await Objective.open({ max_model_calls: 10 }, { store, id: 'root' });
  const queue = await store.openQueue('goals');
  let goal = await queue.createGoal('ship', { objective_id: 'root', goal: 'Ship the report', planner_agent_id: 'planner', coordinator_agent_id: 'lead', max_rounds: 2 });
  assert.equal(goal.status, 'planning');
  const finish = async (kind, result) => {
    const job = await queue.claim('w', [kind]);
    assert.ok(job, `a pending ${kind} job`);
    return queue.finish(job.id, job.execution_id, { type: 'completed', result });
  };
  const tasks = [{ id: 'a', specialist_id: 'research', prompt: 'Find evidence' }];
  for (const round of [0, 1]) {
    await finish(goal.kinds.plan, { tasks });
    goal = await queue.tickGoal('ship');
    assert.equal(goal.status, 'running');
    await finish(goal.kinds.rounds[round].specialist, { text: 'evidence' });
    goal = await queue.tickGoal('ship');
    await finish(goal.kinds.rounds[round].synthesis, { text: 'synthesis' });
    goal = await queue.tickGoal('ship');
    assert.equal(goal.status, 'reviewing');
    const accepted = round === 1;
    await finish(goal.kinds.review, accepted ? { accepted: true, evidence: 'report.pdf written' } : { accepted: false, feedback: 'needs a chart' });
    goal = await queue.tickGoal('ship');
    assert.equal(goal.status, accepted ? 'accepted' : 'planning');
  }
  assert.equal(goal.rounds.length, 2);
  assert.equal(goal.rounds[1].verdict.evidence, 'report.pdf written');
  const retained = (await queue.inspect()).retained_jobs;
  assert.equal((await queue.tickGoal('ship')).status, 'accepted');
  assert.equal((await queue.inspect()).retained_jobs, retained);
  await assert.rejects(queue.createGoal('ship', { objective_id: 'root', goal: 'Another', planner_agent_id: 'p', coordinator_agent_id: 'c' }), /GOAL_IDEMPOTENCY_CONFLICT/);
  const second = await queue.createGoal('stop', { objective_id: 'root', goal: 'Never mind', planner_agent_id: 'p', coordinator_agent_id: 'c' });
  assert.equal((await queue.cancelGoal('stop')).status, 'cancelled');
  assert.equal(await queue.claim('w', [second.kinds.plan]), null);
  await assert.rejects(queue.goal('missing'), /GOAL_NOT_FOUND/);
}));

test('runGoal drives a goal with the host agents and charges the root objective', async () => fixture(async store => {
  const { runGoal, extractJson } = await import('../index.mjs');
  assert.deepEqual(extractJson('Sure!\n```json\n{"a":1}\n```'), { a: 1 });
  assert.deepEqual(extractJson('prefix {"a":{"b":2}} suffix'), { a: { b: 2 } });
  assert.equal(extractJson('no json here'), null);
  assert.equal(extractJson('[1,2]'), null);
  const objective = await Objective.open({ max_model_calls: 20 }, { store, id: 'root' });
  const queue = await store.openQueue('driven');
  const seen = [];
  let reviews = 0;
  const provider = { complete(request) {
    const text = JSON.stringify(request);
    let content = 'specialist output';
    if (text.includes('Plan round')) content = 'Here is the plan:\n```json\n{"tasks":[{"id":"a","specialist_id":"research","prompt":"Find evidence"},{"id":"b","specialist_id":"writer","prompt":"Draft"}]}\n```';
    else if (text.includes('Review round')) content = ++reviews === 1 ? '{"accepted":false,"feedback":"add sources"}' : '{"accepted":true,"evidence":"sources added"}';
    return { content, usage: { input_tokens: 1, output_tokens: 1 } };
  } };
  await queue.createGoal('g', { objective_id: 'root', goal: 'Write it up', planner_agent_id: 'planner', coordinator_agent_id: 'lead', max_rounds: 3 });
  const report = await runGoal(queue, 'g', { agentFor: async request => {
    seen.push(`${request.stage}:${request.agentId}`);
    return Agent.open({ store, objective, agentId: request.agentId, sessionId: request.sessionId, model: 'fixture', provider });
  } });
  assert.equal(report.status, 'accepted');
  assert.equal(report.rounds.length, 2);
  assert.equal(report.rounds[1].verdict.evidence, 'sources added');
  assert.deepEqual(seen.filter(s => s.startsWith('plan')), ['plan:planner', 'plan:planner']);
  assert.equal(seen.length, 2 * (1 + 2 + 1 + 1), 'plan, two tasks, synthesis and review in each of two rounds');
  assert.equal((await objective.inspectAsync()).model_calls, 10);

  // A model that does not answer with JSON fails the goal instead of being guessed.
  await queue.createGoal('bad', { objective_id: 'root', goal: 'x', planner_agent_id: 'planner', coordinator_agent_id: 'lead' });
  const babble = { complete() { return { content: 'I will think about it', usage: { input_tokens: 1, output_tokens: 1 } }; } };
  const failed = await runGoal(queue, 'bad', { agentFor: r => Agent.open({ store, objective, agentId: r.agentId, sessionId: r.sessionId, model: 'fixture', provider: babble }) });
  assert.equal(failed.status, 'failed');
  assert.match(failed.error, /PLAN_JOB_Failed.*not a JSON object/);
  await assert.rejects(runGoal(queue, 'g', {}), /agentFor/);
}));

test('job administration filters, retries and cancels, and the event log is a durable cursor', async () => fixture(async store => {
  const queue = await store.openQueue('admin');
  const job = (kind, lane, extra = {}) => ({ kind, lane, payload: { n: 1 }, ...extra });
  await queue.enqueue(job('flaky', 'l1', { max_attempts: 2 }));
  for (let n = 0; n < 3; n++) await queue.enqueue(job('batch', `b${n}`));
  assert.equal((await queue.listJobs({ kind: 'batch' })).items.length, 3);
  assert.equal((await queue.listJobs({ status: ['pending'] }, { limit: 2 })).items.length, 2);
  const failing = await queue.claim('w', ['flaky']);
  const failed = await queue.finish(failing.id, failing.execution_id, { type: 'failed', error: 'boom' });
  assert.equal(failed.status, 'failed');
  assert.equal((await queue.retryJob(failed.id)).status, 'pending');
  const second = await queue.claim('w', ['flaky']);
  await queue.finish(second.id, second.execution_id, { type: 'failed', error: 'boom again' });
  await assert.rejects(queue.retryJob(failed.id), /JOB_RETRY_NOT_ALLOWED/);
  assert.equal((await queue.retryJob(failed.id, { force: true })).status, 'pending');
  assert.deepEqual((await queue.cancelJobs({ status: ['completed', 'failed'] })).cancelled, []);
  const cancelled = await queue.cancelJobs({ kind: 'batch' });
  assert.equal(cancelled.cancelled.length, 3);
  assert.equal((await queue.listJobs({ kind: 'flaky', status: ['pending'] })).items.length, 1, 'other work is untouched');
  const page = await queue.events({ after: 0, limit: 3 });
  assert.deepEqual(page.items.map(event => event.seq), [1, 2, 3]);
  const rest = await queue.events({ after: page.next_after, limit: 100 });
  assert.equal(rest.items[0].seq, 4);
  assert.ok(rest.items.some(event => event.type === 'job.retried'));
  assert.equal(rest.next_after, rest.latest);
  await assert.rejects(queue.purgeJobs({}, Date.now()), /explicit kind/);
  await assert.rejects(queue.purgeJobs({ kind: 'hive.goal.plan.x' }, Date.now()), /cannot be purged/);
  assert.equal((await queue.purgeJobs({ kind: 'flaky', status: ['pending'] }, Date.now() + 1000)).purged, 0, 'live jobs are never purged');
  assert.equal((await queue.pruneEvents(3)).removed, 3);
  assert.equal((await queue.events()).items[0].seq, 4);
}));
