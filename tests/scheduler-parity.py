"""Node crashes during one occurrence, Python reconciles and runs the next, Node verifies."""
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
const directory = process.argv[1], mode = process.argv[2];
const store = await SessionStore.open({ path: directory + '/db', tenantId: 'cross' });
const root = await Objective.open({ max_tool_calls: 2 }, { store, id: 'root' });
const queue = await store.openQueue('scheduler', { lease_ms: 100 });
const spec = { schedule: { kind: 'interval', every_ms: 1000, anchor_unix_ms: 0 }, max_runs: 2, misfire_policy: 'fire_once',
  job: { kind: 'fixture', lane: 'effects', payload: {}, objective_id: 'root' } };
await queue.createSchedule('repeat', spec);
if (mode === 'crash') {
  await queue.enableSchedule('repeat');
  await queue.tickSchedule('repeat', 0);
  const job = await queue.claim('interrupted', ['fixture']);
  appendFileSync(directory + '/effects', 'first\\n');
  process.stdout.write(JSON.stringify(job), () => process.exit(17));
} else {
  const state = await queue.schedule('repeat');
  assert.equal(state.published, 2); assert.equal(state.status, 'completed');
  assert.equal((await queue.tickSchedule('repeat', 1000)).outcome, 'idle');
  assert.equal((await queue.list()).items.length, 2);
  assert.equal((await root.inspectAsync()).tool_calls, 2);
  assert.equal((await root.inspectAsync()).model_calls, 0);
  assert.deepEqual(readFileSync(directory + '/effects', 'utf8').trim().split('\\n'), ['first', 'second']);
  await store.closeAsync();
}
'''.replace('MODULE', json.dumps(MODULE))
    result = subprocess.run(['node', '--input-type=module', '-e', script, directory, mode], cwd=ROOT, capture_output=True, text=True, timeout=15)
    assert result.returncode == (17 if mode == 'crash' else 0), result.stderr
    return json.loads(result.stdout) if mode == 'crash' else None


async def main():
    with tempfile.TemporaryDirectory(prefix='hive-scheduler-parity-') as directory:
        job = node(directory, 'crash')
        store = await SessionStore.open(str(Path(directory) / 'db'), tenant_id='cross')
        try:
            root = await Objective.open({'max_tool_calls': 2}, store=store, objective_id='root')
            queue = await store.open_queue('scheduler', policy={'lease_ms': 100})
            await asyncio.sleep(max(0, job['lease_expires_at'] - time.time_ns() // 1_000_000 + 5) / 1000)
            await queue.maintain()
            assert (await queue.tick_schedule('repeat', 1000))['outcome'] == 'blocked'
            assert (await queue.inspect())['retained_jobs'] == 1
            assert (await root.inspect_async())['tool_calls'] == 1
            await queue.reconcile(job['id'], job['execution_id'], {'type': 'completed', 'result': {'receipt': 'first'}})
            ticks = await asyncio.gather(queue.tick_schedule('repeat', 1000), queue.tick_schedule('repeat', 1000))
            assert sum(tick['outcome'] == 'published' for tick in ticks) == 1
            async def execute(job, context):
                assert job['input']['payload']['schedule']['scheduled_for'] == 1000
                with Path(directory, 'effects').open('a') as effects: effects.write('second\n')
                return {'type': 'completed', 'result': {'receipt': 'second'}}
            assert len(await queue.run_once({'fixture': execute})) == 1
        finally: await store.close_async()
        node(directory, 'verify')
    print('Scheduler crash/block/reconciliation/next-occurrence Node -> Python -> Node passed')


if __name__ == '__main__': asyncio.run(main())
