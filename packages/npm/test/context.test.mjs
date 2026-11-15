import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, SessionStore } from '../index.mjs';

const policy = { max_context_bytes: 500, context_overflow: 'drop_oldest_turns' };
const bytes = ({ model, messages, tools }) => Buffer.byteLength(JSON.stringify({ model, messages, tools }));

test('oversized context stops before dispatch and leaves no output reservation', async () => {
  let calls = 0;
  const agent = new Agent({ model: 'test', policy: { max_context_bytes: 100, max_total_output_tokens: 10 },
    provider: { complete() { calls++; return { content: 'unexpected' }; } } });
  const result = await agent.run('x'.repeat(1000));
  assert.equal(result.stop_reason, 'context_limit'); assert.equal(calls, 0);
  assert.equal(result.stats.reserved_output_tokens, 0);
  assert.equal(agent.messages.at(-1).content, 'x'.repeat(1000));
});

test('bounded context survives a clean checkpoint while full history stays durable', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-context-'));
  let store = new SessionStore({ path: directory }); let calls = 0; let interrupted = false;
  const provider = { complete(request) {
    calls++; assert.equal(request.context.serialized_bytes, bytes(request));
    if (calls === 2) {
      assert.equal(request.context.dropped_messages, 2);
      assert.deepEqual(request.messages.map(m => m.content), ['System', 'current'.repeat(20)]);
    }
    return { content: 'finished' };
  } };
  const limit = bytes({ model: 'test', tools: [], messages: [
    { role: 'system', content: 'System' }, { role: 'user', content: 'old'.repeat(60) },
  ] });
  const config = { model: 'test', system: 'System', policy: { ...policy, max_context_bytes: limit }, agentId: 'a', sessionId: 's', provider };
  try {
    const agent = new Agent({ ...config, store, onEvent(action) {
      if (interrupted && action.type === 'model') throw new Error('interrupted');
    } });
    await agent.run('old'.repeat(60)); interrupted = true;
    await assert.rejects(agent.run('current'.repeat(20)), /interrupted/);
    const before = store.inspect({ agentId: 'a', sessionId: 's' }).run;
    assert.equal(before.state.messages.length, 4);
    assert.equal(before.state.action.request.context.dropped_messages, 2);
    assert.equal(before.in_flight, null); assert.equal(calls, 1);
    store.close(); store = new SessionStore({ path: directory });
    const resumed = new Agent({ ...config, store, resume: true });
    assert.equal((await resumed.resume()).status, 'completed'); assert.equal(calls, 2);
    assert.equal(resumed.messages.length, 5);
    assert.equal(resumed.messages[1].content, 'old'.repeat(60));
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('large active tool output is committed before stopping with no extra model call', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-context-tool-'));
  const store = new SessionStore({ path: directory }); let calls = 0;
  try {
    const agent = new Agent({ model: 'test', store, agentId: 'a', sessionId: 's', policy,
      tools: [{ name: 'read', description: 'Read', parameters: { type: 'object' }, execute: () => 'x'.repeat(1000) }],
      provider: { complete() { calls++; return { content: null, tool_calls: [{ id: 'c', name: 'read', arguments: {} }] }; } } });
    const result = await agent.run('Read');
    assert.equal(result.stop_reason, 'context_limit'); assert.equal(calls, 1); assert.equal(result.stats.tool_calls, 1);
    const run = store.inspect({ agentId: 'a', sessionId: 's' }).run;
    assert.equal(run.tools[0].status, 'completed'); assert.equal(run.tools[0].result, 'x'.repeat(1000));
    assert.equal(run.in_flight, null); assert.equal(agent.messages.at(-1).role, 'tool');
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('streaming custom provider receives exact UTF-8 JSON bytes and unchanged text', async () => {
  const text = 'é🦀\n"\\'; let calls = 0;
  const provider = { async *stream(request) {
    calls++; assert.equal(request.context.serialized_bytes, bytes(request));
    assert.equal(request.messages[0].content, text); assert.equal(request.context.dropped_messages, 0);
    yield { type: 'text_delta', text: 'ok' }; yield { type: 'reply', reply: { content: 'ok' } };
  } };
  const agent = new Agent({ model: 'test', provider, policy });
  const events = []; for await (const event of agent.stream(text)) events.push(event);
  assert.equal(calls, 1); assert.equal(events.at(-1).result.content, 'ok');
  assert.equal(events.find(e => e.type === 'text_delta').text, 'ok');
});
