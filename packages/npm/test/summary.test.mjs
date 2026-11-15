import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, Objective, ObjectiveLimit, SessionStore, SummaryUncertain } from '../index.mjs';

const policy = { max_model_calls: 3, max_output_tokens: 10, max_total_output_tokens: 30 };
async function subject(store) {
  const agent = new Agent({ model: 'source', provider: { complete() { return { content: 'R7 viernes' }; } },
    ...(store ? { store, agentId: 'subject', sessionId: 's' } : {}) });
  if (!agent.messages.length) await agent.run('Pedido R7');
  return agent;
}

test('explicit summary generation shares the root budget, caches and installs only on request', async () => {
  for (const durable of [false, true]) {
    const dir = mkdtempSync(path.join(tmpdir(), 'hive-summary-')); const store = durable ? new SessionStore({ path: dir }) : undefined;
    const objective = new Objective(policy, store ? { store, id: 'root' } : undefined); const agent = await subject(store); let calls = 0; let context;
    const worker = new Agent({ model: 'summary', ...(store ? { store, objective, agentId: 'worker', sessionId: 's' } : {}),
      provider: { complete(request) { calls++; assert.equal(request.max_output_tokens, 10); return { content: 'R7 viernes', usage: { input_tokens: 2, output_tokens: 3 } }; } } });
    try {
      const source = agent.captureContext(0, 2); const before = agent.messages;
      const result = await agent.summarize('s', source, async (value, ctx) => { context = ctx; return (await ctx.delegate(worker, JSON.stringify(value.messages))).content; }, { objective });
      assert.equal(result.cost.model_calls, 1); assert.equal(result.cost.tool_calls, 1); assert.equal(result.cost.input_tokens, 2); assert.equal(result.cost.output_tokens, 3);
      assert.equal(result.cost.pending_model_calls, 0); assert.equal(result.cost.reserved_output_tokens, 0);
      assert.deepEqual(agent.messages, before); assert.equal(agent.contextPlan, null);
      assert.deepEqual(Object.keys(context).sort(), ['execution_id', 'objective_id', 'summary_key']);
      assert.throws(() => context.delegate(worker, 'Late'), /CLOSED/);
      assert.deepEqual(await agent.summarize('s', source, () => assert.fail('no replay'), { objective }), result);
      assert.equal(calls, 1); agent.setContextPlan({ summary: result.summary });
      assert.equal(agent.contextPlan.summary.id, 's');
    } finally { worker.close(); agent.close(); store?.close(); rmSync(dir, { recursive: true, force: true }); }
  }
});

test('programmatic callbacks need no models and uncertain text requires fenced resolution', async () => {
  const agent = await subject(); const objective = new Objective(policy); const source = agent.captureContext(0, 2); let callbacks = 0;
  try {
    const result = await agent.summarize('pure', source, value => { value.messages[0].content = 'changed'; return 'R7 viernes'; }, { objective });
    assert.equal(result.cost.model_calls, 0); assert.equal(agent.messages[0].content, 'Pedido R7');
    await assert.rejects(agent.summarize('bad', source, () => { callbacks++; return {}; }, { objective }), SummaryUncertain);
    await assert.rejects(agent.summarize('bad', source, () => assert.fail('no replay'), { objective }), SummaryUncertain);
    const executionId = objective.inspect().summaries.bad.execution_id;
    assert.throws(() => objective.resolveSummary({ key: 'bad', executionId: 'stale', content: 'fact' }), /SESSION_CONFLICT/);
    const known = objective.resolveSummary({ key: 'bad', executionId, content: 'R7 viernes' });
    assert.deepEqual(await agent.summarize('bad', source, () => assert.fail('no replay'), { objective }), known);
    assert.equal(callbacks, 1); assert.equal(objective.inspect().tool_calls, 2);
  } finally { agent.close(); }
});

test('simultaneous summaries exclude unrelated worker usage from their own cost', async () => {
  const agent = await subject(); const objective = new Objective(policy); const source = agent.captureContext(0, 2); const input = { source, messages: agent.readContext(source) };
  let arrivals = 0; let release; const gate = new Promise(resolve => { release = resolve; });
  const workers = [[3, 2], [7, 4], [1, 1]].map(([input_tokens, output_tokens]) => new Agent({ model: 'fixture', provider: { async complete() {
    if (++arrivals === 3) release(); await gate; return { content: 'fact', usage: { input_tokens, output_tokens } };
  } }, ...(input_tokens === 1 ? { objective } : {}) }));
  try {
    const [a, b] = await Promise.all([
      objective.summarize('a', input, async (_, ctx) => (await ctx.delegate(workers[0], 'A')).content),
      objective.summarize('b', input, async (_, ctx) => (await ctx.delegate(workers[1], 'B')).content), workers[2].run('Other work')]);
    assert.equal(a.cost.input_tokens, 3); assert.equal(b.cost.input_tokens, 7); assert.equal(objective.inspect().input_tokens, 11);
    assert.equal(a.cost.model_calls, 1); assert.equal(b.cost.model_calls, 1);
  } finally { release(); workers.forEach(worker => worker.close()); agent.close(); }
});

