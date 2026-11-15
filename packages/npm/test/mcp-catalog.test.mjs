import test from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { McpClient, McpOperationUncertain, Browser } from '../index.mjs';

const fixture = fileURLToPath(new URL('../../../tests/fixtures/mcp_stdio.py', import.meta.url));

test('native MCP resources/prompts work without tools and preserve explicit pagination', async () => {
  const client = await McpClient.connect({ command: 'python3', args: [fixture, 'catalog'], timeoutMs: 2000 });
  try {
    assert.deepEqual(client.catalog, []);
    const resources = await client.listResources();
    assert.equal(resources.resources[0].uri, 'test://guide');
    assert.deepEqual((await client.listResources({ cursor: resources.nextCursor })).resources, []);
    assert.equal((await client.readResource('test://guide')).contents[0].text, 'Use the SDK');
    assert.equal((await client.listPrompts()).prompts[0].name, 'review');
    assert.equal((await client.getPrompt('review', { topic: 'Rust' })).messages[0].content.text, 'Review Rust');
  } finally { await client.close(); }
});

test('malformed resource responses close the session and are never retried', async () => {
  const client = await McpClient.connect({ command: 'python3', args: [fixture, 'bad_resource'], timeoutMs: 2000 });
  try {
    await assert.rejects(client.readResource('test://guide'), McpOperationUncertain);
    await assert.rejects(client.readResource('test://guide'), /closed/);
  } finally { await client.close(); }
});

test('browser binding rejects incompatible endpoints before opening a connection', async () => {
  await assert.rejects(Browser.connect('http://127.0.0.1:1/page'), /ws\/wss/);
  await assert.rejects(Browser.connect('ws://user:secret@127.0.0.1/page'), /credentials/);
});
