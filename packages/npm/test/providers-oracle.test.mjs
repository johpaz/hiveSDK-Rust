import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createServer } from 'node:http';
import { Agent, Objective, Oracle, PresetProvider } from '../index.mjs';

const listen = handler => new Promise(resolve => {
  const requests = [];
  const server = createServer((request, response) => {
    let body = ''; request.on('data', chunk => { body += chunk; });
    request.on('end', () => { requests.push({ headers: request.headers, url: request.url, body: body ? JSON.parse(body) : null }); handler(request, response, requests.length); });
  });
  server.listen(0, '127.0.0.1', () => resolve({ server, requests, base: `http://127.0.0.1:${server.address().port}` }));
});
const json = (response, body, status = 200) => { response.writeHead(status, { 'content-type': 'application/json' }); response.end(JSON.stringify(body)); };

test('presets expand to provider configuration and apply their wire profile', async () => {
  const { server, requests, base } = await listen((_, response, count) => json(response, count === 1 ? {
    choices: [{ finish_reason: 'tool_calls', message: { content: null, tool_calls: [{ id: 'c', type: 'function', function: { name: 'my__tool', arguments: '{}' } }] } }],
    usage: { prompt_tokens: 3, completion_tokens: 1 } } : { choices: [{ finish_reason: 'stop', message: { content: 'done' } }], usage: { prompt_tokens: 5, completion_tokens: 1 } }));
  try {
    const seen = [];
    const agent = new Agent({ model: 'm', provider: new PresetProvider('kimi', { apiKey: 'k', baseUrl: `${base}/v1` }),
      tools: [{ name: 'my.tool', description: 'd', parameters: { type: 'object', additionalProperties: false, properties: { list: { type: 'array' } } }, execute() { seen.push('ran'); return 1; } }],
      maxTurns: 4 });
    await agent.run('go'); agent.close();
    assert.deepEqual(seen, ['ran'], 'the reply was mapped back to the original tool name');
    const [call] = requests;
    assert.equal(call.headers.authorization, 'Bearer k');
    assert.equal(call.body.tools[0].function.name, 'my__tool');
    assert.equal(call.body.parallel_tool_calls, false);
    assert.equal(call.body.tool_choice, 'auto');
    assert.deepEqual(call.body.tools[0].function.parameters.properties.list.items, {});
    assert.throws(() => new PresetProvider('nope'), /unknown provider preset/);
    assert.throws(() => new PresetProvider('anthropic', { profile: { normalize_tool_names: true } }), /OpenAI-compatible/);
  } finally { server.close(); }
});

test('the oracle answers closed questions and falls back instead of throwing', async () => {
  const answers = { next: { type: 'choice', choice: 'finish', confidence: 0.9, probabilities: { finish: 0.9, continue: 0.1 } }, sure: { type: 'noul', noul: 0.7 } };
  const { server, requests, base } = await listen((_, response, count) => count === 1
    ? json(response, { answers, usage: { input_tokens: 40, output_tokens: 2 } }) : json(response, {}, 500));
  try {
    const oracle = new Oracle({ endpoint: `${base}/decisions`, model: 'jev', api_key: 'secret', cooldown_ms: 10000 });
    const questions = { next: { type: 'choice', instructions: 'what next', criteria: { finish: 'done', continue: 'more' } }, sure: { type: 'noul', instructions: 'done?' } };
    const reply = await oracle.ask({ turn: 1 }, questions);
    assert.equal(reply.outcome, 'decision');
    assert.equal(reply.answers.next.choice, 'finish');
    assert.deepEqual(reply.charge, { input_tokens: 40, output_tokens: 2 });
    assert.equal(requests[0].headers.authorization, 'Bearer secret');
    assert.deepEqual(requests[0].body.state, { turn: 1 });
    const failed = await oracle.ask({}, questions);
    assert.deepEqual([failed.outcome, failed.reason, failed.sent], ['fallback', 'oracle HTTP 500', true]);
    assert.equal(oracle.status().state, 'fallback');
    assert.equal(oracle.recordOverruled('x'), false);
    oracle.recordAgreement(); oracle.reset();
    assert.equal(oracle.status().state, 'ready');
    assert.throws(() => new Oracle({ endpoint: 'ftp://x', model: 'm' }), /endpoint/);
    await assert.rejects(oracle.ask({}, { q: { type: 'choice', instructions: 'i', criteria: { only: 'one' } } }), /two criteria/);
  } finally { server.close(); }
});

test('oracle calls are reserved on, and settled against, the root objective', async () => {
  const { server, base } = await listen((_, response) => json(response, { answers: { sure: { type: 'noul', noul: 0.5 } }, usage: { input_tokens: 25, output_tokens: 2 } }));
  try {
    const oracle = new Oracle({ endpoint: `${base}/d`, model: 'jev', max_output_tokens: 8 });
    const objective = new Objective({ max_model_calls: 2, max_output_tokens: 50, max_total_output_tokens: 50 });
    const questions = { sure: { type: 'noul', instructions: 'done?' } };
    assert.equal((await oracle.ask({}, questions, { objective })).outcome, 'decision');
    const snapshot = objective._dispatch({ op: 'inspect' });
    assert.deepEqual([snapshot.model_calls, snapshot.input_tokens, snapshot.output_tokens], [1, 25, 2]);
    await oracle.ask({}, questions, { objective });
    await assert.rejects(oracle.ask({}, questions, { objective }), /model_call_limit/);
  } finally { server.close(); }
});
