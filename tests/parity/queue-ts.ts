// Selected job-store outcomes only, on an isolated reference database.
import { pathToFileURL } from 'node:url';
const base = process.argv[2];
const jobs = await import(pathToFileURL(`${base}/packages/core/src/gateway/job-store.ts`).href);
const { getHiveDb, closeHiveDb } = await import(pathToFileURL(`${base}/packages/core/src/storage/hivedb.ts`).href);
const heartbeat = setInterval(() => {}, 20);
try {
  const collection = (await getHiveDb()).collection('jobQueue');
  await collection.createIndex('idempotency_key');
  const input = { lane: 'a', type: 'fixture', payload: { text: 'Hello' }, run_id: 'fixture', idempotency_key: 'once', max_attempts: 1 };
  const first = await jobs.createJob(input); const duplicate = await jobs.createJob(input);
  const active = await jobs.claimJob(first.id, 'worker');
  const duplicateClaim = await jobs.claimJob(first.id, 'other');
  await jobs.completeJob(first.id, { receipt: 'one' }, 'worker');
  await jobs.completeJob(first.id, { receipt: 'late' }, 'other');
  const done = await jobs.getJob(first.id);
  const pending = await jobs.createJob({ ...input, idempotency_key: 'cancel', lane: 'b' });
  await jobs.cancelJob(pending.id);
  const cancelled = await jobs.getJob(pending.id);
  console.log('PARITY_RESULT:' + JSON.stringify({ deduplicated: first.id === duplicate.id, attempts: active.attempts,
    duplicateClaim: duplicateClaim === null, status: done.status, receipt: JSON.parse(done.result_json).receipt, cancelled: cancelled.status }));
} finally { closeHiveDb(); clearInterval(heartbeat); }
// Native runtime shutdown is outside this semantic comparison.
process.exit(0);
