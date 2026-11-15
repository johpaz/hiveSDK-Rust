import test from 'node:test';
import assert from 'node:assert/strict';
import { Agent, Objective, ObjectiveLimit, ToolOperationUncertain } from '../index.mjs';

const reply = (content = 'done', output = 1) => ({ content, usage: { input_tokens: 1, output_tokens: output } });
const call = (arguments_ = {}) => ({ ...reply(null), tool_calls: [{ id: 'delegate-1', name: 'delegate', arguments: arguments_ }] });
const tool = execute => ({ name: 'delegate', description: 'Delegate explicitly', parameters: { type: 'object' }, execute });

test('parent and child share calls without a fresh allowance on another run', async () => {
  const objective = new Objective({ max_model_calls: 2 });
  let calls = 0;
  const child = new Agent({ model: 'child', provider: { complete: () => { calls++; return reply(); } } });
  const parent = new Agent({ model: 'parent', objective,
    provider: { complete: () => { calls++; return call(); } },
    tools: [tool(async (_, context) => (await context.delegate(child, 'Work')).content)] });
  await assert.rejects(parent.run('Work'), ObjectiveLimit);
  assert.equal(calls, 2);
  assert.equal(objective.inspect().model_calls, 2);
  assert.equal(objective.inspect().tool_calls, 1);
  await assert.rejects(parent.run('Again'), ObjectiveLimit);
  assert.equal(calls, 2);
  parent.close(); child.close();
});

test('delegation reconciles one output reservation and reduces each subsequent cap', async () => {
  const objective = new Objective({ max_output_tokens: 10, max_total_output_tokens: 10 });
  const caps = [];
  let parentCalls = 0;
  const child = new Agent({ model: 'child', provider: { complete: request => {
    caps.push(request.max_output_tokens); return reply('child', 2);
  } } });
  const parent = new Agent({ model: 'parent', objective, provider: { complete: request => {
    caps.push(request.max_output_tokens); return parentCalls++ ? reply('finished', 3) : call();
  } }, tools: [tool(async (_, context) => (await context.delegate(child, 'Work')).content)] });
  assert.equal((await parent.run('Work')).content, 'finished');
  assert.deepEqual(caps, [10, 9, 7]);
  const snapshot = objective.inspect();
  assert.equal(snapshot.model_calls, 3); assert.equal(snapshot.input_tokens, 3); assert.equal(snapshot.output_tokens, 6);
  assert.deepEqual(snapshot.reservations, {});
  parent.close(); child.close();
});

test('depth and changing objective are rejected before any child inference', async () => {
  for (const different of [false, true]) {
    const objective = new Objective({ max_depth: different ? 2 : 0 });
    let childCalls = 0;
    const child = new Agent({ model: 'child', objective: different ? new Objective() : undefined,
      provider: { complete: () => { childCalls++; return reply(); } } });
    const parent = new Agent({ model: 'parent', objective, provider: { complete: () => call() },
      tools: [tool((_, context) => context.delegate(child, 'Work'))] });
    await assert.rejects(parent.run('Work'), ObjectiveLimit);
    assert.equal(childCalls, 0); assert.equal(objective.inspect().model_calls, 1);
    parent.close(); child.close();
  }
});

test('schema rejections consume the shared tool budget before any callback', async () => {
  const objective = new Objective({ max_tool_calls: 1 });
  let calls = 0; let effects = 0;
  const parent = new Agent({ model: 'parent', objective, provider: { complete: () => call(calls++ ? { n: 2 } : {}) },
    tools: [{ ...tool(() => { effects++; return true; }), parameters: { type: 'object', required: ['n'], properties: { n: { type: 'integer' } } } } ] });
  await assert.rejects(parent.run('Work'), ObjectiveLimit);
  assert.equal(calls, 2); assert.equal(effects, 0); assert.equal(objective.inspect().tool_calls, 1);
  parent.close();
});

test('unknown usage and lost replies retain reservations without a retry', async () => {
  for (const fail of [false, true]) {
    const objective = new Objective(); let calls = 0;
    const agent = new Agent({ model: 'test', objective, provider: { complete: () => {
      calls++; if (fail) throw new Error('response lost'); return { content: 'unmetered' };
    } } });
    await assert.rejects(agent.run('Work'), fail ? /response lost/ : ObjectiveLimit);
    assert.equal(calls, 1); assert.deepEqual(objective.inspect().reservations, { 1: 2048 });
    assert.equal(objective.inspect().stopped, fail ? null : 'usage_unavailable');
    agent.close();
  }
});

