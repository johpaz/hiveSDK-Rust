import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createServer } from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, Gemini, SessionStore } from '../index.mjs';

const tool = execute => ({ name: 'add', description: 'Add', parameters: { type: 'object', properties: { a: { type: 'number' }, b: { type: 'number' } }, required: ['a', 'b'] }, execute });
const policy = { max_output_tokens: 4, max_total_output_tokens: 5, max_context_bytes: 10000 };
const parts = first => first ? [{ text: '', thoughtSignature: 'text-sig' }, { functionCall: { id: 'c', name: 'add', args: { a: 2, b: 3 } }, thoughtSignature: 'tool-sig' }]
  : [{ text: 'Hola ' }, { text: '🐝' }, { text: '', thoughtSignature: 'tail' }];
const usage = first => ({ promptTokenCount: first ? 10 : 5, cachedContentTokenCount: first ? 4 : 0, candidatesTokenCount: 1, thoughtsTokenCount: first ? 1 : 0 });
const reply = (first, reason = 'STOP') => ({ candidates: [{ index: 0, content: { role: 'model', parts: parts(first) }, finishReason: reason }], usageMetadata: usage(first) });
const frame = (res, value) => res.write(`data: ${JSON.stringify(value)}\n\n`);
function start(res) { res.writeHead(200, { 'content-type': 'text/event-stream' }); }
function block(res, part) { frame(res, { candidates: [{ index: 0, content: { role: 'model', parts: [part] } }] }); }
function finish(res, first, terminal = true) {
  if (terminal) frame(res, { candidates: [{ index: 0, finishReason: 'STOP' }], usageMetadata: usage(first) });
  res.end();
}
async function serve(handler) {
  let requests = 0;
  const server = createServer(async (req, res) => {
    assert.ok(['/v1beta/models/test:generateContent', '/v1beta/models/test:streamGenerateContent?alt=sse'].includes(req.url));
    assert.equal(req.headers['x-goog-api-key'], 'test-only'); assert.equal(req.headers.authorization, undefined);
    let raw = ''; for await (const chunk of req) raw += chunk; requests++;
    const body = JSON.parse(raw);
    for (const key of ['model', 'stream', 'context', 'deadline_unix_ms', 'provider_state']) assert.equal(body[key], undefined);
    await handler(body, res, requests, req.url.includes('streamGenerateContent'));
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  return { provider: () => new Gemini({ baseUrl: `http://127.0.0.1:${server.address().port}/v1beta`, apiKey: 'test-only', defaultMaxOutputTokens: 9 }),
    get requests() { return requests; }, async close() { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); } };
}
function checkRequest(body, first) {
  assert.equal(body.generationConfig.maxOutputTokens, first ? 4 : 3); assert.equal(body.generationConfig.candidateCount, 1);
  assert.deepEqual(body.systemInstruction, { parts: [{ text: 'System' }] });
  assert.equal(body.tools[0].functionDeclarations[0].parametersJsonSchema.type, 'object');
  if (!first) {
    assert.deepEqual(body.contents.at(-2), { role: 'model', parts: parts(true) });
    assert.deepEqual(body.contents.at(-1), { role: 'user', parts: [{ functionResponse: { id: 'c', name: 'add', response: { result: 5 } } }] });
  }
}

for (const streaming of [false, true]) test(`Gemini ${streaming ? 'SSE' : 'JSON'} tools preserve signatures, caps and cache/thought usage`, async () => {
  let executions = 0; let release; const gate = new Promise(resolve => { release = resolve; });
  const server = await serve(async (body, res, count, stream) => {
    const first = count === 1; checkRequest(body, first); assert.equal(stream, streaming);
    if (stream) {
      start(res); for (const [index, part] of parts(first).entries()) { block(res, part); if (!first && index === 0) await gate; } finish(res, first);
    } else res.end(JSON.stringify(reply(first)));
  });
  try {
    const agent = new Agent({ model: 'models/test', system: 'System', policy, provider: server.provider(), tools: [tool(args => { executions++; return args.a + args.b; })] });
    let result; let text = '';
    if (streaming) for await (const event of agent.stream('Add')) {
      if (event.type === 'tool_call_delta') assert.equal(executions, 0);
      if (event.type === 'text_delta') { text += event.text; if (event.text) release(); }
      if (event.type === 'result') result = event.result;
    } else result = await agent.run('Add');
    assert.equal(result.content, 'Hola 🐝'); assert.equal(executions, 1); assert.equal(server.requests, 2);
    assert.equal(result.stats.input_tokens, 15); assert.equal(result.stats.output_tokens, 3); assert.equal(result.stats.model_calls, 2);
    assert.equal(result.stats.reserved_output_tokens, 0); assert.deepEqual(agent.messages.at(-1).provider_state.parts, parts(false));
    if (streaming) assert.equal(text, result.content);
  } finally { release(); await server.close(); }
});

test('Gemini SSE without STOP never executes its function and retains the uncertain reservation', async () => {
  const server = await serve(async (_body, res) => { start(res); for (const part of parts(true)) block(res, part); finish(res, true, false); });
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-gemini-eof-')); const store = new SessionStore({ path: directory }); let executions = 0;
  const config = { model: 'test', store, agentId: 'a', sessionId: 's', policy, provider: server.provider(), tools: [tool(() => { executions++; return 0; })] };
  try {
    await assert.rejects(async () => { for await (const _event of new Agent(config).stream('Add')) { /* partial stream */ } }, /PROVIDER_ERROR/);
    assert.equal(executions, 0); assert.equal(server.requests, 1);
    const run = store.inspect({ agentId: 'a', sessionId: 's' }).run;
    assert.equal(run.in_flight.kind, 'model'); assert.equal(run.state.stats.reserved_output_tokens, 4);
    assert.throws(() => new Agent({ ...config, resume: true }), /SESSION_OPERATION_UNCERTAIN/);
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); await server.close(); }
});

