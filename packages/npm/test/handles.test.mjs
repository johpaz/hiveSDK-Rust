import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Agent } from '../index.mjs';

const toolReply = { content: 'Reading', tool_calls: [{ id: 'c', name: 'read', arguments: {} }], usage: { input_tokens: 3, output_tokens: 2 } };
const tools = execute => [{ name: 'read', description: 'Read', parameters: { type: 'object' }, execute }];

test('native memory handle rolls back failed work after a known tool and preserves committed getters', async () => {
  let models = 0; let effects = 0; let agent;
  agent = new Agent({ model: 'test', tools: tools(() => { effects++; assert.equal(agent.messages.length, 2); return 5; }),
    provider: { complete(request) {
      models++;
      if (models === 2) return toolReply;
      if (models === 3) throw new Error('provider failed after known tool');
      if (models === 4) assert.deepEqual(request.messages.map(message => message.role), ['user', 'assistant', 'user']);
      return { content: 'Known 🦀' };
    } } });
  await agent.run('History'); const before = agent.messages;
  await assert.rejects(agent.run('Failed task'), /provider failed/);
  assert.deepEqual(agent.messages, before); assert.equal(effects, 1);
  const result = await agent.run('Explicit new task'); assert.equal(result.content, 'Known 🦀'); assert.equal(effects, 1);
  const copy = agent.messages; copy[0].content = 'changed'; assert.equal(agent.messages[0].content, 'History');
  agent.close(); assert.equal(agent.messages.length, 4);
});

test('terminal observation failure discards the working handle before a new run', async () => {
  let fail = true;
  const agent = new Agent({ model: 'test', provider: { complete: () => ({ content: 'Known' }) },
    onEvent(action) { if (fail && action.type === 'complete') { fail = false; throw new Error('observer failed'); } } });
  await assert.rejects(agent.run('Failed'), /observer failed/); assert.deepEqual(agent.messages, []);
  await agent.run('Explicit'); assert.equal(agent.messages[0].content, 'Explicit'); assert.equal(agent.messages.length, 2);
});

test('closing an iterator after a tool discards the active turn without rerunning the tool', async () => {
  let models = 0; let effects = 0; let modelActions = 0;
  const agent = new Agent({ model: 'test', tools: tools(() => { effects++; return 5; }),
    provider: { complete: () => ++models === 1 ? toolReply : { content: 'Explicit result' } } });
  for await (const event of agent.stream('Interrupted')) {
    if (event.type === 'model' && ++modelActions === 2) break;
  }
  assert.equal(models, 1); assert.equal(effects, 1); assert.deepEqual(agent.messages, []);
  await agent.run('Explicit'); assert.equal(models, 2); assert.equal(effects, 1); assert.equal(agent.messages.length, 2);
});

test('cancellation after a tool discards working history and keeps the agent reusable', async () => {
  let received; const ready = new Promise(resolve => { received = resolve; }); let models = 0;
  const agent = new Agent({ model: 'test', tools: tools(() => 5), provider: { complete(_request, { signal }) {
    if (++models === 1) return toolReply;
    if (models === 2) { received(); return new Promise((_, reject) => signal.addEventListener('abort', () => reject(new Error('cancelled')), { once: true })); }
    return { content: 'New' };
  } } });
  const pending = agent.run('Cancel'); await ready; agent.cancel(); await assert.rejects(pending, /CANCELLED/);
  assert.deepEqual(agent.messages, []); await agent.run('Explicit'); assert.equal(agent.messages.length, 2);
});