test('objective cancellation propagates through a running child and retains its reservation', async () => {
  const objective = new Objective();
  let enter; const entered = new Promise(resolve => { enter = resolve; });
  let aborted = false;
  const child = new Agent({ model: 'child', provider: { complete: async (_, { signal }) => {
    enter(); await new Promise((_, reject) => signal.addEventListener('abort', () => {
      aborted = true; reject(new Error('child aborted'));
    }, { once: true }));
  } } });
  const parent = new Agent({ model: 'parent', objective, provider: { complete: () => call() },
    tools: [tool((_, context) => context.delegate(child, 'Work'))] });
  const pending = parent.run('Work'); await entered; objective.cancel();
  await assert.rejects(pending, /CANCELLED/);
  assert.equal(aborted, true); assert.equal(objective.inspect().model_calls, 2);
  assert.equal(Object.keys(objective.inspect().reservations).length, 1);
  await assert.rejects(parent.run('Again'), ObjectiveLimit);
  parent.close(); child.close();
});

test('parent joins an unawaited child and rejects delegation after callback scope', async () => {
  const objective = new Objective();
  let release; const gate = new Promise(resolve => { release = resolve; });
  let enter; const entered = new Promise(resolve => { enter = resolve; });
  let parentCalls = 0; let captured;
  const child = new Agent({ model: 'child', provider: { complete: async () => { enter(); await gate; return reply(); } } });
  const parent = new Agent({ model: 'parent', objective, provider: { complete: () => parentCalls++ ? reply() : call() },
    tools: [tool((_, context) => { captured = context; context.delegate(child, 'Work'); return 'started'; })] });
  const pending = parent.run('Work'); await entered;
  assert.equal(parentCalls, 1); release();
  assert.equal((await pending).status, 'completed');
  assert.throws(() => captured.delegate(child, 'Too late'), /CLOSED/);
  parent.close(); child.close();
});

test('a failed parent cancels and joins its child without another model call', async () => {
  const objective = new Objective();
  let enter; const entered = new Promise(resolve => { enter = resolve; });
  let childStopped = false;
  const child = new Agent({ model: 'child', provider: { complete: async (_, { signal }) => {
    enter(); await new Promise((_, reject) => signal.addEventListener('abort', () => {
      childStopped = true; reject(new Error('aborted'));
    }, { once: true }));
  } } });
  const parent = new Agent({ model: 'parent', objective, provider: { complete: () => call() },
    tools: [tool(async (_, context) => { context.delegate(child, 'Work'); await entered; throw new Error('parent failed'); })] });
  await assert.rejects(parent.run('Work'), ToolOperationUncertain);
  assert.equal(childStopped, true); assert.equal(objective.inspect().model_calls, 2);
  parent.close(); child.close();
});

test('process-local objective refuses persistent agents before opening a session', () => {
  assert.throws(() => new Agent({ model: 'test', objective: new Objective(), store: {} }), /cannot use SessionStore/);
});

test('streaming uses the root cap and settles once without auxiliary calls', async () => {
  const objective = new Objective({ max_output_tokens: 7 }); const caps = [];
  const agent = new Agent({ model: 'test', objective, provider: { async *stream(request) {
    caps.push(request.max_output_tokens);
    yield { type: 'text_delta', text: 'done' }; yield { type: 'reply', reply: reply() };
  } } });
  const events = []; for await (const event of agent.stream('Work')) events.push(event);
  assert.deepEqual(caps, [7]); assert.equal(events.at(-1).result.content, 'done');
  assert.equal(objective.inspect().model_calls, 1); assert.deepEqual(objective.inspect().reservations, {});
  agent.close();
});

test('root deadline cancels an active request and cannot be refreshed', async () => {
  const objective = new Objective({ max_run_ms: 300 }); let calls = 0;
  const agent = new Agent({ model: 'test', objective, provider: { async complete(request, { signal }) {
    calls++; assert.equal(request.deadline_unix_ms, objective.inspect().deadline_unix_ms);
    await new Promise((_, reject) => signal.addEventListener('abort', () => reject(new Error('deadline')), { once: true }));
  } } });
  await assert.rejects(agent.run('Work'), /DEADLINE|OBJECTIVE_LIMIT/);
  await assert.rejects(agent.run('Again'), ObjectiveLimit);
  assert.equal(calls, 1); assert.equal(objective.inspect().model_calls, 1);
  agent.close();
});
