import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createServer } from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, Anthropic, SessionStore } from '../index.mjs';

const tool = execute => ({ name: 'add', description: 'Add', parameters: { type: 'object', properties: { a: { type: 'number' }, b: { type: 'number' } }, required: ['a', 'b'] }, execute });
const policy = { max_output_tokens: 4, max_total_output_tokens: 5, max_context_bytes: 10000 };
const use = { type: 'tool_use', id: 'c', name: 'add', input: { a: 2, b: 3 } };
const usage = first => ({ input_tokens: first ? 10 : 5, cache_creation_input_tokens: first ? 4 : 0, cache_read_input_tokens: first ? 6 : 0, output_tokens: first ? 2 : 1 });
const reply = (first, reason = first ? 'tool_use' : 'end_turn') => ({ type: 'message', role: 'assistant', content: first ? [use] : [{ type: 'text', text: 'Hola 🐝' }], stop_reason: reason, usage: usage(first) });
const frame = (res, value) => res.write(`event: ${value.type}\ndata: ${JSON.stringify(value)}\n\n`);
function start(res, first) {
  res.writeHead(200, { 'content-type': 'text/event-stream' });
  frame(res, { type: 'message_start', message: { type: 'message', role: 'assistant', content: [], stop_reason: null, usage: { ...usage(first), output_tokens: 0 } } });
}
function block(res, first) {
  frame(res, { type: 'content_block_start', index: 0, content_block: first ? { ...use, input: {} } : { type: 'text', text: '' } });
  for (const part of first ? ['{"a":2,', '"b":3}'] : ['Hola ', '🐝']) {
    frame(res, { type: 'content_block_delta', index: 0, delta: first ? { type: 'input_json_delta', partial_json: part } : { type: 'text_delta', text: part } });
  }
  frame(res, { type: 'content_block_stop', index: 0 });
}
function finish(res, first, terminal = true) {
  frame(res, { type: 'message_delta', delta: { stop_reason: first ? 'tool_use' : 'end_turn' }, usage: { output_tokens: first ? 2 : 1 } });
  if (terminal) frame(res, { type: 'message_stop' });
  res.end();
}
async function serve(handler) {
  let requests = 0;
  const server = createServer(async (req, res) => {
    assert.equal(req.url, '/v1/messages'); assert.equal(req.headers.authorization, 'Bearer test-only');
    assert.equal(req.headers['anthropic-version'], '2026-10-21');
    let raw = ''; for await (const chunk of req) raw += chunk;
    const body = JSON.parse(raw); requests++;
    for (const key of ['context', 'deadline_unix_ms', 'max_output_tokens', 'stream_options']) assert.equal(body[key], undefined);
    await handler(body, res, requests);
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  return { provider: () => new Anthropic({ baseUrl: `http://127.0.0.1:${server.address().port}/v1`, apiKey: 'test-only', defaultMaxOutputTokens: 9 }),
    get requests() { return requests; }, async close() { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); } };
}
function checkRequest(body, first) {
  assert.equal(body.max_tokens, first ? 4 : 3); assert.deepEqual(body.system, [{ type: 'text', text: 'System' }]);
  assert.equal(body.tools[0].name, 'add'); assert.equal(body.tools[0].input_schema.type, 'object');
  if (!first) {
    assert.equal(body.messages.at(-2).content[0].type, 'tool_use');
    assert.deepEqual(body.messages.at(-1), { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'c', content: '5' }] });
  }
}

for (const streaming of [false, true]) test(`Anthropic ${streaming ? 'SSE' : 'JSON'} executes a tool once and reconciles cache usage and output reservations`, async () => {
  let executions = 0;
  let release; const gate = new Promise(resolve => { release = resolve; });
  const server = await serve(async (body, res, requests) => {
    const first = requests === 1; checkRequest(body, first); assert.equal(body.stream, streaming);
    if (streaming) {
      start(res, first);
      if (!first) {
        frame(res, { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } });
        frame(res, { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'Hola ' } });
        await gate;
        frame(res, { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: '🐝' } });
        frame(res, { type: 'content_block_stop', index: 0 });
      } else block(res, first);
      finish(res, first);
    } else { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(reply(first))); }
  });
  try {
    const agent = new Agent({ model: 'test', system: 'System', policy, provider: server.provider(), tools: [tool(args => { executions++; return args.a + args.b; })] });
    let result; let text = '';
    if (streaming) for await (const event of agent.stream('Add')) {
      if (event.type === 'tool_call_delta') assert.equal(executions, 0);
      if (event.type === 'text_delta') { text += event.text; release(); }
      if (event.type === 'result') result = event.result;
    } else result = await agent.run('Add');
    assert.equal(result.content, 'Hola 🐝'); assert.equal(result.stats.model_calls, 2); assert.equal(executions, 1);
    assert.equal(result.stats.input_tokens, 25); assert.equal(result.stats.output_tokens, 3); assert.equal(result.stats.reserved_output_tokens, 0);
    assert.equal(server.requests, 2); if (streaming) assert.equal(text, result.content);
  } finally { release(); await server.close(); }
});

