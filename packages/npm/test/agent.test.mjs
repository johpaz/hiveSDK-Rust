import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createServer } from 'node:http';
import { Agent, OpenAICompatible } from '../index.mjs';

const tool = (execute = ({ a, b }) => a + b) => ({ name: 'add', description: 'Add numbers',
  parameters: { type: 'object', properties: { a: { type: 'number' }, b: { type: 'number' } }, required: ['a', 'b'] }, execute });
const call = id => ({ content: null, tool_calls: [{ id, name: 'add', arguments: { a: 2, b: 3 } }], usage: { input_tokens: 10, output_tokens: 2 } });

test('native engine invokes async host tool and preserves conversation', async () => {
  let count = 0;
  const events = [];
  const agent = new Agent({ model: 'test', tools: [tool(async ({ a, b }) => a + b)],
    onEvent: event => events.push(event.type), provider: { complete(request) {
      count++;
      if (count === 1) return call('a');
      if (count === 2) assert.equal(request.messages.at(-1).content, '5');
      return { content: '5' };
    } } });
  const result = await agent.run('Add');
  assert.equal(result.content, '5');
  assert.equal(result.status, 'completed');
  assert.equal(result.stats.model_calls, 2);
  assert.deepEqual(events, ['model', 'tool', 'model', 'complete']);
  await agent.run('Again');
  assert.equal(agent.messages.length, 6);
  const messages = agent.messages; messages.length = 0;
  assert.equal(agent.messages.length, 6);
});

test('repeated identical results stop without a third model call', async () => {
  let calls = 0;
  const agent = new Agent({ model: 'test', tools: [tool()], provider: {
    complete: () => call(String(++calls)),
  } });
  const result = await agent.run('Add forever');
  assert.equal(result.stop_reason, 'repeated_tool_result');
  assert.equal(result.stats.model_calls, 2);
  assert.equal(calls, 2);
});

test('tool exceptions become model feedback', async () => {
  let calls = 0;
  const agent = new Agent({ model: 'test', tools: [tool(() => { throw new Error('offline'); })],
    provider: { complete(request) {
      if (++calls === 1) return call('a');
      assert.equal(JSON.parse(request.messages.at(-1).content).error.message, 'offline');
      return { content: 'Could not add' };
    } } });
  assert.equal((await agent.run('Add')).content, 'Could not add');
});

test('model failure preserves committed history and permits a new explicit run', async () => {
  let fail = true;
  const agent = new Agent({ model: 'test', provider: { complete() {
    if (fail) throw new Error('unavailable');
    return { content: 'Recovered' };
  } } });
  await assert.rejects(agent.run('First'), /unavailable/);
  assert.equal(agent.messages.length, 0);
  fail = false;
  assert.equal((await agent.run('Retry')).content, 'Recovered');
  agent.close();
  await assert.rejects(agent.run('Closed'), /CLOSED/);
});

test('concurrent runs are rejected', async () => {
  let release;
  const pending = new Promise(resolve => { release = resolve; });
  const agent = new Agent({ model: 'test', provider: { complete: () => pending } });
  const run = agent.run('First');
  await assert.rejects(agent.run('Second'), /BUSY/);
  assert.throws(() => agent.close(), /BUSY/);
  release({ content: 'Done' });
  await run;
});

test('native HTTP provider sends wire requests and reads usage', async () => {
  let received;
  const server = createServer(async (req, res) => {
    let raw = ''; for await (const part of req) raw += part;
    received = { path: req.url, authorization: req.headers.authorization, body: JSON.parse(raw) };
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({ choices: [{ finish_reason: 'stop', message: { content: 'HTTP works' } }],
      usage: { prompt_tokens: 7, completion_tokens: 2 } }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    const agent = new Agent({ model: 'local-test', provider: new OpenAICompatible({
      baseUrl: `http://127.0.0.1:${server.address().port}/v1`, apiKey: 'test-key',
    }) });
    const result = await agent.run('Hello');
    assert.equal(result.content, 'HTTP works');
    assert.equal(result.stats.input_tokens, 7);
    assert.equal(received.path, '/v1/chat/completions');
    assert.equal(received.authorization, 'Bearer test-key');
    assert.equal(received.body.model, 'local-test');
    assert.equal(received.body.stream, false);
  } finally { await new Promise(resolve => server.close(resolve)); }
});

test('token budget without usage stops explicitly', async () => {
  const agent = new Agent({ model: 'test', tools: [tool()], policy: { max_total_tokens: 20 },
    provider: { complete: () => ({ ...call('a'), usage: undefined }) } });
  assert.equal((await agent.run('Add')).stop_reason, 'usage_unavailable');
});