test('nested delegation inherits summary attribution and cannot bypass depth', async () => {
  for (const depth of [1, 2]) {
    const agent = await subject(); const objective = new Objective({ ...policy, max_depth: depth }); let calls = 0;
    const leaf = new Agent({ model: 'leaf', provider: { complete() { calls++; return { content: 'fact', usage: { input_tokens: 2, output_tokens: 1 } }; } } });
    let workerCalls = 0;
    const worker = new Agent({ model: 'worker', tools: [{ name: 'delegate', description: 'Delegate', parameters: { type: 'object' }, execute: (_, ctx) => ctx.delegate(leaf, 'Check fact').then(result => result.content) }],
      provider: { complete() { calls++; return ++workerCalls === 1 ? { content: null, tool_calls: [{ id: 'c', name: 'delegate', arguments: {} }], usage: { input_tokens: 3, output_tokens: 1 } } : { content: 'R7 viernes', usage: { input_tokens: 4, output_tokens: 2 } }; } } });
    try {
      const pending = agent.summarize('s', agent.captureContext(0, 2), async (_, ctx) => (await ctx.delegate(worker, 'Summarize')).content, { objective });
      if (depth === 1) { await assert.rejects(pending, ObjectiveLimit); assert.equal(calls, 1); }
      else { const result = await pending; assert.equal(result.cost.model_calls, 3); assert.equal(result.cost.tool_calls, 2); assert.equal(result.cost.input_tokens, 9); assert.equal(result.cost.output_tokens, 4); }
    } finally { worker.close(); leaf.close(); agent.close(); }
  }
});

test('failed callbacks cancel and join their unawaited children without refunding unknown output', async () => {
  const agent = await subject(); const objective = new Objective(policy); let enter; let aborted = false;
  const entered = new Promise(resolve => { enter = resolve; });
  const worker = new Agent({ model: 'worker', provider: { async complete(_, { signal }) { enter(); await new Promise((_, reject) => signal.addEventListener('abort', () => { aborted = true; reject(new Error('abort')); }, { once: true })); } } });
  try {
    await assert.rejects(agent.summarize('s', agent.captureContext(0, 2), async (_, ctx) => { ctx.delegate(worker, 'Work'); await entered; throw new Error('failed'); }, { objective }), SummaryUncertain);
    assert.equal(aborted, true); assert.equal(objective.inspect().summary_costs.s.pending_model_calls, 1);
    assert.equal(objective.inspect().summary_costs.s.reserved_output_tokens, 10);
  } finally { worker.close(); agent.close(); }
});

