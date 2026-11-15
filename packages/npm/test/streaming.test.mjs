import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createServer } from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import path from 'node:path';
import { tmpdir } from 'node:os';
import { Agent, OpenAICompatible, SessionStore } from '../index.mjs';

function deferred() { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; }
async function serve(handler) {
  const server = createServer(async (req, res) => {
    try { let raw = ''; for await (const part of req) raw += part; await handler(JSON.parse(raw), res); }
    catch (error) { res.destroy(error); }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  return { provider: options => new OpenAICompatible({ baseUrl: `http://127.0.0.1:${server.address().port}/v1`, ...options }),
    async close() { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); } };
}
function headers(res) { res.writeHead(200, { 'content-type': 'text/event-stream' }); res.flushHeaders(); }
function data(res, value) { res.write(`data: ${JSON.stringify(value)}\n\n`); }
function finish(res, reason, usage) {
  data(res, { choices: [{ index: 0, delta: {}, finish_reason: reason }] });
  if (usage) data(res, { choices: [], usage });
  res.end('data: [DONE]\n\n');
}
const add = execute => ({ name: 'add', description: 'Add', parameters: { type: 'object',
  properties: { a: { type: 'number' }, b: { type: 'number' } }, required: ['a', 'b'] }, execute });

test('native SSE streams text, assembles tools and meters each complete reply once', { timeout: 10_000 }, async () => {
  let requests = 0; let executions = 0;
  const gate = deferred();
  const server = await serve(async (body, res) => {
    assert.equal(body.stream, true); assert.equal(body.stream_options.include_usage, true);
    headers(res);
    if (++requests === 1) {
      data(res, { choices: [{ delta: { tool_calls: [{ index: 0, id: 'a', type: 'function', function: { name: 'ad', arguments: '{"a":2,' } }] } }] });
      data(res, { choices: [{ delta: { tool_calls: [{ index: 0, function: { name: 'd', arguments: '"b":3}' } }] } }] });
      finish(res, 'tool_calls', { prompt_tokens: 5, completion_tokens: 3 });
    } else {
      assert.equal(body.messages.at(-1).content, '5');
      data(res, { choices: [{ delta: { content: 'Hola ' } }] });
      await gate.promise;
      data(res, { choices: [{ delta: { content: '🐝' } }] });
      finish(res, 'stop', { prompt_tokens: 7, completion_tokens: 2 });
    }
  });
  try {
    const agent = new Agent({ model: 'test', provider: server.provider(), tools: [add(({ a, b }) => { executions++; return a + b; })] });
    let text = ''; let result;
    for await (const event of agent.stream('Add')) {
      if (event.type === 'tool_call_delta') assert.equal(executions, 0);
      if (event.type === 'text_delta') { text += event.text; if (event.text === 'Hola ') gate.resolve(); }
      if (event.type === 'result') result = event.result;
    }
    assert.equal(text, 'Hola 🐝'); assert.equal(result.content, text);
    assert.equal(result.stats.model_calls, 2); assert.equal(result.stats.tool_calls, 1);
    assert.equal(result.stats.input_tokens, 12); assert.equal(result.stats.output_tokens, 5);
    assert.equal(executions, 1); assert.equal(requests, 2);
  } finally { gate.resolve(); await server.close(); }
});

test('cancel aborts HTTP before headers and keeps the model operation uncertain', { timeout: 10_000 }, async () => {
  const received = deferred(); const closed = deferred();
  const server = await serve(async (_body, res) => { res.once('close', closed.resolve); received.resolve(); });
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-cancel-')); const store = new SessionStore({ path: directory });
  const config = { model: 'test', provider: server.provider(), store, agentId: 'a', sessionId: 's' };
  try {
    const agent = new Agent(config); const pending = agent.run('Wait'); await received.promise;
    assert.equal(agent.cancel(), true);
    await assert.rejects(pending, /CANCELLED/); await closed.promise;
    assert.equal(store.inspect({ agentId: 'a', sessionId: 's' }).run.in_flight.kind, 'model');
    assert.throws(() => new Agent({ ...config, resume: true }), /SESSION_OPERATION_UNCERTAIN/);
    assert.equal(agent.cancel(), false);
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); await server.close(); }
});

test('returning the stream iterator closes a partial HTTP response', { timeout: 10_000 }, async () => {
  const closed = deferred();
  const server = await serve(async (_body, res) => { res.once('close', closed.resolve); headers(res); data(res, { choices: [{ delta: { content: 'partial' } }] }); });
  try {
    const agent = new Agent({ model: 'test', provider: server.provider() });
    const events = agent.stream('Wait');
    while ((await events.next()).value.type !== 'text_delta') { /* wait for first real delta */ }
    await events.return(); await closed.promise;
    assert.equal(agent.cancel(), false); assert.deepEqual(agent.messages, []);
  } finally { await server.close(); }
});

test('deadline closes a stalled stream without another request', { timeout: 10_000 }, async () => {
  const closed = deferred(); let requests = 0;
  const server = await serve(async (_body, res) => { requests++; res.once('close', closed.resolve); headers(res); data(res, { choices: [{ delta: { content: 'partial' } }] }); });
  try {
    const agent = new Agent({ model: 'test', provider: server.provider({ timeoutMs: 250 }) });
    const text = [];
    await assert.rejects(async () => { for await (const event of agent.stream('Wait')) if (event.type === 'text_delta') text.push(event.text); }, /TIMEOUT/);
    await closed.promise; assert.deepEqual(text, ['partial']); assert.equal(requests, 1);
  } finally { await server.close(); }
});

