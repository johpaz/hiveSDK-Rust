import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { NativeTools } from '../index.mjs';

const scratch = () => mkdtempSync(path.join(tmpdir(), 'hive-tools-'));

test('filesystem tools run inside the root and reject escapes', async () => {
  const outer = scratch();
  const root = path.join(outer, 'root');
  mkdirSync(root);
  writeFileSync(path.join(outer, 'secret.txt'), 'secret');
  try {
    const tools = new NativeTools({ root });
    const names = (await tools.definitions()).map(tool => tool.name);
    assert.deepEqual(names, ['fs_read', 'fs_write', 'fs_edit', 'fs_list', 'fs_glob', 'fs_grep']);

    await tools.execute('fs_write', { path: 'notes/a.txt', content: 'uno\ndos\n', create_dirs: true });
    assert.equal(readFileSync(path.join(root, 'notes/a.txt'), 'utf8'), 'uno\ndos\n');
    const read = await tools.execute('fs_read', { path: 'notes/a.txt', offset: 1 });
    assert.equal(read.content, 'dos\n');
    await tools.execute('fs_edit', { path: 'notes/a.txt', old: 'dos', new: '2' });
    const hits = await tools.execute('fs_grep', { pattern: '2' });
    assert.deepEqual(hits.matches.map(hit => hit.path), ['notes/a.txt']);

    await assert.rejects(tools.execute('fs_read', { path: '../secret.txt' }), /relative|escape/);
    await assert.rejects(tools.execute('fs_read', { path: '/etc/passwd' }), /relative/);
    await assert.rejects(tools.execute('command_run', { program: 'echo' }), /unknown or disabled/);
    assert.throws(() => new NativeTools({ root, nonsense: true }), /unknown field/i);
  } finally { rmSync(outer, { recursive: true, force: true }); }
});

test('commands honour the allowlist, timeout and cancellation', { skip: process.platform === 'win32' }, async () => {
  const root = scratch();
  try {
    const tools = new NativeTools({ root, command: { allowed_programs: ['echo', 'sleep'], max_timeout_ms: 5000 } });
    const echoed = await tools.execute('command_run', { program: 'echo', args: ['a;b'] });
    assert.equal(echoed.stdout, 'a;b\n');
    await assert.rejects(tools.execute('command_run', { program: 'rm' }), /allowlist/);
    const slow = await tools.execute('command_run', { program: 'sleep', args: ['30'], timeout_ms: 200 });
    assert.equal(slow.timed_out, true);

    const pending = tools.execute('command_run', { program: 'sleep', args: ['30'] });
    setTimeout(() => tools.cancel(), 150);
    await assert.rejects(pending, /CANCELLED/);
    assert.equal((await tools.execute('command_run', { program: 'echo', args: ['again'] })).stdout, 'again\n');

    const bound = (await tools.createTools()).find(tool => tool.name === 'command_run');
    const controller = new AbortController();
    const operation = bound.execute({ program: 'sleep', args: ['30'] }, { signal: controller.signal });
    setTimeout(() => controller.abort(), 150);
    await assert.rejects(operation, /CANCELLED/);
    await assert.rejects(bound.execute({ program: 'echo' }, { signal: controller.signal }), /CANCELLED/);

    const immediate = new AbortController();
    const queued = bound.execute({ program: 'sleep', args: ['30'] }, { signal: immediate.signal });
    const independent = tools.execute('command_run', { program: 'echo', args: ['independent'] });
    immediate.abort(); // Before the native async body is necessarily polled.
    await assert.rejects(queued, /CANCELLED/);
    assert.equal((await independent).stdout, 'independent\n');
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('web tools fetch and search, and expose executable Agent tools', async () => {
  const root = scratch();
  const server = createServer((request, response) => {
    response.setHeader('content-type', 'application/json');
    response.end(request.url.startsWith('/search')
      ? JSON.stringify({ web: { results: [{ title: 'T', url: 'https://x', description: 'D' }] } })
      : '{"hello":"world"}');
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  try {
    const guarded = new NativeTools({ root, web: {} });
    await assert.rejects(guarded.execute('web_fetch', { url: `${base}/doc` }), /non-public/);

    const tools = new NativeTools({ root, web: { allow_private_hosts: true, search: { kind: 'brave', api_key: 'k', endpoint: `${base}/search` } } });
    const fetched = await tools.execute('web_fetch', { url: `${base}/doc` });
    assert.equal(fetched.status, 200);
    assert.equal(fetched.body, '{"hello":"world"}');
    const found = await tools.execute('web_search', { query: 'hive', limit: 1 });
    assert.deepEqual(found.results, [{ title: 'T', url: 'https://x', snippet: 'D' }]);

    const agentTools = await tools.createTools();
    const search = agentTools.find(tool => tool.name === 'web_search');
    assert.equal(typeof search.execute, 'function');
    assert.equal((await search.execute({ query: 'hive' })).results.length, 1);
  } finally { server.close(); rmSync(root, { recursive: true, force: true }); }
});

test('computer tools are off unless configured, input is a separate grant and the host controls reach the engine', async () => {
  const dir = scratch();
  try {
    const none = new NativeTools({ root: dir });
    assert.deepEqual((await none.definitions()).filter(tool => tool.name.startsWith('computer_')), []);
    const screenshots = new NativeTools({ root: dir, computer: { backend: 'x11', display: ':99' } });
    assert.deepEqual((await screenshots.definitions()).filter(tool => tool.name.startsWith('computer_')).map(tool => tool.name), ['computer_screenshot', 'computer_wait']);
    const full = new NativeTools({ root: dir, computer: { backend: 'x11', display: ':99', allow_input: true, max_actions: 2 } });
    assert.equal((await full.definitions()).filter(tool => tool.name.startsWith('computer_')).length, 8);
    assert.deepEqual(await full.computerStatus(), { stopped: false, actions: 0 });
    assert.deepEqual(await full.computerStop(), { stopped: true, actions: 0 });
    await assert.rejects(full.execute('computer_screenshot', {}), /stopped|not granted|disabled/);
    assert.deepEqual(await full.computerResume(), { stopped: false, actions: 0 });
    assert.deepEqual(await full.computerReset(), { stopped: false, actions: 0 });
    assert.throws(() => new NativeTools({ root: dir, computer: { allow_screenshot: false } }), /allow_screenshot or allow_input/);
    assert.throws(() => new NativeTools({ root: dir, computer: { surprise: true } }), /unknown field/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
