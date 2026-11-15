import { SessionStore, Objective } from '../../packages/npm/index.mjs';
const store = await SessionStore.open({ path: process.env.HIVE_DB_PATH });
try {
  await Objective.open({}, { store, id: 'root' });
  const queue = await store.openQueue('groups');
  const group = await queue.createGroup('turn', { objective_id: 'root', coordinator_agent_id: 'lead', tasks: [
    { id: 'a', specialist_id: 'research', prompt: 'a' }, { id: 'b', specialist_id: 'review', prompt: 'b' }] });
  const first = await queue.claim('worker', [group.specialist_kind]);
  await queue.finish(first.id, first.execution_id, { type: 'completed', result: { content: 'fixture' } });
  let waitsForAll = false;
  try { await queue.synthesizeGroup('turn'); } catch (error) { if (!/NOT_READY/.test(error.message)) throw error; waitsForAll = true; }
  const second = await queue.claim('worker', [group.specialist_kind]);
  await queue.finish(second.id, second.execution_id, { type: 'failed', error: 'fixture failure' });
  const synthesis = await queue.synthesizeGroup('turn');
  await queue.synthesizeGroup('turn');
  const report = await queue.group('turn');
  console.log('PARITY_RESULT:' + JSON.stringify({ waitsForAll, summaryCount: (await queue.list()).items.filter(job => job.input.kind === group.synthesis_kind).length,
    outcomeCount: report.tasks.length, failurePreserved: synthesis.input.payload.prompt.includes('fixture failure'),
    uncheckedVisible: synthesis.input.payload.prompt.includes('unchecked') }));
} finally { await store.closeAsync(); }
