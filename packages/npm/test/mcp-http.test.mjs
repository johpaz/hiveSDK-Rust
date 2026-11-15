import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createServer } from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, McpClient, McpOperationUncertain, SessionStore } from '../index.mjs';

const declaration = { name: 'add', description: 'Add', inputSchema: { type: 'object', properties: { a: { type: 'integer' }, b: { type: 'integer' } }, required: ['a', 'b'] } };
const call = () => ({ content: null, tool_calls: [{ id: 'c', name: 'remote_add', arguments: { a: 2, b: 3 } }] });
async function serve(handler) {
  let calls = 0;
  const server = createServer(async (req, res) => {
    assert.equal(req.url, '/mcp'); assert.equal(req.headers.authorization, 'Bearer test-only');
    if (req.method === 'DELETE') { res.writeHead(204); res.end(); return; }
    let raw = ''; for await (const chunk of req) raw += chunk; const message = JSON.parse(raw);
    if (message.method === 'initialize') {
      assert.deepEqual(message.params.capabilities, {}); res.setHeader('Mcp-Session-Id', 'session-1'); res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify({ jsonrpc: '2.0', id: message.id, result: { protocolVersion: '2025-11-25', serverInfo: { name: 'local-http', version: '1' }, capabilities: { tools: {} } } })); return;
    }
    assert.equal(req.headers['mcp-session-id'], 'session-1'); assert.equal(req.headers['mcp-protocol-version'], '2025-11-25');
    if (message.method === 'notifications/initialized') { res.writeHead(202); res.end(); return; }
    if (message.method === 'tools/list') { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify({ jsonrpc: '2.0', id: message.id, result: { tools: [declaration] } })); return; }
    assert.equal(message.method, 'tools/call'); calls++; await handler(message, res);
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  return { connect: () => McpClient.connect({ transport: 'http', url: `http://127.0.0.1:${server.address().port}/mcp`, headers: { Authorization: 'Bearer test-only' }, timeoutMs: 5000 }),
    get calls() { return calls; }, async close() { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); } };
}

test('MCP HTTP JSON and SSE results use the same agent tools and known result accounting', async () => {
  for (const streaming of [false, true]) {
    const server = await serve(async (message, res) => {
      const result = { jsonrpc: '2.0', id: message.id, result: { content: [{ type: 'text', text: '5' }], structuredContent: { answer: 5 } } };
      res.setHeader('Content-Type', streaming ? 'text/event-stream' : 'application/json');
      res.end(streaming ? `data: ${JSON.stringify(result)}\n\n` : JSON.stringify(result));
    });
    const client = await server.connect(); let models = 0;
    try {
      const agent = new Agent({ model: 'test', tools: client.tools({ prefix: 'remote_' }), provider: { complete(request) {
        if (++models === 1) return call(); assert.equal(JSON.parse(request.messages.at(-1).content).structuredContent.answer, 5); return { content: 'done' };
      } } });
      const result = await agent.run('Add'); assert.equal(result.content, 'done'); assert.equal(result.stats.tool_calls, 1); assert.equal(server.calls, 1);
    } finally { await client.close(); await server.close(); }
  }
});

test('MCP HTTP response loss leaves a durable uncertain tool and never repeats its POST', async () => {
  const server = await serve(async (_message, res) => res.destroy()); const client = await server.connect();
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-mcp-http-lost-')); const store = new SessionStore({ path: directory }); let models = 0;
  const config = { model: 'test', store, agentId: 'a', sessionId: 's', tools: client.tools({ prefix: 'remote_' }), provider: { complete() { models++; return call(); } } };
  try {
    await assert.rejects(new Agent(config).run('Add'), McpOperationUncertain); assert.equal(server.calls, 1); assert.equal(models, 1);
    assert.equal(store.inspect({ agentId: 'a', sessionId: 's' }).run.in_flight.kind, 'tool');
    assert.throws(() => new Agent({ ...config, resume: true }), /SESSION_OPERATION_UNCERTAIN/);
    await assert.rejects(client.call('add', { a: 2, b: 3 }), /closed/); assert.equal(server.calls, 1);
  } finally { store.close(); await client.close(); await server.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('MCP HTTP cancellation closes the response while preserving the tool claim', async () => {
  let ready, closed; const received = new Promise(resolve => { ready = resolve; }); const ended = new Promise(resolve => { closed = resolve; });
  const server = await serve(async (_message, res) => { res.once('close', closed); ready(); }); const client = await server.connect();
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-mcp-http-cancel-')); const store = new SessionStore({ path: directory });
  try {
    const agent = new Agent({ model: 'test', store, agentId: 'a', sessionId: 's', tools: client.tools({ prefix: 'remote_' }), provider: { complete: call } });
    const pending = assert.rejects(agent.run('Add'), /CANCELLED/); await received; agent.cancel(); await pending; await ended;
    assert.equal(store.inspect({ agentId: 'a', sessionId: 's' }).run.in_flight.kind, 'tool'); assert.equal(server.calls, 1);
  } finally { store.close(); await client.close(); await server.close(); rmSync(directory, { recursive: true, force: true }); }
});
