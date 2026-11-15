import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, SessionStore } from '../index.mjs';

const schema = { type: 'object', properties: { age: { type: 'integer', minimum: 18 } }, required: ['age'], additionalProperties: false };
const tool = execute => ({ name: 'save', description: 'Save', parameters: schema, execute });
const call = (id, age) => ({ content: null, tool_calls: [{ id, name: 'save', arguments: { age } }] });

test('invalid schemas and external references fail before any provider call', () => {
  for (const parameters of [{ type: 'object', required: true },
    { type: 'object', $ref: 'https://example.invalid/schema' },
    { type: 'object', $ref: 'file:///tmp/not-a-schema.json' }]) {
    assert.throws(() => new Agent({ model: 'test', tools: [{ ...tool(() => 0), parameters }],
      provider: { complete: () => assert.fail('provider must not be called') } }), /INVALID_TOOL_SCHEMA/);
  }
});

for (const persistent of [false, true]) {
  test(`model corrects rejected arguments before callback runs (${persistent ? 'HiveDB' : 'memory'})`, async () => {
    const directory = persistent ? mkdtempSync(path.join(tmpdir(), 'hive-schema-')) : null;
    const store = persistent ? new SessionStore({ path: directory }) : undefined;
    let calls = 0; let executions = 0;
    const events = [];
    try {
      const agent = new Agent({ model: 'test', ...(store ? { store, agentId: 'a', sessionId: 's' } : {}),
        tools: [tool(args => { executions++; assert.equal(args.age, 20); return 'saved'; })],
        onEvent: action => events.push(action.type), provider: { complete(request) {
          if (++calls === 1) return call('bad', '20');
          if (calls === 2) {
            const feedback = JSON.parse(request.messages.at(-1).content);
            assert.equal(feedback.error.code, 'INVALID_TOOL_ARGUMENTS');
            assert.equal(feedback.error.issues[0].instance_path, '/age');
            assert.equal(executions, 0);
            return call('corrected', 20);
          }
          return { content: 'Saved' };
        } } });
      const result = await agent.run('Save');
      assert.equal(result.content, 'Saved');
      assert.equal(executions, 1);
      assert.equal(result.stats.model_calls, 3);
      assert.equal(result.stats.tool_calls, 1);
      assert.equal(result.stats.tool_rejections, 1);
      assert.deepEqual(events, ['model', 'tool_rejected', 'model', 'tool', 'model', 'complete']);
      if (store) assert.deepEqual(store.inspect({ agentId: 'a', sessionId: 's' }).run.tools.map(t => t.status), ['rejected', 'completed']);
    } finally { store?.close(); if (directory) rmSync(directory, { recursive: true, force: true }); }
  });
}

test('repeated invalid calls stop without any tool invocation', async () => {
  let calls = 0; let executions = 0;
  const agent = new Agent({ model: 'test', tools: [tool(() => { executions++; return 0; })],
    provider: { complete: () => call(String(++calls), '20') } });
  const result = await agent.run('Save forever');
  assert.equal(result.stop_reason, 'repeated_tool_result');
  assert.equal(calls, 2);
  assert.equal(executions, 0);
  assert.equal(result.stats.tool_rejections, 2);
});

test('rejected call and rejection count survive checkpoint resume', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-schema-resume-'));
  const store = new SessionStore({ path: directory });
  let calls = 0; let events = 0; let executions = 0;
  const config = { model: 'test', store, agentId: 'a', sessionId: 's',
    tools: [tool(() => { executions++; return 'saved'; })], provider: { complete() {
      if (++calls === 1) return call('bad', '20');
      if (calls === 2) return call('corrected', 20);
      return { content: 'Saved' };
    } } };
  try {
    const agent = new Agent({ ...config, onEvent(action) {
      if (action.type === 'model' && ++events === 2) throw new Error('interrupted');
    } });
    await assert.rejects(agent.run('Save'), /interrupted/);
    const snapshot = store.inspect({ agentId: 'a', sessionId: 's' });
    assert.equal(snapshot.run.tools[0].status, 'rejected');
    assert.equal(snapshot.run.in_flight, null);
    const resumed = new Agent({ ...config, resume: true });
    const result = await resumed.resume();
    assert.equal(result.stats.tool_rejections, 1);
    assert.equal(result.stats.tool_calls, 1);
    assert.equal(result.stats.model_calls, 3);
    assert.equal(executions, 1);
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
});
