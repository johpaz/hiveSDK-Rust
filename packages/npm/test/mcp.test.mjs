import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as sleep } from 'node:timers/promises';
import { Agent, McpClient, McpOperationUncertain, ToolOperationUncertain, SessionStore } from '../index.mjs';

const fixture = fileURLToPath(new URL('../../../tests/fixtures/mcp_stdio.py', import.meta.url));
const connect = (mode, receipt, options = {}) => McpClient.connect({ command: 'python3', args: [fixture, mode, receipt], timeoutMs: 5000, ...options });
const rows = file => existsSync(file) ? readFileSync(file, 'utf8').trim().split('\n').filter(Boolean).map(line => JSON.parse(line)) : [];
const call = args => ({ content: null, tool_calls: [{ id: 'c', name: 'fs_add', arguments: args }], usage: { input_tokens: 1, output_tokens: 1 } });
async function waitReceipt(file) { const end = Date.now() + 3000; while (!rows(file).length) { if (Date.now() > end) assert.fail('tool did not reach peer'); await sleep(10); } }

test('MCP catalog tools use shared schema rejection and durable execution IDs', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-mcp-')); const receipt = path.join(directory, 'calls'); const client = await connect('pages', receipt); const store = new SessionStore({ path: path.join(directory, 'db') }); let requests = 0;
  try {
    assert.equal(client.info.protocolVersion, '2025-11-25'); assert.equal(client.catalog.length, 2);
    assert.throws(() => client.tools({ names: ['missing'] })); assert.throws(() => client.tools({ names: ['add', 'add'] }));
    const catalog = client.catalog; catalog[0].name = 'changed'; assert.equal(client.catalog[0].name, 'add');
    const agent = new Agent({ model: 'test', system: 'System', store, agentId: 'a', sessionId: 's', tools: client.tools({ prefix: 'fs_', names: ['add'] }),
      provider: { complete(request) {
        requests++; assert.equal(request.messages[0].content, 'System');
        if (requests === 1) return call({ a: '2', b: 3 });
        if (requests === 2) { assert.equal(rows(receipt).length, 0); return call({ a: 2, b: 3 }); }
        assert.equal(JSON.parse(request.messages.at(-1).content).structuredContent.answer, 5);
        return { content: 'done' };
      } } });
    const result = await agent.run('Add'); assert.equal(result.content, 'done'); assert.equal(result.stats.tool_rejections, 1); assert.equal(result.stats.tool_calls, 1);
    const run = store.inspect({ agentId: 'a', sessionId: 's' }).run;
    assert.equal(rows(receipt).length, 1); assert.equal(rows(receipt)[0].meta['hive/context'].execution_id, run.tools[1].execution_id);
  } finally { store.close(); await client.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('MCP stdout loss retains an uncertain tool and verified resolution resumes without replay', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-mcp-lost-')); const receipt = path.join(directory, 'calls'); const client = await connect('exit', receipt); const store = new SessionStore({ path: path.join(directory, 'db') }); let requests = 0; let replacement;
  const provider = { complete() { return ++requests === 1 ? call({ a: 2, b: 3 }) : { content: 'resolved', usage: { input_tokens: 1, output_tokens: 1 } }; } };
  const config = { model: 'test', store, agentId: 'a', sessionId: 's', policy: { max_total_output_tokens: 5 }, provider };
  try {
    await assert.rejects(new Agent({ ...config, tools: client.tools({ prefix: 'fs_', names: ['add'] }) }).run('Add'), McpOperationUncertain);
    assert.equal(requests, 1); assert.equal(rows(receipt).length, 1);
    const snapshot = store.inspect({ agentId: 'a', sessionId: 's' }); assert.equal(snapshot.run.in_flight.kind, 'tool'); assert.equal(snapshot.run.tools[0].status, 'started');
    assert.throws(() => new Agent({ ...config, tools: client.tools({ prefix: 'fs_', names: ['add'] }), resume: true }), /SESSION_OPERATION_UNCERTAIN/);
    store.resolveTool({ agentId: 'a', sessionId: 's', revision: snapshot.revision, executionId: snapshot.run.in_flight.execution_id,
      result: { content: [{ type: 'text', text: '5' }], structuredContent: { answer: 5 } } });
    const secondReceipt = path.join(directory, 'second'); replacement = await connect('normal', secondReceipt);
    const result = await new Agent({ ...config, tools: replacement.tools({ prefix: 'fs_', names: ['add'] }), resume: true }).resume();
    assert.equal(result.content, 'resolved'); assert.equal(requests, 2); assert.equal(rows(secondReceipt).length, 0); assert.equal(rows(receipt).length, 1);
  } finally { store.close(); await client.close(); await replacement?.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('confirmed MCP tool errors become feedback with their original result', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-mcp-error-')); const receipt = path.join(directory, 'calls'); const client = await connect('tool_error', receipt); let requests = 0;
  try {
    const agent = new Agent({ model: 'test', tools: client.tools({ prefix: 'fs_', names: ['add'] }), provider: { complete(request) {
      if (++requests === 1) return call({ a: 2, b: 3 });
      const feedback = JSON.parse(request.messages.at(-1).content); assert.equal(feedback.error.code, 'MCP_TOOL_ERROR'); assert.equal(feedback.error.result.isError, true);
      return { content: 'handled' };
    } } });
    assert.equal((await agent.run('Add')).content, 'handled'); assert.equal(rows(receipt).length, 1);
  } finally { await client.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('agent cancellation closes MCP while leaving the dispatched tool uncertain', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-mcp-cancel-')); const receipt = path.join(directory, 'calls'); const client = await connect('wait', receipt); const store = new SessionStore({ path: path.join(directory, 'db') });
  try {
    const agent = new Agent({ model: 'test', store, agentId: 'a', sessionId: 's', tools: client.tools({ prefix: 'fs_', names: ['add'] }), provider: { complete: () => call({ a: 2, b: 3 }) } });
    const pending = agent.run('Add'); const failure = assert.rejects(pending, /CANCELLED/); await waitReceipt(receipt); agent.cancel(); await failure;
    assert.equal(store.inspect({ agentId: 'a', sessionId: 's' }).run.in_flight.kind, 'tool'); assert.equal(rows(receipt).length, 1);
  } finally { store.close(); await client.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('MCP discovery rejects invalid schemas before an agent can use them', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-mcp-schema-'));
  try { await assert.rejects(connect('bad_schema', path.join(directory, 'calls')), /INVALID_TOOL_SCHEMA/); }
  finally { rmSync(directory, { recursive: true, force: true }); }
});

test('custom uncertain tool failures never become a new paid model continuation', async () => {
  let requests = 0;
  const agent = new Agent({ model: 'test', tools: [{ name: 'fs_add', description: 'Add', parameters: { type: 'object' }, execute() { throw new ToolOperationUncertain(); } }],
    provider: { complete() { requests++; return call({ a: 2, b: 3 }); } } });
  await assert.rejects(agent.run('Add'), ToolOperationUncertain); assert.equal(requests, 1);
});