test('Gemini cancellation closes the pending native HTTP request', async () => {
  let received, closed; const ready = new Promise(resolve => { received = resolve; }); const done = new Promise(resolve => { closed = resolve; });
  const server = await serve(async (_body, res) => { res.once('close', closed); received(); });
  try {
    const agent = new Agent({ model: 'test', policy, provider: server.provider() }); const pending = agent.run('Wait');
    await ready; agent.cancel(); await assert.rejects(pending, /CANCELLED/); await done; assert.equal(server.requests, 1);
  } finally { await server.close(); }
});

test('Gemini resumes JSON-to-SSE with exact signed parts and no replay of a known tool', async () => {
  const server = await serve(async (body, res, count, stream) => {
    const first = count === 1; checkRequest(body, first);
    if (stream) { start(res); for (const part of parts(first)) block(res, part); finish(res, first); }
    else res.end(JSON.stringify(reply(first)));
  });
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-gemini-resume-')); let store = new SessionStore({ path: directory }); let calls = 0; let models = 0;
  const config = { model: 'test', system: 'System', policy, agentId: 'a', sessionId: 's', tools: [tool(args => { calls++; return args.a + args.b; })] };
  try {
    await assert.rejects(new Agent({ ...config, store, provider: server.provider(), onEvent(action) { if (action.type === 'model' && ++models === 2) throw new Error('interrupted'); } }).run('Add'), /interrupted/);
    assert.equal(calls, 1); assert.equal(server.requests, 1); store.close(); store = new SessionStore({ path: directory });
    const agent = new Agent({ ...config, store, provider: server.provider(), resume: true }); let result;
    for await (const event of agent.resumeStream()) if (event.type === 'result') result = event.result;
    assert.equal(result.content, 'Hola 🐝'); assert.equal(result.stats.input_tokens, 15); assert.equal(result.stats.output_tokens, 3);
    assert.equal(calls, 1); assert.equal(server.requests, 2); assert.deepEqual(agent.messages[2].provider_state.parts, parts(true));
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); await server.close(); }
});

test('Gemini truncation, safety, malformed calls and HTTP errors never retry or run tools', async () => {
  for (const reason of ['MAX_TOKENS', 'SAFETY', 'MALFORMED_FUNCTION_CALL', 'http_error']) {
    let calls = 0;
    const server = await serve(async (_body, res) => { if (reason === 'http_error') { res.writeHead(429); res.end('private'); } else res.end(JSON.stringify(reply(true, reason))); });
    try {
      await assert.rejects(new Agent({ model: 'test', policy, provider: server.provider(), tools: [tool(() => { calls++; return 0; })] }).run('Add'), /PROVIDER_ERROR/);
      assert.equal(calls, 0); assert.equal(server.requests, 1);
    } finally { await server.close(); }
  }
});
