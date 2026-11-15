"""Durable specialist group crash -> reconcile/proof -> one synthesis across bindings.

Only fixture receipts in a temporary database; no model or account calls.
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
POLICY = {'max_tool_calls': 8}
SPEC = {'objective_id': 'root', 'coordinator_agent_id': 'lead', 'require_verification': True,
        'tasks': [{'id': 'a', 'specialist_id': 'research', 'prompt': 'Find fixture evidence'}]}


def node(directory, mode):
    script = '''
import assert from 'node:assert/strict';
import { SessionStore, Objective } from MODULE;
const directory = process.argv[1]; const mode = process.argv[2];
const store = await SessionStore.open({ path: directory + '/db', tenantId: 'cross' });
const root = await Objective.open(POLICY, { store, id: 'root' });
const queue = await store.openQueue('groups', { lease_ms: 100 });
const group = await queue.createGroup('turn', SPEC);
if (mode === 'crash') {
  const job = await queue.claim('interrupted', [group.specialist_kind]);
  process.stdout.write(JSON.stringify(job), () => process.exit(17));
} else {
  assert.equal(group.tasks[0].job.attempts, 1);
  assert.equal(group.tasks[0].proof.met, true);
  assert.equal(group.status, 'completed');
  const synthesis = await queue.synthesizeGroup('turn');
  assert.equal(synthesis.id, group.synthesis.id);
  assert.equal(synthesis.result.receipt, 'one synthesis');
  assert.equal((await root.inspectAsync()).tool_calls, 3);
  assert.equal((await queue.inspect()).retained_jobs, 2);
  await store.closeAsync();
}
'''.replace('MODULE', json.dumps(MODULE)).replace('POLICY', json.dumps(POLICY)).replace('SPEC', json.dumps(SPEC))
    result = subprocess.run(['node', '--input-type=module', '-e', script, directory, mode], cwd=ROOT,
                            capture_output=True, text=True, timeout=15)
    assert result.returncode == (17 if mode == 'crash' else 0), result.stderr
    return json.loads(result.stdout) if mode == 'crash' else None


async def main():
    with tempfile.TemporaryDirectory(prefix='hive-delegation-parity-') as directory:
        job = node(directory, 'crash')
        store = await SessionStore.open(str(Path(directory) / 'db'), tenant_id='cross')
        try:
            root = await Objective.open(POLICY, store=store, objective_id='root')
            queue = await store.open_queue('groups', policy={'lease_ms': 100})
            await queue.create_group('turn', SPEC)
            assert (await queue.inspect())['retained_jobs'] == 1
            await asyncio.sleep(max(0, job['lease_expires_at'] - time.time_ns() // 1_000_000 + 5) / 1000)
            await queue.maintain()
            assert (await queue.group('turn'))['status'] == 'blocked_uncertain'
            try: await queue.synthesize_group('turn')
            except RuntimeError as error: assert 'NOT_READY' in str(error)
            else: raise AssertionError('uncertain child allowed synthesis')
            await queue.reconcile(job['id'], job['execution_id'], {'type': 'completed', 'result': {'receipt': 'external fixture receipt'}})
            value = await queue.group_verification_input('turn', 'a')
            async def check(_value, _context): return {'passed': True, 'evidence': ['external fixture receipt']}
            await root.verify('acceptance', value, check)
            await queue.record_group_proof('turn', 'a', 'acceptance')
            first, second = await asyncio.gather(queue.synthesize_group('turn'), queue.synthesize_group('turn'))
            assert first['id'] == second['id']
            group = await queue.group('turn')
            async def synthesize(job, context):
                assert context.depth == 0 and context.objective_id == 'root'
                assert 'external fixture receipt' in job['input']['payload']['prompt']
                return {'type': 'completed', 'result': {'receipt': 'one synthesis'}}
            assert len(await queue.run_once({group['synthesis_kind']: synthesize})) == 1
            assert await queue.run_once({group['synthesis_kind']: synthesize}) == []
        finally: await store.close_async()
        node(directory, 'verify')
    print('Delegation crash/reconciliation/proof/fan-in Node -> Python -> Node passed')


if __name__ == '__main__':
    asyncio.run(main())
