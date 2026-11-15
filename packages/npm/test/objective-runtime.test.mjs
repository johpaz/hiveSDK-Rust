import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createRequire } from 'node:module';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, Objective, SessionStore, VerificationUncertain, SummaryUncertain, TriggerUncertain } from '../index.mjs';
const native = createRequire(import.meta.url)('../native/hive.node');
const reply = { content: 'Known 🦀', usage: { input_tokens: 3, output_tokens: 2 } };
const policy = { max_model_calls: 8, max_output_tokens: 8, max_total_output_tokens: 64, max_depth: 2 };
function deferred() { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; }
async function fixture(callback) {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-runtime-')); const store = await SessionStore.open({ path: directory });
  try { await callback(store); } finally { await store.closeAsync(); rmSync(directory, { recursive: true, force: true }); }
}
async function sourceInput() {
  const agent = await Agent.open({ model: 'source', provider: { complete: () => reply } }); await agent.run('History');
  const source = agent.captureContext(0, 2); return { source, messages: agent.readContext(source) };
}
function gate(op, { after = false, nth = 1 } = {}) {
  const entered = deferred(); const released = deferred(); const original = native.NativeSessionStore.prototype.dispatchAsync; let seen = 0;
  native.NativeSessionStore.prototype.dispatchAsync = async function(command) {
    if (JSON.parse(command).command?.op === op && ++seen === nth) {
      const value = after ? await original.call(this, command) : undefined;
      entered.resolve(); await released.promise;
      return after ? value : original.call(this, command);
    }
    return original.call(this, command);
  };
  return { ready: entered.promise, release: released.resolve, restore() { native.NativeSessionStore.prototype.dispatchAsync = original; } };
}
async function action(objective, kind, callback, input) {
  if (kind === 'verify') return { run: signal => objective.verify('k', {}, callback, { signal }), uncertain: VerificationUncertain,
    resolveKnown: executionId => objective.resolveVerificationAsync({ key: 'k', executionId, verdict: { passed: true } }),
    resolve: () => objective.resolveVerificationAsync({ key: 'k', executionId: 'stale', verdict: { passed: true } }) };
  if (kind === 'summary') return { run: signal => objective.summarize('k', input, callback, { signal }), uncertain: SummaryUncertain,
    resolveKnown: executionId => objective.resolveSummaryAsync({ key: 'k', executionId, content: 'Known' }),
    resolve: () => objective.resolveSummaryAsync({ key: 'k', executionId: 'stale', content: 'Known' }) };
  const trigger = await objective.triggerAsync('event', { kind: 'event' }, callback); await trigger.enableAsync();
  return { run: signal => trigger.emit('k', {}, { signal }), uncertain: TriggerUncertain, cancel: () => trigger.cancel(),
    resolveKnown: executionId => trigger.resolveAsync({ occurrenceKey: 'k', executionId, result: 'Known' }),
    resolve: () => trigger.resolveAsync({ occurrenceKey: 'k', executionId: 'stale', result: 'Known' }) };
}
test('durable run, delegation, summaries, verification and watchers never use the synchronous store bridge', () => fixture(async store => {
  const objective = await Objective.open(policy, { store, id: 'root' });
  const source = await Agent.open({ model: 'source', store, agentId: 'source', sessionId: 's', provider: { complete: () => reply } }); await source.run('History');
  const worker = await Agent.open({ model: 'worker', store, objective, agentId: 'worker', sessionId: 's', provider: { complete: () => reply } }); let parentCalls = 0;
  const parent = await Agent.open({ model: 'parent', store, objective, agentId: 'parent', sessionId: 's',
    provider: { complete: () => ++parentCalls === 1 ? { ...reply, content: null, tool_calls: [{ id: 'c', name: 'delegate', arguments: {} }] } : reply },
    tools: [{ name: 'delegate', description: 'Delegate', parameters: { type: 'object' }, execute: async (_, ctx) => (await ctx.delegate(worker, 'Work')).content }] });
  const original = native.NativeSessionStore.prototype.dispatch;
  native.NativeSessionStore.prototype.dispatch = () => assert.fail('synchronous store entry point');
  objective._dispatch = () => assert.fail('synchronous objective control');
  try {
    const result = await objective.verify('v', {}, async (_, ctx) => { await ctx.delegate(worker, 'Review'); return { passed: true }; }, { modelReview: { evaluationId: 'fixture', justification: 'explicit check' } });
    assert.equal(result.verdict.passed, true);
    assert.equal((await source.summarize('s', source.captureContext(0, 2), async (_, ctx) => (await ctx.delegate(worker, 'Summary')).content, { objective })).summary.content, reply.content);
    assert.equal((await parent.run('Task')).content, reply.content);
    const event = await objective.triggerAsync('event', { kind: 'event' }, async (_, ctx) => (await ctx.delegate(worker, 'Event')).content); await event.enableAsync();
    assert.equal((await event.emit('one', {})).result, reply.content); await event.emit('one', {});
    const once = await objective.triggerAsync('once', { kind: 'at', unixMs: Date.now() - 1 }, () => 'done'); await once.enableAsync();
    assert.equal((await Array.fromAsync(once.watch())).length, 1); await once.disableAsync();
    assert.equal((await objective.inspectAsync()).model_calls, 6);
  } finally { native.NativeSessionStore.prototype.dispatch = original; }
}));
test('pending callback claims retain the scope, prohibit reconciliation and cancel without effects', () => fixture(async store => {
  const input = await sourceInput();
  for (const kind of ['verify', 'summary', 'trigger']) {
    const objective = await Objective.open(policy, { store, id: kind }); let calls = 0;
    const entry = await action(objective, kind, () => { calls++; return kind === 'verify' ? { passed: true } : 'Known'; }, input);
    const held = gate(kind === 'trigger' ? 'trigger_emit' : `${kind === 'summary' ? 'summary' : 'verify'}_begin`); const controller = new AbortController(); let pending;
    try {
      pending = entry.run(controller.signal); const outcome = assert.rejects(pending, /CANCELLED/); await held.ready;
      assert.throws(() => store.close(), /BUSY/); await assert.rejects(entry.resolve(), /BUSY/);
      if (entry.cancel) assert.equal(entry.cancel(), true); else controller.abort();
      held.release(); await outcome; assert.equal(calls, 0); assert.equal((await objective.inspectAsync()).tool_calls, 1);
      await assert.rejects(entry.run(), entry.uncertain); assert.equal(calls, 0);
      const state = await objective.inspectAsync();
      const id = kind === 'trigger' ? state.triggers.event.executions.k.execution_id : kind === 'summary' ? state.summaries.k.execution_id : state.verifications.k.execution_id;
      await assert.rejects(entry.resolve(), /SESSION_CONFLICT/); await entry.resolveKnown(id); assert.ok(await entry.run()); assert.equal(calls, 0);
    } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
  }
}));
test('known finalizations join their worker and cached results survive a stopped root', () => fixture(async store => {
  const input = await sourceInput();
  for (const kind of ['verify', 'summary', 'trigger']) {
    const objective = await Objective.open(policy, { store, id: kind }); let calls = 0;
    const entry = await action(objective, kind, () => { calls++; return kind === 'verify' ? { passed: true } : 'Known'; }, input);
    const held = gate(`${kind === 'trigger' ? 'trigger' : kind === 'summary' ? 'summary' : 'verify'}_finish`); const controller = new AbortController(); let pending;
    try {
      pending = entry.run(controller.signal); const outcome = assert.rejects(pending, /CANCELLED/); await held.ready;
      controller.abort(); assert.throws(() => store.close(), /BUSY/); held.release(); await outcome;
      await objective.cancelAsync(); const cached = await entry.run(); assert.ok(cached); assert.equal(calls, 1); assert.equal((await objective.inspectAsync()).tool_calls, 1);
    } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
  }
}));
test('cancellation during an agent root check does not dispatch or leak an active scope', () => fixture(async store => {
  const objective = await Objective.open(policy, { store, id: 'root' }); let models = 0;
  const agent = await Agent.open({ model: 'test', store, objective, agentId: 'a', sessionId: 's', provider: { complete() { models++; return reply; } } });
  const held = gate('check'); let pending;
  try {
    pending = agent.run('Work'); const outcome = assert.rejects(pending, /CANCELLED/); await held.ready;
    await objective.cancelAsync(); assert.throws(() => store.close(), /BUSY/); held.release(); await outcome;
    assert.equal(models, 0); assert.equal(await store.inspectAsync({ agentId: 'a', sessionId: 's' }), null);
  } finally { held.release(); if (pending) await Promise.allSettled([pending]); held.restore(); }
}));
test('disable completing before a stale async inspection returns cannot strand the watcher', () => fixture(async store => {
  const objective = await Objective.open(policy, { store, id: 'root' });
  const trigger = await objective.triggerAsync('future', { kind: 'at', unixMs: Date.now() + 60_000 }, () => assert.fail('not due')); await trigger.enableAsync();
  const held = gate('trigger_inspect', { after: true, nth: 3 }); const iterator = trigger.watch(); let pending; let timer;
  try {
    pending = iterator.next(); await held.ready; await assert.rejects(trigger.watch().next(), /BUSY/);
    await trigger.disableAsync(); held.release();
    const result = await Promise.race([pending, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('lost watcher wake')), 2000); })]);
    assert.equal(result.done, true); assert.equal((await objective.inspectAsync()).tool_calls, 0);
  } finally { clearTimeout(timer); held.release(); trigger.cancel(); if (pending) await Promise.allSettled([pending]); await iterator.return(); held.restore(); }
}));
