import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createRequire } from 'node:module';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, Objective, SessionStore } from '../index.mjs';
const require = createRequire(import.meta.url);
const { NativeDurableEngine } = require('../native/hive.node');
const reply = { content: 'Known 🦀', usage: { input_tokens: 3, output_tokens: 2 } };
function deferred() { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; }
// Gate only a specific worker boundary; real storage still executes after release.
function gate(operation) {
  const entered = deferred(); const released = deferred(); const original = NativeDurableEngine.prototype.dispatchAsync; let gated = false;
  NativeDurableEngine.prototype.dispatchAsync = async function(command) {
    if (!gated && JSON.parse(command).op === operation) { gated = true; entered.resolve(); await released.promise; }
    return original.call(this, command);
  };
  return { ready: entered.promise, release: released.resolve, restore() { NativeDurableEngine.prototype.dispatchAsync = original; } };
}
async function fixture(callback) {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-durable-handle-')); const store = new SessionStore({ path: directory });
  try { await callback(store); } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
}
test('pending durable begin keeps the store busy and cancellation never invokes a callback', () => fixture(async store => {
  let models = 0; const config = { model: 'test', store, agentId: 'a', sessionId: 's', provider: { complete() { models++; return reply; } } };
  const agent = new Agent(config); const held = gate('begin_run'); let pending;
  try {
    pending = agent.run('Work'); const outcome = assert.rejects(pending, /CANCELLED/); await held.ready;
    assert.throws(() => store.close(), /BUSY/); assert.throws(() => agent.close(), /BUSY/);
    assert.deepEqual(agent.messages, []); agent.cancel();
    let heartbeat = false; await new Promise(resolve => setImmediate(() => { heartbeat = true; resolve(); })); assert.equal(heartbeat, true);
    held.release(); await outcome; assert.equal(models, 0);
    const snapshot = store.inspect({ agentId: 'a', sessionId: 's' }); assert.equal(snapshot.run.in_flight, null);
    const resumed = new Agent({ ...config, resume: true }); assert.equal((await resumed.resume()).content, reply.content); assert.equal(models, 1);
  } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
}));
test('cancelling a pending claim keeps its reservation without dispatching the model', () => fixture(async store => {
  let models = 0; const held = gate('claim'); let pending;
  const config = { model: 'test', store, agentId: 'a', sessionId: 's', provider: { complete() { models++; return reply; } } };
  try {
    const agent = new Agent(config); pending = agent.run('Work'); const outcome = assert.rejects(pending, /CANCELLED/); await held.ready;
    agent.cancel(); held.release(); await outcome; assert.equal(models, 0);
    const snapshot = store.inspect({ agentId: 'a', sessionId: 's' }); assert.equal(snapshot.run.in_flight.kind, 'model');
    assert.throws(() => new Agent({ ...config, resume: true }), /SESSION_OPERATION_UNCERTAIN/);
  } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
}));
test('known reply finishing during cancellation remains settled and resumes without another model', () => fixture(async store => {
  let models = 0; const held = gate('advance'); let pending;
  const objective = new Objective({ max_model_calls: 1, max_output_tokens: 8, max_total_output_tokens: 8 }, { store, id: 'root' });
  const config = { model: 'test', store, objective, agentId: 'a', sessionId: 's', provider: { complete() { models++; return reply; } } };
  try {
    const agent = new Agent(config); pending = agent.run('Work'); const outcome = assert.rejects(pending, /CANCELLED/); await held.ready;
    agent.cancel(); assert.throws(() => store.close(), /BUSY/); held.release(); await outcome;
    const snapshot = store.inspect({ agentId: 'a', sessionId: 's' }); assert.equal(snapshot.run.in_flight, null); assert.equal(snapshot.run.state.action.type, 'complete');
    assert.equal(objective.inspect().output_tokens, 2); assert.deepEqual(objective.inspect().reservations, {});
    assert.equal((await new Agent({ ...config, resume: true }).resume()).content, reply.content); assert.equal(models, 1);
  } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
}));
test('store closure invalidates durable commands and preserves cached getters', () => fixture(async store => {
  let models = 0; const agent = new Agent({ model: 'test', store, agentId: 'a', sessionId: 's', provider: { complete() { models++; return reply; } } });
  await agent.run('Work'); const messages = agent.messages; store.close();
  assert.deepEqual(agent.messages, messages); const source = agent.captureContext(0, 2); assert.equal(agent.readContext(source).length, 2);
  await assert.rejects(agent.run('Closed'), /CLOSED/); assert.equal(models, 1);
}));
