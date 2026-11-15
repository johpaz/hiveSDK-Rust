import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createServer } from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { Agent, OpenAICompatible, SessionStore } from '../index.mjs';

const tool = execute => ({ name: 'read', description: 'Read', parameters: { type: 'object' }, execute });
const reply = (output, useTool = false) => ({ content: useTool ? null : 'done', tool_calls: useTool ? [{ id: 'c', name: 'read', arguments: {} }] : [],
  usage: { input_tokens: 1, output_tokens: output } });
function deferred() { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; }
async function serve(handler) {
  const server = createServer(async (req, res) => { let raw = ''; for await (const chunk of req) raw += chunk; await handler(JSON.parse(raw), res); });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  return { provider: options => new OpenAICompatible({ baseUrl: `http://127.0.0.1:${server.address().port}/v1`, ...options }),
    async close() { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); } };
}

test('output budget is reserved durably before dispatch and shrinks the next cap', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-output-budget-')); const store = new SessionStore({ path: dir });
  let calls = 0;
  try {
    const agent = new Agent({ model: 'test', store, agentId: 'a', sessionId: 's', tools: [tool(() => 'ok')],
      policy: { max_output_tokens: 4, max_total_output_tokens: 5 }, provider: { complete(request) {
        const expected = ++calls === 1 ? 4 : 2;
        assert.equal(request.max_output_tokens, expected);
        assert.equal(store.inspect({ agentId: 'a', sessionId: 's' }).run.state.stats.reserved_output_tokens, expected);
        return reply(calls === 1 ? 3 : 2, calls === 1);
      } } });
    const result = await agent.run('Read');
    assert.equal(result.status, 'completed'); assert.equal(result.stats.output_tokens, 5);
    assert.equal(result.stats.reserved_output_tokens, 0); assert.equal(calls, 2);
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('modern and legacy output limits are sent explicitly for JSON and SSE', { timeout: 10_000 }, async () => {
  let parameter; let requests = 0;
  const server = await serve(async (body, res) => {
    requests++; assert.equal(body[parameter], 2);
    assert.equal(body[parameter === 'max_tokens' ? 'max_completion_tokens' : 'max_tokens'], undefined);
    assert.equal(body.deadline_unix_ms, undefined); assert.equal(body.max_output_tokens, undefined);
    if (body.stream) {
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      for (const value of [{ choices: [{ delta: { content: 'done' }, finish_reason: 'stop' }] },
        { choices: [], usage: { prompt_tokens: 1, completion_tokens: 2 } }]) res.write(`data: ${JSON.stringify(value)}\n\n`);
      res.end('data: [DONE]\n\n');
    } else { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({ choices: [{ message: { content: 'done' } }], usage: { prompt_tokens: 1, completion_tokens: 2 } })); }
  });
  try {
    for (parameter of ['max_completion_tokens', 'max_tokens']) for (const streaming of [false, true]) {
      const agent = new Agent({ model: 'test', provider: server.provider({ outputTokenParameter: parameter }),
        policy: { max_output_tokens: 2, max_run_ms: 3000 } });
      if (streaming) { for await (const _event of agent.stream('Read')) { /* consume */ } }
      else assert.equal((await agent.run('Read')).content, 'done');
    }
    assert.equal(requests, 4);
  } finally { await server.close(); }
});

test('deadline after a slow hook stops before invoking the provider', async () => {
  let calls = 0;
  const agent = new Agent({ model: 'test', policy: { max_run_ms: 20 },
    onEvent: async action => { if (action.type === 'model') await sleep(50); },
    provider: { complete() { calls++; return reply(1); } } });
  const result = await agent.run('Read');
  assert.equal(result.stop_reason, 'deadline_exceeded'); assert.equal(calls, 0);
});

test('global deadline closes HTTP and keeps the unknown output reservation', { timeout: 10_000 }, async () => {
  const received = deferred(); const closed = deferred();
  const server = await serve(async (_body, res) => { res.once('close', closed.resolve); received.resolve(); });
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-run-deadline-')); const store = new SessionStore({ path: dir });
  const config = { model: 'test', store, agentId: 'a', sessionId: 's', provider: server.provider({ timeoutMs: 5000 }),
    policy: { max_run_ms: 1500, max_total_output_tokens: 7 } };
  try {
    const agent = new Agent(config); const pending = agent.run('Wait'); await received.promise;
    await assert.rejects(pending, /DEADLINE_EXCEEDED/); await closed.promise;
    const run = store.inspect({ agentId: 'a', sessionId: 's' }).run;
    assert.equal(run.in_flight.kind, 'model'); assert.equal(run.state.stats.reserved_output_tokens, 7);
    assert.throws(() => new Agent({ ...config, resume: true }), /SESSION_OPERATION_UNCERTAIN/);
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); await server.close(); }
});

test('an expired clean checkpoint cannot obtain a fresh deadline on resume', { timeout: 10_000 }, async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-expired-resume-')); const store = new SessionStore({ path: dir });
  let calls = 0;
  const config = { model: 'test', store, agentId: 'a', sessionId: 's', policy: { max_run_ms: 500, max_total_output_tokens: 7 },
    provider: { complete() { calls++; return reply(1); } } };
  try {
    const first = new Agent({ ...config, onEvent(action) { if (action.type === 'model') throw new Error('interrupted'); } });
    await assert.rejects(first.run('Read'), /interrupted/);
    const deadline = store.inspect({ agentId: 'a', sessionId: 's' }).run.state.deadline_unix_ms;
    await sleep(Math.max(0, deadline - Date.now() + 30));
    const result = await new Agent({ ...config, resume: true }).resume();
    assert.equal(result.stop_reason, 'deadline_exceeded'); assert.equal(calls, 0);
    const state = store.inspect({ agentId: 'a', sessionId: 's' }).run.state;
    assert.equal(state.deadline_unix_ms, deadline); assert.equal(state.stats.reserved_output_tokens, 0);
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('a late known tool result is committed without a final paid continuation', { timeout: 10_000 }, async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-late-tool-')); const store = new SessionStore({ path: dir }); let calls = 0;
  try {
    const agent = new Agent({ model: 'test', store, agentId: 'a', sessionId: 's', policy: { max_run_ms: 1500 },
      provider: { complete() { calls++; return reply(1, true); } }, tools: [tool((_args, context) =>
        new Promise(resolve => context.signal.addEventListener('abort', () => resolve({ known: true }), { once: true })))] });
    const result = await agent.run('Read');
    assert.equal(result.stop_reason, 'deadline_exceeded'); assert.equal(calls, 1);
    const run = store.inspect({ agentId: 'a', sessionId: 's' }).run;
    assert.equal(run.in_flight, null); assert.deepEqual(run.tools[0].result, { known: true });
    assert.equal(run.tools[0].status, 'completed');
  } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
});
