// Selected sealed-group fan-in behavior; isolated reference DB, no worker/model.
import { pathToFileURL } from 'node:url';
const base = process.argv[2];
const groups = await import(pathToFileURL(`${base}/packages/core/src/gateway/delegation-groups.ts`).href);
const { closeHiveDb } = await import(pathToFileURL(`${base}/packages/core/src/storage/hivedb.ts`).href);
const heartbeat = setInterval(() => {}, 20);
try {
  let count = 0; let prompt = '';
  groups.setDelegationSummaryEnqueuer(async (_group, content, _key) => { count++; prompt = content; return { id: 'summary' }; });
  for (const taskId of ['a', 'b']) await groups.registerDelegatedTask({ turnId: 'turn', taskId, threadId: 'thread' });
  await groups.sealDelegationGroup('turn');
  await groups.recordDelegationOutcome({ turnId: 'turn', taskId: 'a', jobId: 'a', workerId: 'research', taskName: 'a', ok: true, result: { content: 'fixture' } });
  const waitsForAll = count === 0;
  const failure = { turnId: 'turn', taskId: 'b', jobId: 'b', workerId: 'review', taskName: 'b', ok: false, error: 'fixture failure' };
  await groups.recordDelegationOutcome(failure);
  await groups.recordDelegationOutcome(failure);
  await groups.sealDelegationGroup('turn');
  const group = await groups.getDelegationGroup('turn');
  const outcomes = JSON.parse(group!.outcomes_json);
  console.log('PARITY_RESULT:' + JSON.stringify({ waitsForAll, summaryCount: count, outcomeCount: outcomes.length,
    failurePreserved: prompt.includes('fixture failure'), uncheckedVisible: prompt.includes('unchecked') }));
} finally { groups.setDelegationSummaryEnqueuer(null); closeHiveDb(); clearInterval(heartbeat); }
process.exit(0);