test('Anthropic EOF before message_stop preserves uncertainty and never executes the apparent tool', async () => {
  const server = await serve(async (_body, res) => { start(res, true); block(res, true); finish(res, true, false); });
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-anthropic-eof-')); const store = new SessionStore({ path: directory }); let executions = 0;
  const config = { model: 'test', store, agentId: 'a', sessionId: 's', policy, provider: server.provider(), tools: [tool(() => { executions++; return 0; })] };
  try {
    await assert.rejects(async () => { for await (const _event of new Agent(config).stream('Add')) { /* consume partial stream */ } }, /PROVIDER_ERROR/);
    assert.equal(executions, 0); assert.equal(server.requests, 1);
    const run = store.inspect({ agentId: 'a', sessionId: 's' }).run;
    assert.equal(run.in_flight.kind, 'model'); assert.equal(run.state.stats.reserved_output_tokens, 4);
    assert.throws(() => new Agent({ ...config, resume: true }), /SESSION_OPERATION_UNCERTAIN/);
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); await server.close(); }
});

test('Anthropic cancellation closes HTTP before headers', async () => {
  let received, closed; const receivedPromise = new Promise(resolve => { received = resolve; }); const closedPromise = new Promise(resolve => { closed = resolve; });
  const server = await serve(async (_body, res) => { res.once('close', closed); received(); });
  try {
    const agent = new Agent({ model: 'test', policy, provider: server.provider() });
    const pending = agent.run('Wait'); await receivedPromise; assert.equal(agent.cancel(), true);
    await assert.rejects(pending, /CANCELLED/); await closedPromise; assert.equal(server.requests, 1);
  } finally { await server.close(); }
});

test('Anthropic clean checkpoint resumes with tool results, cache usage and the remaining cap', async () => {
  const server = await serve(async (body, res, requests) => { const first = requests === 1; checkRequest(body, first); res.end(JSON.stringify(reply(first))); });
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-anthropic-resume-')); let store = new SessionStore({ path: directory }); let executions = 0; let models = 0;
  const config = { model: 'test', system: 'System', agentId: 'a', sessionId: 's', policy, provider: server.provider(), tools: [tool(args => { executions++; return args.a + args.b; })] };
  try {
    const first = new Agent({ ...config, store, onEvent(action) { if (action.type === 'model' && ++models === 2) throw new Error('interrupted'); } });
    await assert.rejects(first.run('Add'), /interrupted/); assert.equal(server.requests, 1); assert.equal(executions, 1);
    store.close(); store = new SessionStore({ path: directory });
    const result = await new Agent({ ...config, provider: server.provider(), store, resume: true }).resume();
    assert.equal(result.content, 'Hola 🐝'); assert.equal(result.stats.input_tokens, 25); assert.equal(result.stats.output_tokens, 3);
    assert.equal(result.stats.model_calls, 2); assert.equal(executions, 1); assert.equal(server.requests, 2);
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); await server.close(); }
});

test('Anthropic refuses truncation, paused turns and HTTP failures without automatic retries', async () => {
  for (const reason of ['max_tokens', 'pause_turn', 'refusal', 'http_error']) {
    let executions = 0;
    const server = await serve(async (_body, res) => {
      if (reason === 'http_error') { res.writeHead(429); res.end('private vendor message'); }
      else res.end(JSON.stringify(reply(true, reason)));
    });
    try {
      const agent = new Agent({ model: 'test', policy, provider: server.provider(), tools: [tool(() => { executions++; return 0; })] });
      await assert.rejects(agent.run('Add'), /PROVIDER_ERROR/); assert.equal(server.requests, 1); assert.equal(executions, 0);
    } finally { await server.close(); }
  }
});
