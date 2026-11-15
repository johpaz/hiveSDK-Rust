import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createRequire } from 'node:module';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, Objective, SessionStore, ToolOperationUncertain, VerificationUncertain, SummaryUncertain } from '../index.mjs';
const native = createRequire(import.meta.url)('../native/hive.node');
const reply = { content: 'Known 🦀', usage: { input_tokens: 3, output_tokens: 2 } };
const toolReply = { ...reply, content: null, tool_calls: [{ id: 'c', name: 'read', arguments: {} }] };
function deferred() { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; }
function gate(Type, method, matches) {
  const entered = deferred(); const released = deferred(); const original = Type.prototype[method]; let used = false;
  Type.prototype[method] = async function(...args) {
    if (!used && matches(...args)) { used = true; entered.resolve(); await released.promise; }
    return original.apply(this, args);
  };
  return { ready: entered.promise, release: released.resolve, restore() { Type.prototype[method] = original; } };
}
async function fixture(callback) {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-async-storage-')); const store = await SessionStore.open({ path: directory });
  try { await callback(store, directory); } finally { await store.closeAsync(); rmSync(directory, { recursive: true, force: true }); }
}
const config = store => ({ model: 'test', store, agentId: 'a', sessionId: 's', provider: { complete: () => reply } });

test('async factories, tenants, inspection and context plans preserve CAS and current APIs', () => fixture(async store => {
  const agent = await Agent.open(config(store)); await agent.run('History');
  const stale = await Agent.open(config(store)); const source = agent.captureContext(0, 2);
  const plan = { summary: { id: 's', source, content: 'Known' }, retrieved: [] };
  await agent.setContextPlanAsync(plan); assert.deepEqual(agent.contextPlan, plan);
  await assert.rejects(stale.setContextPlanAsync(null), /SESSION_CONFLICT/);
  assert.deepEqual((await Agent.open(config(store))).contextPlan, plan);
  assert.equal((await store.inspectAsync({ agentId: 'a', sessionId: 's' })).run.state.messages.length, 2);
  const tenant = await store.forTenantAsync('other'); assert.equal(await tenant.inspectAsync({ agentId: 'a', sessionId: 's' }), null); await tenant.closeAsync();
  const objective = await Objective.open({}, { store, id: 'root' }); assert.equal((await objective.inspectAsync()).model_calls, 0);
  const memory = await Agent.open({ model: 'test', provider: { complete: () => reply } }); await memory.run('Memory'); await memory.setContextPlanAsync(null);
}));
test('async model resolution fences stale IDs and terminal resume does not call a model', () => fixture(async store => {
  let calls = 0; const options = { ...config(store), provider: { complete() { calls++; throw new Error('lost'); } } };
  await assert.rejects((await Agent.open(options)).run('Work'), /lost/);
  const snapshot = await store.inspectAsync({ agentId: 'a', sessionId: 's' });
  const resolution = { agentId: 'a', sessionId: 's', revision: snapshot.revision, executionId: snapshot.run.in_flight.execution_id, reply };
  await assert.rejects(store.resolveModelAsync({ ...resolution, executionId: 'stale' }), /SESSION_CONFLICT/);
  await store.resolveModelAsync(resolution);
  assert.equal((await (await Agent.open({ ...options, resume: true })).resume()).content, reply.content); assert.equal(calls, 1);
}));
test('async tool resolution and recovery do not replay effects', () => fixture(async store => {
  let models = 0; let tools = 0;
  const options = { ...config(store), provider: { complete: () => ++models === 1 ? toolReply : reply },
    tools: [{ name: 'read', description: 'Read', parameters: { type: 'object' }, execute() { tools++; throw new ToolOperationUncertain(); } }] };
  await assert.rejects((await Agent.open(options)).run('Work'), ToolOperationUncertain);
  const snapshot = await store.inspectAsync({ agentId: 'a', sessionId: 's' });
  await store.resolveToolAsync({ agentId: 'a', sessionId: 's', revision: snapshot.revision, executionId: snapshot.run.in_flight.execution_id, result: 5 });
  await (await Agent.open({ ...options, resume: true })).resume(); assert.equal(tools, 1); assert.equal(models, 2);
  const failed = { ...config(store), sessionId: 'abandoned', provider: { complete() { throw new Error('lost'); } } };
  await assert.rejects((await Agent.open(failed)).run('Work'), /lost/); await store.recoverAsync({ agentId: 'a', sessionId: 'abandoned' });
  assert.deepEqual((await Agent.open(failed)).messages, []);
}));
test('pending maintenance blocks new loads, runs and synchronous store calls before awaiting', () => fixture(async store => {
  const options = { ...config(store), provider: { complete() { throw new Error('lost'); } } };
  const agent = await Agent.open(options); await assert.rejects(agent.run('Work'), /lost/);
  const held = gate(native.NativeSessionStore, 'dispatchAsync', command => JSON.parse(command).op === 'recover'); let pending;
  try {
    pending = store.recoverAsync({ agentId: 'a', sessionId: 's' }); await held.ready;
    assert.throws(() => new Agent(options), /BUSY/); await assert.rejects(Agent.open(options), /BUSY/);
    assert.throws(() => agent.setContextPlan(null), /BUSY/);
    await assert.rejects(agent.run('Blocked'), /BUSY/); assert.throws(() => store.inspect({ agentId: 'a', sessionId: 's' }), /BUSY/);
    await assert.rejects(store.closeAsync(), /BUSY/); held.release(); await pending;
    assert.equal((await store.inspectAsync({ agentId: 'a', sessionId: 's' })).status, 'ready');
  } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
}));
test('pending inspection retains the store and permits event loop progress', () => fixture(async store => {
  const held = gate(native.NativeSessionStore, 'dispatchAsync', command => JSON.parse(command).op === 'inspect'); let pending;
  try {
    pending = store.inspectAsync({ agentId: 'a', sessionId: 's' }); await held.ready;
    assert.throws(() => store.close(), /BUSY/); await assert.rejects(store.recoverAsync({ agentId: 'a', sessionId: 's' }), /BUSY/);
    await new Promise(resolve => setImmediate(resolve)); held.release(); assert.equal(await pending, null);
  } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
}));
test('async context update guards the agent and store until its known result is installed', () => fixture(async store => {
  const agent = await Agent.open(config(store)); await agent.run('History');
  const plan = { summary: { id: 's', source: agent.captureContext(0, 2), content: 'Known' }, retrieved: [] };
  const held = gate(native.NativeDurableEngine, 'dispatchAsync', command => JSON.parse(command).op === 'set_context_plan'); let pending;
  try {
    pending = agent.setContextPlanAsync(plan); await held.ready;
    assert.equal(agent.cancel(), false); assert.throws(() => agent.close(), /BUSY/); await assert.rejects(agent.run('Blocked'), /BUSY/);
    assert.throws(() => store.close(), /BUSY/); held.release(); await pending; assert.deepEqual(agent.contextPlan, plan);
  } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
}));
test('objective async reconciliation preserves errors, IDs and summary sources', () => fixture(async store => {
  const objective = await Objective.open({}, { store, id: 'root' });
  await assert.rejects(objective.verify('v', {}, () => { throw new Error('unknown'); }), VerificationUncertain);
  const id = (await objective.inspectAsync()).verifications.v.execution_id;
  await assert.rejects(objective.resolveVerificationAsync({ key: 'v', executionId: 'stale', verdict: { passed: true } }), /SESSION_CONFLICT/);
  assert.equal((await objective.resolveVerificationAsync({ key: 'v', executionId: id, verdict: { passed: true } })).verdict.passed, true);
  const agent = await Agent.open(config(store)); await agent.run('History'); const source = agent.captureContext(0, 2);
  await assert.rejects(objective.summarize('s', { source, messages: agent.readContext(source) }, () => { throw new Error('unknown'); }), SummaryUncertain);
  const summaryId = (await objective.inspectAsync()).summaries.s.execution_id;
  assert.equal((await objective.resolveSummaryAsync({ key: 's', executionId: summaryId, content: 'Known' })).summary.content, 'Known');
  const local = await Objective.open({});
  await assert.rejects(local.verify('v', {}, () => { throw new Error('unknown'); }), VerificationUncertain);
  const localId = (await local.inspectAsync()).verifications.v.execution_id;
  assert.equal((await local.resolveVerificationAsync({ key: 'v', executionId: localId, verdict: { passed: true } })).verdict.passed, true);
  await local.cancelAsync(); assert.equal((await local.inspectAsync()).stopped, 'cancelled');
}));
test('async objective cancellation persists the stop and signals active callbacks', () => fixture(async store => {
  const objective = await Objective.open({}, { store, id: 'root' }); const entered = deferred();
  const agent = await Agent.open({ ...config(store), objective, provider: { complete(_request, { signal }) {
    entered.resolve(); return new Promise((_, reject) => signal.addEventListener('abort', () => reject(new Error('cancelled')), { once: true }));
  } } });
  const pending = agent.run('Work'); const outcome = assert.rejects(pending, /CANCELLED/); await entered.promise;
  await objective.cancelAsync(); await outcome; assert.equal((await objective.inspectAsync()).stopped, 'cancelled');
}));
test('async public APIs do not fall back to synchronous native entry points', () => fixture(async store => {
  const methods = [[native.NativeSessionStore, 'dispatch'], [native.NativeSessionStore, 'openAgent'],
    [native.NativeSessionStore, 'forTenant'], [native.NativeSessionStore, 'close'], [native.NativeDurableEngine, 'dispatch']];
  const originals = methods.map(([Type, method]) => Type.prototype[method]);
  for (const [Type, method] of methods) Type.prototype[method] = () => { throw new Error(`synchronous native entry point: ${method}`); };
  try {
    const agent = await Agent.open(config(store)); await agent.setContextPlanAsync(null);
    const objective = await Objective.open({}, { store, id: 'root' }); await objective.inspectAsync();
    await store.inspectAsync({ agentId: 'a', sessionId: 's' });
    const tenant = await store.forTenantAsync('other'); await tenant.closeAsync();
  } finally { methods.forEach(([Type, method], index) => { Type.prototype[method] = originals[index]; }); }
}));
test('pending async agent load retains the store through metadata initialization', () => fixture(async store => {
  const held = gate(native.NativeSessionStore, 'openAgentAsync', () => true); let pending;
  try {
    pending = Agent.open(config(store)); await held.ready;
    assert.throws(() => store.close(), /BUSY/); held.release(); assert.deepEqual((await pending).messages, []);
  } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
}));
test('pending async close excludes new operations until the store is closed', () => fixture(async store => {
  const agent = await Agent.open(config(store)); const held = gate(native.NativeSessionStore, 'closeAsync', () => true); let pending;
  try {
    pending = store.closeAsync(); await held.ready;
    await assert.rejects(Agent.open(config(store)), /BUSY/); assert.throws(() => store.inspect({ agentId: 'a', sessionId: 's' }), /BUSY/);
    held.release(); await pending; await assert.rejects(store.inspectAsync({ agentId: 'a', sessionId: 's' }), /CLOSED/);
    assert.deepEqual(agent.messages, []);
  } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
}));
