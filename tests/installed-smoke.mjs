// Copy into the clean consumer directory before running, so imports resolve there.
import assert from 'node:assert/strict';
import { Agent, Objective, SessionStore } from '@johpaz/hive-sdk-rust';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

const root = mkdtempSync(path.join(tmpdir(), 'hive-installed-'));
const provider = { complete(request) {
  const last = request.messages.at(-1);
  return last.role === 'tool' ? { content: `sum=${last.content}`, usage: { input_tokens: 1, output_tokens: 1 } }
    : { content: null, tool_calls: [{ id: 'sum-1', name: 'sum', arguments: { a: 2, b: 3 } }],
      usage: { input_tokens: 1, output_tokens: 1 } };
} };
const tools = [{ name: 'sum', description: 'Sum integers', parameters: { type: 'object', required: ['a', 'b'],
  properties: { a: { type: 'integer' }, b: { type: 'integer' } } }, execute: ({ a, b }) => a + b }];
try {
  const store = await SessionStore.open({ path: path.join(root, 'db') });
  const durableObjective = await Objective.open({ max_model_calls: 2, max_tool_calls: 1 }, { store, id: 'installed-root' });
  const options = { model: 'installed', provider, tools, store, objective: durableObjective, agentId: 'smoke', sessionId: 'test' };
  const agent = await Agent.open(options);
  assert.equal((await agent.run('Sum')).content, 'sum=5'); agent.close();
  const reopened = await Agent.open(options); assert.equal(reopened.messages.at(-1).content, 'sum=5'); reopened.close();
  const durableUsage = await durableObjective.inspectAsync();
  assert.equal(durableUsage.model_calls, 2); assert.equal(durableUsage.tool_calls, 1);
  assert.equal(durableUsage.input_tokens, 2); assert.equal(durableUsage.output_tokens, 2);
  await store.closeAsync();
  const objective = new Objective({ max_model_calls: 1 });
  const budgeted = new Agent({ model: 'installed', objective, provider: { complete: () => ({ content: 'ok', usage: { input_tokens: 1, output_tokens: 1 } }) } });
  assert.equal((await budgeted.run('Hello')).content, 'ok'); assert.equal(objective.inspect().model_calls, 1); budgeted.close();
  console.log(`Installed npm binding OK on Node ${process.versions.node} (${process.platform}/${process.arch})`);
} finally { rmSync(root, { recursive: true, force: true }); }
