import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, SessionStore } from '../index.mjs';

const budget = (encoding = 'cl100k_base', max_tokens = 160) => ({ encoding, max_tokens, overhead_tokens: 7 });

test('provider framing and combined windows are explicit and checked before dispatch', async () => {
  for (const framing of ['neutral_json', 'openai_chat_json', 'anthropic_messages_json', 'gemini_generate_content_json']) {
    let input; let calls = 0;
    const provider = { complete(request) {
      calls++; input = request.context.token_estimate.total_tokens;
      assert.equal(request.context.token_estimate.framing, framing);
      return { content: 'done', usage: { input_tokens: 1, output_tokens: 1 } };
    } };
    const policy = { input_token_budget: { ...budget(), framing }, max_output_tokens: 10 };
    const probe = new Agent({ model: 'test', provider, policy }); await probe.run('Hello 🦀'); probe.close();
    for (const [extra, expected] of [[10, 'completed'], [9, 'stopped']]) {
      const before = calls;
      const agent = new Agent({ model: 'test', provider, policy: { ...policy,
        input_token_budget: { ...policy.input_token_budget, context_window_tokens: input + extra } } });
      const result = await agent.run('Hello 🦀'); agent.close();
      assert.equal(result.status, expected); assert.equal(calls - before, extra === 10 ? 1 : 0);
      if (extra === 9) assert.equal(result.stop_reason, 'context_limit');
    }
  }
});

