import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { Agent, Objective, ObjectiveLimit, SessionStore, ToolOperationUncertain } from '../index.mjs';

const policy = { max_model_calls: 3, max_output_tokens: 10, max_total_output_tokens: 15, max_depth: 1 };
const reply = (content = 'done') => ({ content, usage: { input_tokens: 4, output_tokens: 3 } });
const call = () => ({ ...reply(null), tool_calls: [{ id: 'c', name: 'delegate', arguments: {} }] });
const tool = execute => ({ name: 'delegate', description: 'Explicit delegation', parameters: { type: 'object' }, execute });
const directory = () => mkdtempSync(path.join(tmpdir(), 'hive-objective-durable-'));

test('durable parent and child share caps and retain the same root across reopening', async () => {
  const dir = directory(); let store = new SessionStore({ path: dir });
  try {
    const objective = new Objective(policy, { store, id: 'root' }); const caps = []; let calls = 0;
    const child = new Agent({ model: 'child', objective: new Objective(policy, { store, id: 'root' }), store, agentId: 'a', sessionId: 'child', provider: { complete(request) {
      caps.push(request.max_output_tokens); return reply('child');
    } } });
    const parentOptions = { model: 'parent', objective, store, agentId: 'a', sessionId: 'parent',
      provider: { complete(request) { caps.push(request.max_output_tokens); return calls++ ? reply('finished') : call(); } },
      tools: [tool(async (_, context) => (await context.delegate(child, 'Work')).content)] };
    const parent = new Agent(parentOptions);
    assert.equal((await parent.run('Work')).content, 'finished');
    assert.deepEqual(caps, [10, 10, 9]);
    const snapshot = objective.inspect(); assert.equal(snapshot.model_calls, 3); assert.equal(snapshot.output_tokens, 9);
    assert.deepEqual(snapshot.reservations, {}); assert.equal(snapshot.tool_calls, 1);
    assert.equal(store.inspect({ agentId: 'a', sessionId: 'child' }).run.objective_depth, 1);
    parent.close(); child.close(); store.close();
    store = new SessionStore({ path: dir });
    const reopened = new Objective(policy, { store, id: 'root' });
    assert.equal(reopened.inspect().model_calls, 3);
    const agent = new Agent({ ...parentOptions, store, objective: reopened });
    await assert.rejects(agent.run('Again'), ObjectiveLimit); assert.equal(calls, 2); agent.close();
    assert.throws(() => new Objective({ ...policy, max_model_calls: 4 }, { store, id: 'root' }), /SESSION_CONFIG_MISMATCH/);
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('a clean checkpoint resumes with root counters and without replaying a tool', async () => {
  const dir = directory(); let store = new SessionStore({ path: dir }); let effects = 0; let calls = 0;
  const tools = [tool(() => { effects++; return 'effect'; })];
  try {
    let objective = new Objective(policy, { store, id: 'root' });
    const base = { model: 'test', tools, agentId: 'a', sessionId: 's', provider: { complete: () => { calls++; return calls === 1 ? call() : reply('resumed'); } } };
    const agent = new Agent({ ...base, store, objective, onEvent(action) { if (action.type === 'model' && calls) throw new Error('pause'); } });
    await assert.rejects(agent.run('Work'), /pause/); agent.close();
    assert.equal(objective.inspect().model_calls, 1); assert.equal(objective.inspect().tool_calls, 1);
    store.close(); store = new SessionStore({ path: dir }); objective = new Objective(policy, { store, id: 'root' });
    const resumed = new Agent({ ...base, store, objective, resume: true });
    assert.equal((await resumed.resume()).content, 'resumed'); resumed.close();
    assert.equal(effects, 1); assert.equal(calls, 2); assert.equal(objective.inspect().model_calls, 2);
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('uncertain model reconciliation settles its durable root permit exactly once', async () => {
  const dir = directory(); let store = new SessionStore({ path: dir }); let calls = 0;
  try {
    let objective = new Objective(policy, { store, id: 'root' });
    const base = { model: 'test', agentId: 'a', sessionId: 's', provider: { complete() { calls++; throw new Error('lost'); } } };
    const agent = new Agent({ ...base, store, objective }); await assert.rejects(agent.run('Work'), /lost/); agent.close();
    assert.deepEqual(objective.inspect().reservations, { 1: 10 });
    store.close(); store = new SessionStore({ path: dir }); objective = new Objective(policy, { store, id: 'root' });
    assert.throws(() => new Agent({ ...base, store, resume: true }), /SESSION_CONFIG_MISMATCH/);
    assert.throws(() => new Agent({ ...base, store, objective, resume: true }), /SESSION_OPERATION_UNCERTAIN/);
    const inspection = store.inspect({ agentId: 'a', sessionId: 's' });
    assert.equal(inspection.run.in_flight.objective_permit, 1);
    const resolution = { agentId: 'a', sessionId: 's', revision: inspection.revision,
      executionId: inspection.run.in_flight.execution_id, reply: reply('verified') };
    store.resolveModel(resolution); assert.throws(() => store.resolveModel(resolution));
    const resumed = new Agent({ ...base, store, objective, resume: true });
    assert.equal((await resumed.resume()).content, 'verified'); resumed.close();
    assert.equal(calls, 1); assert.equal(objective.inspect().model_calls, 1); assert.equal(objective.inspect().output_tokens, 3);
    assert.deepEqual(objective.inspect().reservations, {});
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('missing root usage returns a durable stop and cannot restart spending', async () => {
  const dir = directory(); const store = new SessionStore({ path: dir });
  try {
    const objective = new Objective(policy, { store, id: 'root' });
    const agent = new Agent({ model: 'test', store, objective, agentId: 'a', sessionId: 's', provider: { complete: () => ({ content: 'unmetered' }) } });
    const result = await agent.run('Work'); assert.equal(result.stop_reason, 'usage_unavailable'); agent.close();
    assert.equal(objective.inspect().stopped, 'usage_unavailable'); assert.deepEqual(objective.inspect().reservations, { 1: 10 });
    let calls = 0;
    const other = new Agent({ model: 'test', store, objective, agentId: 'a', sessionId: 'other', provider: { complete() { calls++; return reply(); } } });
    await assert.rejects(other.run('Again'), ObjectiveLimit); assert.equal(calls, 0);
    other.close();
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('actual process exit after dispatch retains the root reservation without another inference', async () => {
  const dir = directory(); let store;
  try {
    const module = new URL('../index.mjs', import.meta.url).href;
    const script = `import { Agent, Objective, SessionStore } from ${JSON.stringify(module)};
      const store = new SessionStore({path:process.argv[1]});
      const objective = new Objective(${JSON.stringify(policy)}, {store,id:'root'});
      await new Agent({model:'test',store,objective,agentId:'a',sessionId:'s',provider:{complete(){process.exit(19)}}}).run('Work');`;
    const child = spawnSync(process.execPath, ['--input-type=module', '-e', script, dir], { encoding: 'utf8', timeout: 30_000 });
    assert.equal(child.status, 19, child.stderr);
    store = new SessionStore({ path: dir });
    const objective = new Objective(policy, { store, id: 'root' }); let calls = 0;
    const base = { model: 'test', store, objective, agentId: 'a', sessionId: 's', provider: { complete() { calls++; return reply(); } } };
    assert.deepEqual(objective.inspect().reservations, { 1: 10 });
    assert.throws(() => new Agent({ ...base, resume: true }), /SESSION_OPERATION_UNCERTAIN/);
    const inspection = store.inspect({ agentId: 'a', sessionId: 's' });
    store.resolveModel({ agentId: 'a', sessionId: 's', revision: inspection.revision,
      executionId: inspection.run.in_flight.execution_id, reply: reply('verified after crash') });
    const resumed = new Agent({ ...base, resume: true });
    assert.equal((await resumed.resume()).content, 'verified after crash'); resumed.close();
    assert.equal(calls, 0); assert.equal(objective.inspect().model_calls, 1);
  } finally { store?.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('resuming a child preserves its depth and cannot delegate another level', async () => {
  const dir = directory(); const store = new SessionStore({ path: dir });
  try {
    const objective = new Objective(policy, { store, id: 'root' }); let grandCalls = 0;
    const grand = new Agent({ model: 'grand', provider: { complete() { grandCalls++; return reply(); } } });
    const childConfig = { model: 'child', store, objective, agentId: 'a', sessionId: 'child',
      provider: { complete: () => call() }, tools: [tool((_, context) => context.delegate(grand, 'Work'))] };
    const child = new Agent({ ...childConfig, onEvent() { throw new Error('pause child before dispatch'); } });
    const parent = new Agent({ model: 'parent', store, objective, agentId: 'a', sessionId: 'parent',
      provider: { complete: () => call() }, tools: [tool((_, context) => context.delegate(child, 'Work'))] });
    await assert.rejects(parent.run('Work'), ToolOperationUncertain); parent.close(); child.close();
    const resumed = new Agent({ ...childConfig, resume: true });
    await assert.rejects(resumed.resume(), ObjectiveLimit); resumed.close(); grand.close();
    assert.equal(grandCalls, 0); assert.equal(store.inspect({ agentId: 'a', sessionId: 'child' }).run.objective_depth, 1);
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('durable cancellation propagates, prevents store close and remains terminal after reopening', async () => {
  const dir = directory(); let store = new SessionStore({ path: dir });
  try {
    const objective = new Objective(policy, { store, id: 'root' });
    let enter; const entered = new Promise(resolve => { enter = resolve; });
    const agent = new Agent({ model: 'test', objective, store, agentId: 'a', sessionId: 's', provider: {
      async complete(_, { signal }) { enter(); await new Promise((_, reject) => signal.addEventListener('abort', () => reject(new Error('aborted')), { once: true })); }
    } });
    const pending = agent.run('Work'); await entered;
    assert.throws(() => store.close(), /BUSY/); objective.cancel(); await assert.rejects(pending, /CANCELLED/); agent.close();
    assert.equal(objective.inspect().stopped, 'cancelled'); assert.deepEqual(objective.inspect().reservations, { 1: 10 });
    store.close(); store = new SessionStore({ path: dir });
    assert.equal(new Objective(policy, { store, id: 'root' }).inspect().stopped, 'cancelled');
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});
