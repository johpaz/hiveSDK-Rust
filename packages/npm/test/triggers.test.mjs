import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, Objective, ObjectiveLimit, SessionStore, TriggerUncertain } from '../index.mjs';

test('events are disabled by default, isolate input and cache a confirmed null result', async () => {
  const objective = new Objective(); let calls = 0; let context;
  const trigger = objective.trigger('receipts', { kind: 'event' }, (value, ctx) => {
    calls++; context = ctx; value.n = 99; return null;
  });
  const input = { n: 1 };
  assert.equal(await trigger.emit('r1', input), null);
  assert.equal(calls, 0); assert.equal(objective.inspect().tool_calls, 0);
  trigger.enable(); const receipt = await trigger.emit('r1', input);
  assert.equal(receipt.result, null); assert.equal(input.n, 1);
  assert.deepEqual(Object.keys(context).sort(), ['execution_id', 'objective_id', 'occurrence_key', 'trigger_id']);
  assert.deepEqual(await trigger.emit('r1', input), receipt);
  assert.equal(calls, 1); assert.equal(objective.inspect().tool_calls, 1);
  await assert.rejects(trigger.emit('r1', { n: 2 }), /already bound/);
  assert.throws(() => context.delegate(null, 'late'), /CLOSED/);
  await assert.rejects(trigger.tick(), /calendar/);
  await assert.rejects(trigger.watch().next(), /Event triggers/);
});

test('delayed intervals coalesce and never catch up with a burst', async () => {
  const objective = new Objective(); let captured;
  const trigger = objective.trigger('interval', { kind: 'interval', everyMs: 60_000, anchorUnixMs: Date.now() - 600_100 }, value => { captured = value; return 'done'; });
  trigger.enable(); assert.equal((await trigger.tick()).result, 'done');
  assert.ok(captured.skipped_occurrences >= 9);
  assert.ok(trigger.inspect().next_due_unix_ms > Date.now());
  assert.equal(await trigger.tick(), null); assert.equal(objective.inspect().tool_calls, 1);
});

test('a one-shot watcher runs only when consumed and ends after its single allowance', async () => {
  const objective = new Objective({ max_tool_calls: 1 }); let calls = 0;
  const trigger = objective.trigger('once', { kind: 'at', unixMs: Date.now() - 1 }, () => ++calls);
  assert.deepEqual(await Array.fromAsync(trigger.watch()), []);
  trigger.enable(); const watcher = trigger.watch();
  assert.equal(calls, 0);
  const receipts = await Array.fromAsync(watcher);
  assert.equal(receipts.length, 1); assert.equal(receipts[0].result, 1);
  assert.deepEqual(await Array.fromAsync(trigger.watch()), []);
  assert.equal(calls, 1); assert.equal(objective.inspect().model_calls, 0);
});

test('idle watcher disable/cancel wakes promptly and releases the durable store', async () => {
  for (const cancel of [false, true]) {
    const directory = mkdtempSync(path.join(tmpdir(), 'hive-trigger-'));
    const store = new SessionStore({ path: directory });
    try {
      const objective = new Objective({}, { store, id: 'root' });
      const trigger = objective.trigger('future', { kind: 'at', unixMs: Date.now() + 60_000 }, () => assert.fail('no effect'));
      trigger.enable(); const pending = trigger.watch().next();
      await new Promise(resolve => setImmediate(resolve));
      assert.throws(() => store.close(), /BUSY/);
      await assert.rejects(trigger.watch().next(), /BUSY/);
      if (cancel) { assert.equal(trigger.cancel(), true); await assert.rejects(pending, /CANCELLED/); }
      else { trigger.disable(); assert.equal((await pending).done, true); }
      assert.equal(trigger.cancel(), false); assert.equal(objective.inspect().tool_calls, 0);
    } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
  }
});

test('unknown outcome blocks other events until fenced reconciliation', async () => {
  const objective = new Objective(); let calls = 0;
  const trigger = objective.trigger('event', { kind: 'event' }, () => { calls++; throw new Error('unknown effect'); });
  trigger.enable(); await assert.rejects(trigger.emit('one', {}), TriggerUncertain);
  await assert.rejects(trigger.emit('two', {}), TriggerUncertain);
  const state = trigger.inspect(); const executionId = state.executions.one.execution_id;
  assert.throws(() => trigger.resolve({ occurrenceKey: 'one', executionId: 'stale', result: null }), /SESSION_CONFLICT/);
  const receipt = trigger.resolve({ occurrenceKey: 'one', executionId, result: null });
  assert.deepEqual(await trigger.emit('one', {}), receipt); assert.equal(calls, 1);
  assert.equal(objective.inspect().tool_calls, 1);
});

test('trigger delegation shares model and output allowance across occurrences', async () => {
  const objective = new Objective({ max_model_calls: 1, max_output_tokens: 10, max_total_output_tokens: 10 }); let calls = 0;
  const agent = new Agent({ model: 'fixture', provider: { complete(request) {
    calls++; assert.equal(request.max_output_tokens, 10);
    return { content: 'done', usage: { input_tokens: 2, output_tokens: 3 } };
  } } });
  try {
    const trigger = objective.trigger('work', { kind: 'event' }, (_, context) => context.delegate(agent, 'Work'));
    trigger.enable(); assert.equal((await trigger.emit('one', {})).result.content, 'done');
    await assert.rejects(trigger.emit('two', {}), ObjectiveLimit);
    assert.equal(calls, 1); assert.equal(objective.inspect().output_tokens, 3);
  } finally { agent.close(); }
});

