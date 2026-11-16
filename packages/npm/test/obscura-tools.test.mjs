import assert from 'node:assert/strict';
import { test } from 'node:test';
import { chmodSync, copyFileSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { NativeTools } from '../index.mjs';

const fixture = fileURLToPath(new URL('../../../crates/hive-core/tests/fixtures/fake_obscura.py', import.meta.url));
const windows = process.platform === 'win32';

function setup() {
  const root = mkdtempSync(path.join(tmpdir(), 'hive-obscura-js-'));
  const executable = path.join(root, 'obscura');
  copyFileSync(fixture, executable); chmodSync(executable, 0o755);
  const starts = () => readFileSync(`${executable}.log`, 'utf8').trim().split('\n').map(line => JSON.parse(line)).filter(line => line.argv).length;
  return { root, executable, starts };
}

test('the obscura tools are absent unless configured and sensitive ones are opt-in', async () => {
  const { root, executable } = setup();
  try {
    const names = async config => (await new NativeTools({ root, ...config }).definitions()).map(tool => tool.name).filter(name => name.startsWith('obscura_'));
    assert.deepEqual(await names({}), []);
    const base = await names({ obscura: { executable } });
    assert.equal(base.length, 31);
    assert.ok(!base.includes('obscura_evaluate') && !base.includes('obscura_get_cookies'));
    assert.ok((await names({ obscura: { executable, allow_evaluate: true } })).includes('obscura_evaluate'));
    assert.ok((await names({ obscura: { executable, allow_credentials: true } })).includes('obscura_storage_state'));
    assert.throws(() => new NativeTools({ root, obscura: { call_timeout_ms: 0 } }), /timeouts/);
    await new NativeTools({ root, obscura: { executable } }).close();   // closing nothing is fine
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('installObscura needs the obscura tools and a valid install_dir; automatic installation can be turned off', async () => {
  const { root } = setup();
  try {
    await assert.rejects(new NativeTools({ root }).installObscura(), /disabled/);
    assert.throws(() => new NativeTools({ root, obscura: { install_dir: 'relative/bin' } }), /absolute/);
    assert.doesNotThrow(() => new NativeTools({ root, obscura: { install_dir: root, auto_install: false } }));
    // An explicit executable that is missing is an error, never a download.
    const tools = new NativeTools({ root, obscura: { executable: '/nonexistent/obscura', auto_install: false } });
    await assert.rejects(tools.execute('obscura_tab_list', {}), /does not exist/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('calls run through one Obscura session, binary results land in the sandbox, and close ends it', { skip: windows }, async () => {
  const { root, executable, starts } = setup();
  const tools = new NativeTools({ root, obscura: { executable, allowed_hosts: ['example.com'] } });
  try {
    const page = await tools.execute('obscura_navigate', { url: 'https://example.com/a' });
    assert.deepEqual(page, { text: 'URL: https://example.com/a\nTitle: Fake page', truncated: false });
    await assert.rejects(tools.execute('obscura_navigate', { url: 'https://evil.example/' }), /allowed_hosts/);
    await assert.rejects(tools.execute('obscura_navigate', { url: 'https://example.com/redir-bad' }), /ended on a blocked address/);
    const shot = await tools.execute('obscura_screenshot', { width: 32, height: 24, path: 'shots/p.png', create_dirs: true });
    assert.deepEqual([shot.width, shot.height, shot.path], [32, 24, 'shots/p.png']);
    assert.equal(readFileSync(path.join(root, 'shots/p.png')).subarray(1, 4).toString(), 'PNG');
    const agentTools = await tools.createTools();
    assert.equal((await agentTools.find(tool => tool.name === 'obscura_tab_list').execute({})).text, 'browser_tab_list ok {}');
    assert.equal(starts(), 1);
    await tools.close();
    await tools.execute('obscura_tab_list', {});
    assert.equal(starts(), 2, 'close() ends the session; the next call starts a new one');
    await assert.rejects(tools.execute('obscura_navigate', { url: 'https://example.com/hang', timeout_ms: 300 }), /TIMEOUT/);
    await tools.execute('obscura_tab_list', {});
    assert.equal(starts(), 3, 'a timed-out call discards its session');
  } finally { await tools.close(); rmSync(root, { recursive: true, force: true }); }
});
