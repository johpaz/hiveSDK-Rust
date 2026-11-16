import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { OpenAICompatible } from '../index.mjs';

function server(handler) {
  const hits = [];
  const http = createServer((request, response) => { hits.push(request.url); request.resume(); request.on('end', () => handler(hits.length, response)); });
  return new Promise(resolve => http.listen(0, '127.0.0.1', () => resolve({ http, hits, url: `http://127.0.0.1:${http.address().port}/v1` })));
}
const ok = response => {
  const body = JSON.stringify({ choices: [{ message: { role: 'assistant', content: 'hola' }, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } });
  response.writeHead(200, { 'content-type': 'application/json' }); response.end(body);
};
const request = { model: 'm', messages: [{ role: 'user', content: 'hi' }], tools: [] };

test('a refused request is retried and an ambiguous failure never is', async () => {
  const flaky = await server((n, response) => n < 3 ? (response.writeHead(n === 1 ? 429 : 503), response.end('no')) : ok(response));
  try {
    const provider = new OpenAICompatible({ baseUrl: flaky.url, resilience: { retry: { max_attempts: 3, base_delay_ms: 5, max_delay_ms: 20 } } });
    assert.equal((await provider.complete(request)).content, 'hola');
    assert.equal(flaky.hits.length, 3);
  } finally { flaky.http.close(); }
  const broken = await server((_n, response) => { response.writeHead(500); response.end('boom'); });
  try {
    const provider = new OpenAICompatible({ baseUrl: broken.url, resilience: { retry: { max_attempts: 4, base_delay_ms: 5, max_delay_ms: 20 } } });
    await assert.rejects(provider.complete(request), /HTTP 500/);
    assert.equal(broken.hits.length, 1, 'a 500 may have been processed and is not repeated');
  } finally { broken.http.close(); }
});

test('the breaker opens without sending and invalid config is rejected', async () => {
  const down = await server((_n, response) => { response.writeHead(500); response.end('x'); });
  try {
    const provider = new OpenAICompatible({ baseUrl: down.url, resilience: { circuit_breaker: { failure_threshold: 2, cooldown_ms: 60_000 } } });
    await assert.rejects(provider.complete(request), /HTTP 500/);
    await assert.rejects(provider.complete(request), /HTTP 500/);
    await assert.rejects(provider.complete(request), /PROVIDER_CIRCUIT_OPEN/);
    assert.equal(down.hits.length, 2);
  } finally { down.http.close(); }
  assert.throws(() => new OpenAICompatible({ resilience: { retry: { max_attempts: 9 } } }), /max_attempts/);
  assert.doesNotThrow(() => new OpenAICompatible({}));
});