test('known late result commits before local cancellation is reported', async () => {
  const objective = new Objective(); let enter; let release;
  const entered = new Promise(resolve => { enter = resolve; });
  const gate = new Promise(resolve => { release = resolve; });
  const trigger = objective.trigger('event', { kind: 'event' }, async () => { enter(); await gate; return 'confirmed'; });
  trigger.enable(); const pending = trigger.emit('one', {}); await entered;
  assert.throws(() => trigger.resolve({ occurrenceKey: 'one', executionId: 'x', result: null }), /BUSY/);
  assert.equal(trigger.cancel(), true); release(); await assert.rejects(pending, /CANCELLED/);
  assert.equal((await trigger.emit('one', {})).result, 'confirmed');
  assert.equal(objective.inspect().tool_calls, 1);
});

test('reopening preserves enabled state, calendar and cached outcomes', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-trigger-reopen-')); let store;
  try {
    store = new SessionStore({ path: directory }); let objective = new Objective({}, { store, id: 'root' });
    let trigger = objective.trigger('interval', { kind: 'interval', everyMs: 60_000 }, () => 'work');
    trigger.enable(); const before = trigger.inspect(); store.close();
    store = new SessionStore({ path: directory }); objective = new Objective({}, { store, id: 'root' });
    trigger = objective.trigger('interval', { kind: 'interval', everyMs: 60_000 }, () => assert.fail('not due'));
    assert.deepEqual(trigger.inspect(), before); assert.equal(await trigger.tick(), null);
    assert.throws(() => objective.trigger('interval', { kind: 'interval', everyMs: 1 }, () => null), /schedule/);
  } finally { store?.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('future watcher waits for its date and deadline stops idle work without effects', async () => {
  let calls = 0;
  const objective = new Objective(); const due = Date.now() + 40;
  const trigger = objective.trigger('future', { kind: 'at', unixMs: due }, () => { calls++; assert.ok(Date.now() >= due); return 'due'; });
  trigger.enable(); assert.equal((await Array.fromAsync(trigger.watch()))[0].result, 'due');
  assert.equal(calls, 1);
  const limited = new Objective({ max_run_ms: 40 });
  const idle = limited.trigger('idle', { kind: 'at', unixMs: Date.now() + 60_000 }, () => assert.fail('deadline must prevent effect'));
  idle.enable(); await assert.rejects(Array.fromAsync(idle.watch()), /DEADLINE|OBJECTIVE_LIMIT/);
  assert.equal(limited.inspect().tool_calls, 0); assert.equal(idle.cancel(), false);
});

test('a cron trigger claims its latest due occurrence once, in its zone, and rejects invalid expressions', async () => {
  const objective = new Objective(); const inputs = [];
  const trigger = objective.trigger('cron', { kind: 'cron', expression: '* * * * * *', timezone: 'America/Bogota' }, value => { inputs.push(value); return 'done'; });
  assert.deepEqual(trigger.inspect().schedule, { kind: 'cron', expression: '* * * * * *', timezone: 'America/Bogota', dom_and_dow: false });
  assert.equal(await trigger.tick(), null, 'disabled triggers never claim');
  trigger.enable();
  assert.equal(await trigger.tick(), null, 'the first occurrence is not due yet');
  await new Promise(resolve => setTimeout(resolve, 1300));
  assert.equal((await trigger.tick()).result, 'done');
  assert.equal(inputs.length, 1);
  assert.ok(inputs[0].scheduled_for_unix_ms <= inputs[0].observed_unix_ms && inputs[0].skipped_is_lower_bound === undefined);
  assert.ok(trigger.inspect().next_due_unix_ms > inputs[0].observed_unix_ms - 1000);
  assert.equal(objective.inspect().tool_calls, 1);
  for (const schedule of [{ kind: 'cron', expression: '80 * * * *', timezone: 'UTC' }, { kind: 'cron', expression: '* * * * *', timezone: 'Mars/Base' }, { kind: 'cron', expression: '', timezone: 'UTC' }]) {
    assert.throws(() => objective.trigger('bad', schedule, () => null), /cron|schedule|timezone|zone/i, JSON.stringify(schedule));
  }
});

test('a cron trigger and its next occurrence survive reopening the store', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-trigger-cron-')); let store;
  try {
    store = new SessionStore({ path: directory }); let objective = new Objective({}, { store, id: 'root' });
    let trigger = objective.trigger('daily', { kind: 'cron', expression: '0 8 * * *', timezone: 'America/Bogota' }, () => 'work');
    trigger.enable(); const before = trigger.inspect();
    assert.equal(before.next_due_unix_ms % 86_400_000, 13 * 3_600_000, '08:00 in Bogota is 13:00 UTC');
    store.close();
    store = new SessionStore({ path: directory }); objective = new Objective({}, { store, id: 'root' });
    trigger = objective.trigger('daily', { kind: 'cron', expression: '0 8 * * *', timezone: 'America/Bogota' }, () => assert.fail('not due'));
    assert.deepEqual(trigger.inspect(), before); assert.equal(await trigger.tick(), null);
    assert.throws(() => objective.trigger('daily', { kind: 'cron', expression: '0 9 * * *', timezone: 'America/Bogota' }, () => null), /schedule/);
  } finally { store?.close(); rmSync(directory, { recursive: true, force: true }); }
});
