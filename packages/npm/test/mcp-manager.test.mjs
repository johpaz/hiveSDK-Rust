import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Agent, McpManager, McpOperationUncertain } from '../index.mjs';

const fixture = fileURLToPath(new URL('../../../tests/fixtures/mcp_stdio.py', import.meta.url));
const server = (mode, receipt, extra = {}) => ({ command: 'python3', args: [fixture, mode, receipt], timeoutMs: 3000, ...extra });

test('a set of servers is reconciled, exposes qualified tools and reports failures per server', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-mcp-manager-'));
  const manager = new McpManager();
  try {
    const first = await manager.apply({ math: server('normal', path.join(dir, 'a')), pages: server('pages', path.join(dir, 'b')),
      missing: { command: '/nonexistent/mcp-server' }, off: server('normal', path.join(dir, 'o'), { enabled: false }) });
    assert.deepEqual([first.added.sort(), first.disabled, Object.keys(first.failed)], [['math', 'pages'], ['off'], ['missing']]);
    const states = Object.fromEntries(manager.status.map(row => [row.name, row.state]));
    assert.deepEqual(states, { math: 'connected', pages: 'connected', missing: 'failed', off: 'disabled' });
    const tools = manager.tools();
    assert.deepEqual(tools.map(tool => tool.name).sort(), ['math__add', 'math__echo', 'pages__add', 'pages__echo']);
    const add = tools.find(tool => tool.name === 'math__add');
    assert.equal((await add.execute({ a: 2, b: 5 }, {})).structuredContent.answer, 7);
    assert.deepEqual(manager.tools({ servers: ['pages'] }).map(tool => tool.name).sort(), ['pages__add', 'pages__echo']);
    await assert.rejects(manager.call('missing', 'add', { a: 1, b: 1 }), /MCP_SERVER_UNAVAILABLE.*reconnect/);
    // Reload: keep, restart, add, remove.
    const second = await manager.apply({ math: server('normal', path.join(dir, 'a')), pages: server('normal', path.join(dir, 'b')), fresh: server('normal', path.join(dir, 'f')) });
    assert.deepEqual([second.unchanged, second.restarted, second.added, second.removed.sort()], [['math'], ['pages'], ['fresh'], ['missing', 'off']]);
    assert.equal((await manager.call('fresh', 'add', { a: 1, b: 2 })).structuredContent.answer, 3);
    // An invalid set changes nothing.
    const before = JSON.stringify(manager.status);
    await assert.rejects(manager.apply({ 'bad name': server('normal', path.join(dir, 'x')) }), /server name/);
    await assert.rejects(manager.apply([]), /object/);
    assert.equal(JSON.stringify(manager.status), before);
    // The tools work inside an Agent.
    const provider = { n: 0, complete() { this.n++; return this.n === 1
      ? { content: null, tool_calls: [{ id: 'c1', name: 'math__add', arguments: { a: 20, b: 22 } }], usage: { input_tokens: 1, output_tokens: 1 } }
      : { content: 'ok', usage: { input_tokens: 1, output_tokens: 1 } }; } };
    const agent = new Agent({ model: 'm', provider, tools: manager.tools(), maxTurns: 4 });
    assert.equal((await agent.run('suma')).content, 'ok');
    agent.close();
  } finally { await manager.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('an uncertain call closes its server and is never replayed; reconnect starts a new session', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-mcp-manager-'));
  const receipt = path.join(dir, 'calls');
  const manager = new McpManager();
  try {
    await manager.apply({ flaky: server('exit', receipt) });
    await assert.rejects(manager.call('flaky', 'add', { a: 1, b: 1 }), McpOperationUncertain);
    assert.equal(manager.status[0].state, 'closed');
    await assert.rejects(manager.call('flaky', 'add', { a: 1, b: 1 }), /is closed; reconnect it/);
    await new Promise(resolve => setTimeout(resolve, 200));
    assert.equal(readFileSync(receipt, 'utf8').trim().split('\n').length, 1, 'the failed call was sent once');
    assert.equal((await manager.reconnect('flaky')).connected, true);
    assert.equal(manager.status[0].state, 'connected');
    await assert.rejects(manager.reconnect('ghost'), /unknown MCP server/);
  } finally { await manager.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('a manager reports a changed tool list and refreshTools updates its qualified catalog', async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-mcp-manager-refresh-'));
  const manager = new McpManager();
  try {
    await manager.apply({ live: server('mutating', path.join(dir, 'a')) });
    assert.deepEqual(manager.catalog.map(tool => tool.name).sort(), ['live__add', 'live__echo']);
    assert.equal(manager.status[0].tools_changed, false);
    await manager.call('live', 'echo', { value: 'mutate' });
    assert.equal(manager.status[0].tools_changed, true);
    assert.deepEqual(await manager.refreshTools('live'), { server: 'live', added: ['extra'], removed: [], changed: ['add'] });
    assert.deepEqual([manager.status[0].tools, manager.status[0].tools_changed], [3, false]);
    assert.deepEqual(manager.catalog.map(tool => tool.name).sort(), ['live__add', 'live__echo', 'live__extra']);
    await assert.rejects(manager.refreshTools('nobody'), /unknown MCP server/);
  } finally { await manager.close?.(); rmSync(dir, { recursive: true, force: true }); }
});
