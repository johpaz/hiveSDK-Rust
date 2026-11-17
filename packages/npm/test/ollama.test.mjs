import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { Agent, Ollama, Anthropic, SessionStore } from '../index.mjs';

// A fake server that records each request body and answers with a canned reply.
async function server(reply) {
  const seen = [];
  const http = createServer((request, response) => {
    let body = ''; request.on('data', c => body += c);
    request.on('end', () => { seen.push({ url: request.url, body: JSON.parse(body) }); response.writeHead(200, { 'content-type': 'application/json' }); response.end(JSON.stringify(reply)); });
  });
  await new Promise(resolve => http.listen(0, '127.0.0.1', resolve));
  return { seen, port: http.address().port, close: () => { http.closeAllConnections(); http.close(); } };
}
const request = { model: 'llama3.2', messages: [{ role: 'user', content: 'hola' }], tools: [] };

test('Ollama talks to /api/chat with the options only its native API takes', async () => {
  const fake = await server({ message: { role: 'assistant', content: 'hola desde ollama' }, done: true, done_reason: 'stop', prompt_eval_count: 4, eval_count: 3 });
  try {
    const provider = new Ollama({ baseUrl: `http://127.0.0.1:${fake.port}/api`, numCtx: 16384, keepAlive: '10m', think: false });
    const reply = await provider.complete(request);
    assert.equal(reply.content, 'hola desde ollama');
    assert.deepEqual([reply.usage.input_tokens, reply.usage.output_tokens], [4, 3]);
    const { url, body } = fake.seen[0];
    assert.equal(url, '/api/chat');
    assert.equal(body.options.num_ctx, 16384);
    assert.equal(body.options.num_predict, 2457, 'the output limit is derived from the window it asks for');
    assert.deepEqual([body.keep_alive, body.think], ['10m', false]);
    assert.throws(() => new Ollama({ numCtx: 0 }), /positive/);
  } finally { fake.close(); }
});

test('the output limit of a provider is derived from its context window unless one is given', async () => {
  const fake = await server({ id: 'm', type: 'message', role: 'assistant', content: [{ type: 'text', text: 'ok' }], stop_reason: 'end_turn', usage: { input_tokens: 1, output_tokens: 1 } });
  try {
    const base = { baseUrl: `http://127.0.0.1:${fake.port}/v1`, apiKey: 'test-only' };
    await new Anthropic({ ...base, contextWindow: 200_000 }).complete({ ...request, model: 'claude' });
    await new Anthropic({ ...base }).complete({ ...request, model: 'claude' });
    await new Anthropic({ ...base, contextWindow: 200_000, defaultMaxOutputTokens: 500 }).complete({ ...request, model: 'claude' });
    assert.deepEqual(fake.seen.map(({ body }) => body.max_tokens), [30000, 4096, 500]);
  } finally { fake.close(); }
});
