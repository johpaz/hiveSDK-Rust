"""Abrupt Node exit -> Python recovery/reconciliation -> Node readback.

All effects are fixture files inside a temporary directory; no models/accounts.
"""
import asyncio
import json
from pathlib import Path
import subprocess
import tempfile
import time
from hive_sdk import SessionStore, Objective

ROOT = Path(__file__).resolve().parent.parent
MODULE = (ROOT / 'packages/npm/index.mjs').as_uri()


def node(directory, mode):
    script = '''
import assert from 'node:assert/strict';
import { appendFileSync, readFileSync } from 'node:fs';
import { SessionStore, Objective } from MODULE;
const directory = process.argv[1]; const mode = process.argv[2];
const store = await SessionStore.open({ path: directory + '/db', tenantId: 'cross' });
const root = await Objective.open({ max_tool_calls: 2 }, { store, id: 'root' });
const queue = await store.openQueue('jobs', { lease_ms: 100 });
const input = { kind: 'effect', lane: 'same', payload: { fixture: true }, objective_id: 'root', idempotency_key: 'once' };
const job = await queue.enqueue(input);
if (mode === 'crash') {
  const claim = await queue.claim('interrupted-worker', ['effect']);
  appendFileSync(directory + '/effects', 'one fixture effect\\n');
  // Deliberately exit before finish or close; leave the durable lease in flight.
  process.stdout.write(JSON.stringify({ id: claim.id, execution_id: claim.execution_id }), () => process.exit(17));
} else {
  const recovered = await queue.get(job.id);
  assert.equal(recovered.status, 'completed');
  assert.equal(recovered.attempts, 1);
  assert.equal(recovered.result.receipt, 'fixture-one');
  assert.equal((await root.inspectAsync()).tool_calls, 1);
  assert.equal(readFileSync(directory + '/effects', 'utf8').trim().split('\\n').length, 1);
  await store.closeAsync();
}
'''.replace('MODULE', json.dumps(MODULE))
    result = subprocess.run(['node', '--input-type=module', '-e', script, directory, mode], cwd=ROOT,
                            capture_output=True, text=True, timeout=15)
    assert result.returncode == (17 if mode == 'crash' else 0), result.stderr
    return json.loads(result.stdout) if mode == 'crash' else None


async def main():
    with tempfile.TemporaryDirectory(prefix='hive-queue-parity-') as directory:
        receipt = node(directory, 'crash')
        store = await SessionStore.open(str(Path(directory) / 'db'), tenant_id='cross')
        try:
            root = await Objective.open({'max_tool_calls': 2}, store=store, objective_id='root')
            queue = await store.open_queue('jobs', policy={'lease_ms': 100})
            job = await queue.get(receipt['id'])
            assert job['status'] == 'running'
            assert await queue.claim('restarted', ['effect']) is None
            now = time.time_ns() // 1_000_000
            await asyncio.sleep(max(0, job['lease_expires_at'] - now + 5) / 1000)
            expired = await queue.maintain()
            assert len(expired) == 1 and expired[0]['status'] == 'uncertain'
            duplicate = await queue.enqueue({'kind': 'effect', 'lane': 'same', 'payload': {'fixture': True},
                                             'objective_id': 'root', 'idempotency_key': 'once'})
            assert duplicate['id'] == job['id'] and duplicate['attempts'] == 1
            assert (await root.inspect_async())['tool_calls'] == 1
            await queue.reconcile(job['id'], receipt['execution_id'], {'type': 'completed', 'result': {'receipt': 'fixture-one'}})
        finally: await store.close_async()
        node(directory, 'verify')
    print('Queue crash/reconciliation parity Node -> Python -> Node passed; fixture effect executed once')


if __name__ == '__main__':
    asyncio.run(main())