test('known late summary commits before cancellation and keeps the store busy until settlement', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-summary-cancel-')); const store = new SessionStore({ path: dir });
  const objective = new Objective(policy, { store, id: 'root' }); const agent = await subject(store);
  let enter; let release; const entered = new Promise(resolve => { enter = resolve; }); const gate = new Promise(resolve => { release = resolve; });
  try {
    const source = agent.captureContext(0, 2);
    const pending = agent.summarize('s', source, async () => { enter(); await gate; return 'R7 viernes'; }, { objective }); await entered;
    assert.throws(() => store.close(), /BUSY/); assert.throws(() => agent.close(), /BUSY/);
    assert.equal(agent.cancel(), true); release(); await assert.rejects(pending, /CANCELLED/);
    assert.equal((await agent.summarize('s', source, () => assert.fail('no replay'), { objective })).summary.content, 'R7 viernes');
  } finally { release(); agent.close(); store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('actual process exit leaves the summary pending until explicit reconciliation', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-summary-crash-')); let store;
  const agent = await subject(); const source = agent.captureContext(0, 2); const input = { source, messages: agent.readContext(source) };
  try {
    const script = `import {writeFileSync} from 'node:fs';import {join} from 'node:path';import {Objective,SessionStore} from ${JSON.stringify(new URL('../index.mjs', import.meta.url).href)};
      const store=new SessionStore({path:process.argv[1]});const root=new Objective(${JSON.stringify(policy)},{store,id:'root'});
      await root.summarize('s',JSON.parse(process.argv[2]),()=>{writeFileSync(join(process.argv[1],'effect'),'confirmed');process.exit(19)});`;
    const child = spawnSync(process.execPath, ['--input-type=module', '-e', script, dir, JSON.stringify(input)], { encoding: 'utf8', timeout: 30_000 });
    assert.equal(child.status, 19, child.stderr); assert.equal(readFileSync(path.join(dir, 'effect'), 'utf8'), 'confirmed');
    store = new SessionStore({ path: dir }); const objective = new Objective(policy, { store, id: 'root' });
    await assert.rejects(agent.summarize('s', source, () => assert.fail('no replay'), { objective }), SummaryUncertain);
    objective.resolveSummary({ key: 's', executionId: objective.inspect().summaries.s.execution_id, content: 'R7 viernes' });
    const result = await agent.summarize('s', source, () => assert.fail('no replay'), { objective });
    assert.equal(result.cost.model_calls, 0); assert.equal(result.cost.tool_calls, 1);
  } finally { agent.close(); store?.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('a clean durable child checkpoint resumes in the original summary scope', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-summary-resume-')); let store = new SessionStore({ path: dir });
  const rootPolicy = { max_model_calls: 2, max_output_tokens: 8, max_total_output_tokens: 10 };
  let objective = new Objective(rootPolicy, { store, id: 'root' }); const agent = await subject(); let calls = 0;
  const tools = [{ name: 'read', description: 'Read', parameters: { type: 'object' }, execute: () => 5 }];
  let worker = new Agent({ model: 'summary', store, objective, agentId: 'worker', sessionId: 's', tools,
    provider: { complete() { calls++; return { content: null, tool_calls: [{ id: 'c', name: 'read', arguments: {} }], usage: { input_tokens: 2, output_tokens: 3 } }; } },
    onEvent(action) { if (action.type === 'model' && calls) throw new Error('pause'); } });
  try {
    const source = agent.captureContext(0, 2);
    await assert.rejects(agent.summarize('s', source, async (_, ctx) => (await ctx.delegate(worker, 'Work')).content, { objective }), SummaryUncertain);
    worker.close(); store.close(); store = new SessionStore({ path: dir }); objective = new Objective(rootPolicy, { store, id: 'root' });
    assert.equal(store.inspect({ agentId: 'worker', sessionId: 's' }).run.summary_key, 's');
    worker = new Agent({ model: 'summary', store, objective, agentId: 'worker', sessionId: 's', resume: true,
      tools: tools.map(tool => ({ ...tool, execute: () => assert.fail('no tool replay') })),
      provider: { complete(request) { calls++; assert.equal(request.max_output_tokens, 7); return { content: 'R7 viernes', usage: { input_tokens: 4, output_tokens: 2 } }; } } });
    const done = await worker.resume();
    objective.resolveSummary({ key: 's', executionId: objective.inspect().summaries.s.execution_id, content: done.content });
    const known = await agent.summarize('s', source, () => assert.fail('no callback replay'), { objective });
    assert.equal(known.cost.model_calls, 2); assert.equal(known.cost.tool_calls, 2);
    assert.equal(known.cost.input_tokens, 6); assert.equal(known.cost.output_tokens, 5); assert.equal(calls, 2);
  } finally { worker.close(); agent.close(); store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('finishing a summary never cancels a completed child reused by another task', async () => {
  const agent = await subject(); const objective = new Objective(policy); let calls = 0; let aborted = false;
  let ready; let releaseSummary; let started; let releaseOther;
  const summaryReady = new Promise(resolve => { ready = resolve; });
  const summaryGate = new Promise(resolve => { releaseSummary = resolve; });
  const otherStarted = new Promise(resolve => { started = resolve; });
  const otherGate = new Promise(resolve => { releaseOther = resolve; });
  const worker = new Agent({ model: 'worker', provider: { async complete(_, { signal }) {
    if (++calls === 1) return { content: 'fact', usage: { input_tokens: 2, output_tokens: 1 } };
    started(); await Promise.race([otherGate, new Promise((_, reject) => signal.addEventListener('abort', () => { aborted = true; reject(new Error('cancelled unrelated work')); }, { once: true }))]);
    return { content: 'other result' };
  } } });
  let other;
  const pending = agent.summarize('s', agent.captureContext(0, 2), async (_, ctx) => {
    const result = await ctx.delegate(worker, 'Fact'); ready(); await summaryGate; return result.content;
  }, { objective });
  try {
    await summaryReady; other = worker.run('Other task'); other.catch(() => {}); await otherStarted;
    releaseSummary(); await pending; assert.equal(aborted, false);
    releaseOther(); assert.equal((await other).content, 'other result');
  } finally { releaseSummary(); releaseOther(); await Promise.allSettled([pending, other]); worker.close(); agent.close(); }
});

test('delegating to an already busy agent fails without cancelling its existing work', async () => {
  const agent = await subject(); const objective = new Objective(policy); let enter; let release; let aborted = false;
  const entered = new Promise(resolve => { enter = resolve; }); const gate = new Promise(resolve => { release = resolve; });
  const worker = new Agent({ model: 'worker', provider: { async complete(_, { signal }) {
    enter(); await Promise.race([gate, new Promise((_, reject) => signal.addEventListener('abort', () => { aborted = true; reject(new Error('wrong cancellation')); }, { once: true }))]);
    return { content: 'other result' };
  } } });
  const other = worker.run('Other work'); other.catch(() => {});
  try {
    await entered;
    const result = await agent.summarize('s', agent.captureContext(0, 2), (_, ctx) => { assert.throws(() => ctx.delegate(worker, 'Busy'), /BUSY/); return 'R7 viernes'; }, { objective });
    assert.equal(result.cost.model_calls, 0); assert.equal(aborted, false); release(); await other;
  } finally { release(); await Promise.allSettled([other]); worker.close(); agent.close(); }
});
