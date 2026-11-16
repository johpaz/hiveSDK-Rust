"""Native vault crash/reopen/rotation and filesystem exchange: Node -> Python -> Node.

Uses fixed fixture keys and isolated storage; never reads a real profile/keyring.
"""
import asyncio
import json
from pathlib import Path
import subprocess
import tempfile
from hive_sdk import SessionStore, NativeTools

ROOT = Path(__file__).resolve().parent.parent
MODULE = (ROOT / 'packages/npm/index.mjs').as_uri()


def node(directory, mode):
    script = '''
import assert from 'node:assert/strict';
import { SessionStore, NativeTools } from MODULE;
const [directory, mode] = process.argv.slice(1);
const store = await SessionStore.open({ path: directory + '/db', tenantId: 'cross' });
const tools = new NativeTools({ root: directory });
if (mode === 'crash') {
  const vault = await store.openSecrets({ key: new Uint8Array(32).fill(7), keyId: 'first' });
  await vault.put('api', 'fixture-one');
  await vault.put('retired', 'fixture-retired');
  await tools.execute('fs_write', { path: 'exchange.txt', content: 'node\\n' });
  process.exit(17); // Commit acknowledged; no orderly vault/store cleanup.
} else {
  await assert.rejects(store.openSecrets({ key: new Uint8Array(32).fill(7), keyId: 'first' }), /AUTHENTICATION/);
  const vault = await store.openSecrets({ key: new Uint8Array(32).fill(8), keyId: 'second' });
  assert.equal(await vault.reveal('api'), 'fixture-two');
  assert.equal((await vault.inspect('api')).revision, 3);
  assert.equal(await vault.reveal('retired'), null);
  assert.equal((await vault.inspect('retired')).revision, 3);
  assert.equal((await vault.list()).items.length, 1);
  assert.equal((await vault.list({ includeDeleted: true })).items.length, 2);
  assert.equal((await vault.info()).encryptions, 7);
  assert.equal((await tools.execute('fs_read', { path: 'exchange.txt' })).content, 'node\\npython\\n');
  const otherStore = await store.forTenantAsync('other');
  const other = await otherStore.openSecrets({ key: new Uint8Array(32).fill(8), keyId: 'second' });
  assert.equal(await other.reveal('api'), null);
  await other.close(); await otherStore.closeAsync(); await vault.close(); await store.closeAsync();
}
'''.replace('MODULE', json.dumps(MODULE))
    result = subprocess.run(['node', '--input-type=module', '-e', script, directory, mode],
                            cwd=ROOT, capture_output=True, text=True, timeout=20)
    assert result.returncode == (17 if mode == 'crash' else 0), result.stderr


async def main():
    with tempfile.TemporaryDirectory(prefix='hive-secrets-parity-') as directory:
        node(directory, 'crash')
        store = await SessionStore.open(str(Path(directory) / 'db'), tenant_id='cross')
        vault = await store.open_secrets(key=bytes([7])*32, key_id='first')
        try:
            assert await vault.reveal('api') == 'fixture-one'
            receipt = await vault.rekey(key_id='second', key=bytes([8])*32)
            assert receipt['retained_records'] == 2 and receipt['encryptions'] == 3
            assert (await vault.inspect('api'))['revision'] == 2
            await vault.put('api', 'fixture-two', revision=2)
            await vault.delete('retired', 2)
            tools = NativeTools({'root': directory})
            await tools.execute('fs_edit', {'path': 'exchange.txt', 'old': 'node\n', 'new': 'node\npython\n'})
        finally:
            await vault.close()
            await store.close_async()
        node(directory, 'verify')
    print('Secrets crash/reopen/rotation and filesystem exchange Node -> Python -> Node passed')


if __name__ == '__main__':
    asyncio.run(main())
