import { SessionStore } from '../../packages/npm/index.mjs';
const store = await SessionStore.open({ path: process.env.HIVE_DB_PATH });
try {
  const queue = await store.openQueue('reference');
  const input = { kind: 'fixture', lane: 'a', payload: { text: 'Hello' }, idempotency_key: 'once', max_attempts: 1 };
  const first = await queue.enqueue(input); const duplicate = await queue.enqueue(input);
  const active = await queue.claim('worker', ['fixture']);
  const duplicateClaim = await queue.claim('other', ['fixture']);
  await queue.finish(first.id, active.execution_id, { type: 'completed', result: { receipt: 'one' } });
  try { await queue.finish(first.id, active.execution_id, { type: 'completed', result: { receipt: 'late' } }); } catch {}
  const done = await queue.get(first.id);
  const pending = await queue.enqueue({ ...input, idempotency_key: 'cancel', lane: 'b' });
  const cancelled = await queue.cancel(pending.id);
  console.log('PARITY_RESULT:' + JSON.stringify({ deduplicated: first.id === duplicate.id, attempts: active.attempts,
    duplicateClaim: duplicateClaim === null, status: done.status, receipt: done.result.receipt, cancelled: cancelled.status }));
} finally { await store.closeAsync(); }
