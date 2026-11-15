"""Prove a catalog record can pass Node -> Python -> Node in the same HiveDB."""
import asyncio
import json
from pathlib import Path
import subprocess
import tempfile
from hive_sdk import SessionStore

ROOT = Path(__file__).resolve().parent.parent
MODULE = (ROOT / 'packages/npm/index.mjs').as_uri()


def node(directory, verify=False):
    script = '''
import assert from 'node:assert/strict';
import { SessionStore } from MODULE;
const store = await SessionStore.open({ path: process.argv[1], tenantId: 'parity' });
try {
  const memories = store.services.collection('memory');
  if (process.argv[2] === 'verify') {
    const row = await memories.get('shared');
    assert.equal(row.revision, 2);
    assert.equal(row.data.content, 'Actualizado desde Python');
    const hits = await memories.search('PYTHON');
    assert.equal(hits[0].record.id, 'shared');
  } else {
    await memories.create('shared', { title: 'Compartido', content: 'Creado desde Node' });
  }
} finally { await store.closeAsync(); }
'''.replace('MODULE', json.dumps(MODULE))
    subprocess.run(['node', '--input-type=module', '-e', script, directory, 'verify' if verify else 'create'],
                   cwd=ROOT, check=True, timeout=15)


async def main():
    with tempfile.TemporaryDirectory(prefix='hive-catalog-parity-') as directory:
        node(directory)
        store = await SessionStore.open(directory, tenant_id='parity')
        try:
            memory = store.services.collection('memory')
            row = await memory.get('shared')
            assert row['revision'] == 1 and row['data']['content'] == 'Creado desde Node'
            await memory.update('shared', row['revision'], {'content': 'Actualizado desde Python'})
            other = await store.for_tenant_async('other')
            try: assert await other.services.collection('memory').get('shared') is None
            finally: await other.close_async()
        finally:
            await store.close_async()
        node(directory, verify=True)
    print('Catalog parity Node -> Python -> Node passed')


if __name__ == '__main__':
    asyncio.run(main())
