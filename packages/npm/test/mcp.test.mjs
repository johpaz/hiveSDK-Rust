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

test('a server that changes its tools is noticed and refreshTools adopts the new list', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-mcp-refresh-')); const client = await connect('mutating', path.join(directory, 'calls'));
  try {
    assert.deepEqual(client.catalog.map(tool => tool.name), ['add', 'echo']); assert.equal(client.toolsChanged, false);
    assert.equal((await client.call('echo', { value: 'mutate' })).structuredContent.value, 'mutate');
    assert.equal(client.toolsChanged, true, 'announced with the reply that carried it');
    assert.deepEqual(client.catalog.map(tool => tool.name), ['add', 'echo'], 'unchanged until refreshed');
    assert.deepEqual(await client.refreshTools(), { added: ['extra'], removed: [], changed: ['add'] });
    assert.equal(client.toolsChanged, false);
    assert.deepEqual(client.catalog.map(tool => tool.name), ['add', 'echo', 'extra']);
    assert.ok(client.tools().some(tool => tool.name === 'mcp_extra'), 'tools() offers the new one');
    assert.equal((await client.call('extra', { value: 1 })).structuredContent.value, 1);
  } finally { await client.close(); rmSync(directory, { recursive: true, force: true }); }
});

// A minimal MCP server over the classic HTTP+SSE transport, as n8n's MCP Server Trigger speaks it.
async function sseServer() {
  const { createServer } = await import('node:http'); let stream; const seen = [];
  const server = createServer((request, response) => {
    if (request.method === 'GET') { response.writeHead(200, { 'content-type': 'text/event-stream' }); stream = response; response.write(': hi\n\nevent: endpoint\ndata: /messages?sessionId=s1\n\n'); return; }
    let body = ''; request.on('data', chunk => body += chunk); request.on('end', () => {
      response.writeHead(202); response.end('Accepted'); const message = JSON.parse(body); seen.push(message.method); if (message.id === undefined) return;
      const result = message.method === 'initialize' ? { protocolVersion: '2025-03-26', capabilities: { tools: {} }, serverInfo: { name: 'n8n-like', version: '1' } }
        : message.method === 'tools/list' ? { tools: [{ name: 'echo', description: 'Echo', inputSchema: { type: 'object', properties: { value: {} }, required: ['value'] } }] }
        : { content: [{ type: 'text', text: JSON.stringify(message.params.arguments.value) }], structuredContent: { value: message.params.arguments.value } };
      stream.write(`event: message\ndata: ${JSON.stringify({ jsonrpc: '2.0', id: message.id, result })}\n\n`);
    });
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  return { url: `http://127.0.0.1:${server.address().port}/mcp/x`, seen, close: () => { stream?.end(); server.closeAllConnections(); server.close(); } };
}

test('the classic HTTP+SSE transport connects, lists and calls, from a client and from a manager', async () => {
  const server = await sseServer(); let client; let manager;
  try {
    client = await McpClient.connect({ transport: 'sse', url: server.url });
    assert.equal(client.info.protocolVersion, '2025-03-26'); assert.deepEqual(client.catalog.map(tool => tool.name), ['echo']);
    assert.deepEqual((await client.call('echo', { value: { n: 2 } })).structuredContent, { value: { n: 2 } });
    assert.deepEqual(server.seen, ['initialize', 'notifications/initialized', 'tools/list', 'tools/call']);
    await assert.rejects(McpClient.connect({ transport: 'sse', url: server.url, command: 'x' }), /Process options/);
    const { McpManager } = await import('../index.mjs'); manager = new McpManager();
    const report = await manager.apply({ calendar: { transport: 'sse', url: server.url } });
    assert.deepEqual(report.added, ['calendar']); assert.equal(manager.status[0].state, 'connected');
    assert.deepEqual(manager.catalog.map(tool => tool.name), ['calendar__echo']);
  } finally { await client?.close(); await manager?.close?.(); server.close(); }
});
