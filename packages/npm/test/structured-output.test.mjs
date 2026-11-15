import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createServer } from 'node:http';
import { Agent, OpenAICompatible, Anthropic, Gemini } from '../index.mjs';

const schema = { type: 'object', properties: { answer: { type: 'string' } }, required: ['answer'], additionalProperties: false };
const structured_output = { name: 'answer_v1', schema };
const policy = { structured_output, max_output_tokens: 8, max_total_output_tokens: 20 };
const text = ' \n{"answer":"sí 🦀"}\t';
const usage = { input_tokens: 3, output_tokens: 2 };
function payload(kind, content) {
  if (kind === 'openai') return { choices: [{ message: { content }, finish_reason: 'stop' }], usage: { prompt_tokens: 3, completion_tokens: 2 } };
  if (kind === 'anthropic') return { type: 'message', role: 'assistant', content: [{ type: 'text', text: content }], stop_reason: 'end_turn', usage };
  return { candidates: [{ index: 0, content: { role: 'model', parts: [{ text: content }] }, finishReason: 'STOP' }], usageMetadata: { promptTokenCount: 3, candidatesTokenCount: 2 } };
}
function send(res, kind, content, stream) {
  if (!stream) { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(payload(kind, content))); return; }
  res.writeHead(200, { 'content-type': 'text/event-stream' });
  const frame = value => res.write(`${value.type ? `event: ${value.type}\n` : ''}data: ${JSON.stringify(value)}\n\n`);
  const chunks = [content.slice(0, 3), content.slice(3)];
  if (kind === 'openai') {
    for (const part of chunks) frame({ choices: [{ index: 0, delta: { content: part } }] });
    frame({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 3, completion_tokens: 2 } });
    res.end('data: [DONE]\n\n');
  } else if (kind === 'anthropic') {
    frame({ type: 'message_start', message: { type: 'message', role: 'assistant', content: [], stop_reason: null, usage: { input_tokens: 3, output_tokens: 0 } } });
    frame({ type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } });
    for (const part of chunks) frame({ type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: part } });
    frame({ type: 'content_block_stop', index: 0 });
    frame({ type: 'message_delta', delta: { stop_reason: 'end_turn' }, usage: { output_tokens: 2 } });
    frame({ type: 'message_stop' }); res.end();
  } else {
    for (const part of chunks) frame({ candidates: [{ index: 0, content: { role: 'model', parts: [{ text: part }] } }] });
    frame({ candidates: [{ index: 0, finishReason: 'STOP' }], usageMetadata: { promptTokenCount: 3, candidatesTokenCount: 2 } }); res.end();
  }
}
function check(body, kind) {
  if (kind === 'openai') {
    assert.deepEqual(body.response_format, { type: 'json_schema', json_schema: { name: 'answer_v1', schema, strict: true } });
    assert.equal(body.max_completion_tokens, 8);
  } else if (kind === 'anthropic') {
    assert.deepEqual(body.output_config, { format: { type: 'json_schema', schema } }); assert.equal(body.max_tokens, 8);
  } else {
    assert.deepEqual(body.generationConfig, { candidateCount: 1, maxOutputTokens: 8, responseMimeType: 'application/json', responseJsonSchema: schema });
  }
  assert.equal(body.structured_output, undefined);
}
async function serve(kind, content, streaming, reject = false) {
  let requests = 0; let failure;
  const server = createServer(async (req, res) => {
    try {
      let raw = ''; for await (const chunk of req) raw += chunk;
      requests++; const body = JSON.parse(raw); check(body, kind);
      assert.equal(req.url, kind === 'gemini' ? `/v1beta/models/test:${streaming ? 'streamGenerateContent?alt=sse' : 'generateContent'}` : kind === 'openai' ? '/v1/chat/completions' : '/v1/messages');
      if (reject) { res.writeHead(400); res.end('unsupported schema'); }
      else send(res, kind, content, streaming);
    } catch (error) { failure = error; res.destroy(); }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  const Provider = { openai: OpenAICompatible, anthropic: Anthropic, gemini: Gemini }[kind];
  return { provider: new Provider({ baseUrl: `http://127.0.0.1:${server.address().port}/${kind === 'gemini' ? 'v1beta' : 'v1'}`, apiKey: 'test-only' }),
    get requests() { return requests; }, async close() { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); if (failure) throw failure; } };
}
for (const kind of ['openai', 'anthropic', 'gemini']) for (const streaming of [false, true]) {
  test(`${kind} structured ${streaming ? 'SSE' : 'JSON'} validates only the final reply, preserves raw text and settles known usage`, async () => {
    for (const content of [text, '{"answer":7}']) {
      const server = await serve(kind, content, streaming);
      try {
        const agent = new Agent({ model: 'test', provider: server.provider, policy }); let result; let partial = '';
        if (streaming) for await (const event of agent.stream('Answer')) {
          if (event.type === 'text_delta') partial += event.text;
          if (event.type === 'result') result = event.result;
        } else result = await agent.run('Answer');
        if (content === text) assert.equal(result.content, content);
        else assert.equal(result.stop_reason, 'invalid_structured_output');
        assert.equal(agent.messages.at(-1).content, content);
        assert.equal(result.stats.model_calls, 1); assert.equal(server.requests, 1);
        assert.equal(result.stats.input_tokens, 3); assert.equal(result.stats.output_tokens, 2); assert.equal(result.stats.reserved_output_tokens, 0);
        if (streaming) assert.equal(partial, content);
      } finally { await server.close(); }
    }
  });
}
test('unsupported provider schemas are surfaced after one request with no fallback', async () => {
  for (const kind of ['openai', 'anthropic', 'gemini']) {
    const server = await serve(kind, text, false, true);
    try { await assert.rejects(new Agent({ model: 'test', provider: server.provider, policy }).run('Answer'), /PROVIDER_ERROR/); assert.equal(server.requests, 1); }
    finally { await server.close(); }
  }
});
test('custom providers cannot weaken final validation by mutating their request schema', async () => {
  let requests = 0;
  const agent = new Agent({ model: 'test', policy, provider: { complete(request) {
    requests++; assert.deepEqual(request.structured_output, structured_output);
    request.structured_output.schema = {}; return { content: '{}', usage };
  } } });
  const result = await agent.run('Answer'); assert.equal(result.stop_reason, 'invalid_structured_output'); assert.equal(requests, 1);
});
test('invalid output schemas fail before any provider callback', () => {
  let requests = 0;
  assert.throws(() => new Agent({ model: 'test', policy: { structured_output: { name: 'bad.name', schema } }, provider: { complete() { requests++; } } }), /INVALID_INPUT/);
  assert.equal(requests, 0);
});