test('incomplete SSE never executes an apparently complete tool call', { timeout: 10_000 }, async () => {
  let executions = 0;
  const server = await serve(async (_body, res) => { headers(res);
    data(res, { choices: [{ delta: { tool_calls: [{ index: 0, id: 'a', function: { name: 'add', arguments: '{"a":2,"b":3}' } }] }, finish_reason: 'tool_calls' }] });
    res.end();
  });
  try {
    const agent = new Agent({ model: 'test', provider: server.provider(), tools: [add(() => { executions++; return 5; })] });
    await assert.rejects(async () => { for await (const _event of agent.stream('Add')) { /* consume */ } }, /\[DONE\]/);
    assert.equal(executions, 0);
  } finally { await server.close(); }
});

test('already aborted signal performs no provider call', async () => {
  let calls = 0; const controller = new AbortController(); controller.abort();
  const agent = new Agent({ model: 'test', provider: { complete() { calls++; return { content: 'unused' }; } } });
  await assert.rejects(agent.run('Wait', { signal: controller.signal }), /CANCELLED/); assert.equal(calls, 0);
});

test('known cooperative tool result is checkpointed before cancellation stops continuation', { timeout: 10_000 }, async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-tool-cancel-')); const store = new SessionStore({ path: directory });
  const started = deferred(); let calls = 0;
  const agent = new Agent({ model: 'test', store, agentId: 'a', sessionId: 's',
    provider: { complete() { calls++; return { content: null, tool_calls: [{ id: 'a', name: 'add', arguments: { a: 2, b: 3 } }] }; } },
    tools: [add((_args, context) => new Promise(resolve => {
      started.resolve(); context.signal.addEventListener('abort', () => resolve({ cancelled: true }), { once: true });
    }))] });
  try {
    const pending = agent.run('Add'); await started.promise; agent.cancel();
    await assert.rejects(pending, /CANCELLED/);
    const run = store.inspect({ agentId: 'a', sessionId: 's' }).run;
    assert.equal(run.in_flight, null); assert.equal(run.tools[0].status, 'completed');
    assert.deepEqual(run.tools[0].result, { cancelled: true }); assert.equal(calls, 1);
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('stream-only custom provider works for stream and run without fabricated chunks', async () => {
  const provider = { async *stream() { yield { type: 'text_delta', text: 'Hello' }; yield { type: 'reply', reply: { content: 'Hello' } }; } };
  const agent = new Agent({ model: 'test', provider });
  const events = []; for await (const event of agent.stream('Hello')) events.push(event);
  assert.equal(events.filter(e => e.type === 'text_delta').length, 1);
  assert.equal((await agent.run('Again')).content, 'Hello');
});

test('external AbortSignal cancels HTTP while the iterator is paused', { timeout: 10_000 }, async () => {
  const closed = deferred(); const controller = new AbortController();
  const server = await serve(async (_body, res) => { res.once('close', closed.resolve); headers(res); data(res, { choices: [{ delta: { content: 'partial' } }] }); });
  try {
    const agent = new Agent({ model: 'test', provider: server.provider() });
    const events = agent.stream('Wait', { signal: controller.signal });
    while ((await events.next()).value.type !== 'text_delta') { /* consume lifecycle */ }
    controller.abort(); await closed.promise;
    await events.return(); assert.equal(agent.cancel(), false);
  } finally { await server.close(); }
});

test('resumeStream preserves usage and does not replay a confirmed tool', { timeout: 10_000 }, async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-resume-stream-')); const store = new SessionStore({ path: directory });
  let requests = 0; let executions = 0; let events = 0;
  const server = await serve(async (_body, res) => {
    headers(res);
    if (++requests === 1) {
      data(res, { choices: [{ delta: { tool_calls: [{ index: 0, id: 'a', function: { name: 'add', arguments: '{"a":2,"b":3}' } }] } }] });
      finish(res, 'tool_calls', { prompt_tokens: 5, completion_tokens: 3 });
    } else {
      data(res, { choices: [{ delta: { content: '5' } }] });
      finish(res, 'stop', { prompt_tokens: 7, completion_tokens: 2 });
    }
  });
  const config = { model: 'test', provider: server.provider(), store, agentId: 'a', sessionId: 's',
    tools: [add(() => { executions++; return 5; })] };
  try {
    const first = new Agent({ ...config, onEvent(action) {
      if (action.type === 'model' && ++events === 2) throw new Error('checkpoint interruption');
    } });
    await assert.rejects(async () => { for await (const _event of first.stream('Add')) { /* consume */ } }, /checkpoint interruption/);
    assert.equal(requests, 1);
    const resumed = new Agent({ ...config, resume: true }); let result;
    for await (const event of resumed.resumeStream()) if (event.type === 'result') result = event.result;
    assert.equal(result.content, '5'); assert.equal(result.stats.model_calls, 2);
    assert.equal(result.stats.input_tokens, 12); assert.equal(result.stats.output_tokens, 5);
    assert.equal(result.stats.tool_calls, 1); assert.equal(executions, 1); assert.equal(requests, 2);
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); await server.close(); }
});
