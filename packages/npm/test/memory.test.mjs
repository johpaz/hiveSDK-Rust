import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, SessionStore } from '../index.mjs';

async function populate(agent) { await agent.run('Pedido R7, entrega viernes 🦀'); await agent.run('Factura R8'); }
function plan(agent) { return { summary: { id: 'summary-v1', source: agent.captureContext(1, 5), content: 'R7 viernes, R8 pagada' }, retrieved: [] }; }

test('local retrieval and supplied summary preserve history with no auxiliary inference', async () => {
  let calls = 0; let request;
  const agent = new Agent({ model: 'test', system: 'System', provider: { complete(value) { calls++; request = value; return { content: 'done' }; } } });
  await populate(agent); const original = agent.messages;
  const source = agent.captureContext(1, 3);
  assert.equal(source.sha256, createHash('sha256').update(JSON.stringify(original.slice(1, 3))).digest('hex'));
  assert.deepEqual(agent.readContext(source), original.slice(1, 3));
  const found = agent.searchContext('R7 viernes');
  assert.equal(found.hits[0].source.start, 1); assert.equal(found.hits[0].score, 2);
  assert.equal(found.serialized_bytes, Buffer.byteLength(JSON.stringify(found.hits)));
  assert.deepEqual(agent.searchContext('R7', { maxBytes: 2 }).hits, []);
  assert.equal(calls, 2);
  const selected = plan(agent); selected.retrieved.push(source); agent.setContextPlan(selected);
  assert.equal(calls, 2); await agent.run('Next');
  assert.equal(request.context.summary_id, 'summary-v1'); assert.equal(request.context.dropped_messages, 2);
  assert.equal(request.messages[1].role, 'user'); assert.equal(JSON.parse(request.messages[1].content).context_summary.id, 'summary-v1');
  assert.equal(request.messages[2].content, original[1].content);
  assert.deepEqual(agent.messages.slice(0, 5), original);
  agent.setContextPlan(null); await agent.run('Restored');
  assert.equal(request.messages[1].content, original[1].content); assert.equal(request.context, undefined);
  agent.close();
});

test('context plans survive reopening and stale writers cannot replace them', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-memory-')); let store;
  const config = { model: 'test', system: 'System', agentId: 'a', sessionId: 's', provider: { complete() { return { content: 'done' }; } } };
  try {
    store = new SessionStore({ path: directory }); let agent = new Agent({ ...config, store }); await populate(agent);
    const stale = new Agent({ ...config, store }); const selected = plan(agent); agent.setContextPlan(selected);
    assert.throws(() => stale.setContextPlan(null), /SESSION_CONFLICT/); stale.close(); agent.close(); store.close();
    store = new SessionStore({ path: directory }); agent = new Agent({ ...config, store });
    assert.deepEqual(agent.contextPlan, selected);
    const copy = agent.contextPlan; copy.summary.content = 'tampered'; assert.equal(agent.contextPlan.summary.content, selected.summary.content);
    await agent.run('Next'); assert.equal(agent.messages.length, 7); agent.close();
  } finally { store?.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('invalid sources and duplicate retrieval fail before any new inference', async () => {
  let calls = 0;
  const agent = new Agent({ model: 'test', system: 'System', provider: { complete() { calls++; return { content: 'done' }; } } });
  await populate(agent);
  assert.throws(() => agent.captureContext(1, 2), /closed user turns/);
  const bad = plan(agent); bad.summary.source.sha256 = '0'.repeat(64);
  assert.throws(() => agent.setContextPlan(bad), /hash/);
  const source = agent.captureContext(1, 3);
  assert.throws(() => agent.setContextPlan({ retrieved: [source, source] }), /distinct/);
  assert.throws(() => agent.searchContext('!!!'), /lexical terms/);
  assert.equal(calls, 2); assert.equal(agent.contextPlan, null); agent.close();
  assert.throws(() => agent.searchContext('R7'), /CLOSED/);
});

test('an oversized pinned summary stops before reserving another model call', async () => {
  let calls = 0;
  const agent = new Agent({ model: 'test', system: 'System', policy: { max_context_bytes: 1400, context_overflow: 'drop_oldest_turns', max_total_output_tokens: 20 },
    provider: { complete() { calls++; return { content: 'done', usage: { input_tokens: 1, output_tokens: 1 } }; } } });
  await populate(agent); const selected = plan(agent); selected.summary.content = 'x'.repeat(2000); agent.setContextPlan(selected);
  const result = await agent.run('Next');
  assert.equal(result.stop_reason, 'context_limit'); assert.equal(calls, 2);
  assert.equal(result.stats.reserved_output_tokens, 0); assert.equal(agent.messages.length, 6); agent.close();
});

test('retrieval retains a whole tool batch and busy agents reject context mutation', async () => {
  let calls = 0; let agent;
  agent = new Agent({ model: 'test', tools: [{ name: 'lookup', description: 'Lookup', parameters: { type: 'object' }, execute() {
    assert.throws(() => agent.searchContext('R9'), /BUSY/); return { delivered: 'martes' };
  } }], provider: { complete() {
    return ++calls === 1 ? { content: null, tool_calls: [{ id: 'c', name: 'lookup', arguments: { receipt: 'R9' } }] } : { content: 'R9 confirmed' };
  } } });
  await agent.run('Lookup receipt'); const hit = agent.searchContext('R9 martes').hits[0];
  assert.equal(hit.messages.length, 4); assert.equal(hit.messages[1].tool_calls[0].id, 'c'); assert.equal(hit.messages[2].tool_call_id, 'c');
  agent.close();
});