test('accumulated input and pending tool survive reopen without another paid continuation', async () => {
  let estimate; let calls = 0; let effects = 0;
  const tools = [{ name: 'read', description: 'Read', parameters: { type: 'object' }, execute() { effects++; return 5; } }];
  const policy = { input_token_budget: { ...budget(), framing: 'openai_chat_json' } };
  const probe = new Agent({ model: 'test', tools, policy, provider: { complete(request) {
    estimate = request.context.token_estimate.total_tokens; return { content: 'done' };
  } } }); await probe.run('Read'); probe.close();
  policy.input_token_budget.max_total_tokens = estimate;
  const provider = { complete() { calls++; return { content: null, tool_calls: [{ id: 'c', name: 'read', arguments: {} }], usage: { input_tokens: 1, output_tokens: 1 } }; } };
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-input-total-')); let store;
  const config = { model: 'test', tools, policy, provider, agentId: 'a', sessionId: 's' };
  try {
    store = new SessionStore({ path: directory });
    const agent = new Agent({ ...config, store, onEvent(action) { if (action.type === 'tool') throw new Error('pause'); } });
    await assert.rejects(agent.run('Read'), /pause/); agent.close();
    assert.equal(effects, 0); store.close(); store = new SessionStore({ path: directory });
    const resumed = new Agent({ ...config, store, resume: true }); const result = await resumed.resume(); resumed.close();
    assert.equal(result.stop_reason, 'input_limit'); assert.equal(calls, 1); assert.equal(effects, 1);
    assert.equal(result.stats.charged_input_tokens, estimate); assert.equal(result.stats.estimated_input_tokens, estimate);
    assert.equal(result.stats.input_tokens, 1); assert.equal(result.stats.reserved_input_tokens, 0);
    const view = store.inspect({ agentId: 'a', sessionId: 's' }); assert.equal(view.run.tools[0].status, 'completed');
  } finally { store?.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('cumulative input rejects missing usage and actual overrun before tool effects', async () => {
  for (const usage of [undefined, { input_tokens: 1001, output_tokens: 1 }]) {
    const agent = new Agent({ model: 'test', policy: { input_token_budget: { ...budget(), max_total_tokens: 1000 } },
      tools: [{ name: 'read', description: 'Read', parameters: { type: 'object' }, execute() { assert.fail('no effect'); } }],
      provider: { complete() { return { content: null, tool_calls: [{ id: 'c', name: 'read', arguments: {} }], usage }; } } });
    const result = await agent.run('Read'); agent.close();
    assert.equal(result.stop_reason, usage ? 'input_limit' : 'usage_unavailable');
    assert.equal(result.stats.tool_calls, 0); assert.equal(result.stats.reserved_input_tokens, 0);
  }
});

test('both local encodings report estimates for complete and streaming without inventing actual usage', async () => {
  let calls = 0;
  for (const encoding of ['cl100k_base', 'o200k_base']) for (const streaming of [false, true]) {
    const provider = { async *stream(request) {
      calls++;
      const estimate = request.context.token_estimate;
      assert.equal(estimate.encoding, encoding); assert.equal(estimate.overhead_tokens, 7);
      assert.equal(estimate.total_tokens, estimate.serialized_tokens + 7);
      assert.ok(estimate.total_tokens <= 160); assert.ok(estimate.serialized_tokens > 0);
      assert.equal(request.messages[0].content, 'Hola 🦀\n"\\ <|endoftext|>');
      yield { type: 'text_delta', text: 'done' };
      yield { type: 'reply', reply: { content: 'done', usage: { input_tokens: 123, output_tokens: 1 } } };
    } };
    const agent = new Agent({ model: 'private-alias', policy: { input_token_budget: budget(encoding) }, provider });
    let result;
    if (streaming) { for await (const event of agent.stream('Hola 🦀\n"\\ <|endoftext|>')) if (event.type === 'result') result = event.result; }
    else result = await agent.run('Hola 🦀\n"\\ <|endoftext|>');
    assert.equal(result.status, 'completed'); assert.equal(result.stats.input_tokens, 123);
    assert.equal(result.stats.model_calls, 1);
  }
  assert.equal(calls, 4);
});

test('input token budget stops before dispatch or output reservation and rejects invalid profiles', async () => {
  let calls = 0;
  const provider = { complete() { calls++; return { content: 'unexpected' }; } };
  const result = await new Agent({ model: 'test', provider,
    policy: { input_token_budget: budget('cl100k_base', 8), max_total_output_tokens: 30 } }).run('Hello');
  assert.equal(result.stop_reason, 'context_limit'); assert.equal(calls, 0);
  assert.equal(result.stats.reserved_output_tokens, 0);
  for (const input_token_budget of [budget('unknown'), budget('cl100k_base', 0),
    { ...budget(), max_tokens: 7 }, { ...budget(), typo: 1 }]) {
    assert.throws(() => new Agent({ model: 'test', provider, policy: { input_token_budget } }));
  }
});

test('token-only selection and full history survive interruption and reopening', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-token-resume-'));
  let store = new SessionStore({ path: directory }); let calls = 0; let interrupted = false;
  const config = { model: 'test', system: 'System', agentId: 'a', sessionId: 's',
    policy: { input_token_budget: budget(), context_overflow: 'drop_oldest_turns' },
    provider: { complete(request) {
      calls++; assert.ok(request.context.token_estimate.total_tokens <= 160);
      if (calls === 2) {
        assert.equal(request.context.dropped_messages, 2);
        assert.deepEqual(request.messages.map(m => m.content), ['System', 'current '.repeat(100)]);
      }
      return { content: 'finished' };
    } } };
  try {
    const agent = new Agent({ ...config, store, onEvent(action) {
      if (interrupted && action.type === 'model') throw new Error('interrupted');
    } });
    await agent.run('old '.repeat(100)); interrupted = true;
    await assert.rejects(agent.run('current '.repeat(100)), /interrupted/);
    const before = store.inspect({ agentId: 'a', sessionId: 's' }).run;
    assert.equal(before.in_flight, null); assert.equal(before.state.messages.length, 4);
    assert.equal(before.state.action.request.context.dropped_messages, 2);
    store.close(); store = new SessionStore({ path: directory });
    const resumed = new Agent({ ...config, store, resume: true });
    assert.equal((await resumed.resume()).status, 'completed'); assert.equal(calls, 2);
    assert.equal(resumed.messages.length, 5); assert.equal(resumed.messages[1].content, 'old '.repeat(100));
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('token overflow after a known tool result persists its ledger without another model call', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-token-tool-'));
  const store = new SessionStore({ path: directory }); let calls = 0;
  try {
    const agent = new Agent({ model: 'test', store, agentId: 'a', sessionId: 's',
      policy: { input_token_budget: budget(), max_context_bytes: 100_000, max_total_output_tokens: 30 },
      tools: [{ name: 'read', description: 'Read', parameters: { type: 'object' }, execute: () => 'result '.repeat(1000) }],
      provider: { complete() { calls++; return { content: null, tool_calls: [{ id: 'c', name: 'read', arguments: {} }],
        usage: { input_tokens: 1, output_tokens: 1 } }; } } });
    const result = await agent.run('Read');
    assert.equal(result.stop_reason, 'context_limit'); assert.equal(calls, 1);
    assert.equal(result.stats.reserved_output_tokens, 0); assert.equal(result.stats.input_tokens, 1);
    const run = store.inspect({ agentId: 'a', sessionId: 's' }).run;
    assert.equal(run.tools[0].status, 'completed'); assert.equal(run.tools[0].result, 'result '.repeat(1000));
    assert.equal(run.in_flight, null);
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
});
