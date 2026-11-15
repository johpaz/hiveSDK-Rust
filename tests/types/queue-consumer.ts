import type { SessionStore, Agent, JobExecutor, Job } from '../../packages/npm/index';

// Public API composition: a queued agent result can be persisted directly.
export async function runAgentJob(store: SessionStore, agent: Agent): Promise<Job[]> {
  const queue = await store.openQueue('typed');
  await queue.enqueue({ kind: 'agent', lane: 'a', payload: { prompt: 'Run' }, objective_id: 'root' });
  const executor: JobExecutor = async (_job, context) => ({
    type: 'completed', result: await context.runAgent(agent, 'Run'),
  });
  return queue.runOnce({ agent: executor });
}

export async function verifiedGroup(store: SessionStore) {
  const queue = await store.openQueue('typed-groups');
  const group = await queue.createGroup('turn', { objective_id: 'root', coordinator_agent_id: 'lead',
    require_verification: true, tasks: [{ id: 'a', specialist_id: 'research', prompt: 'Find evidence' }] });
  const input = await queue.groupVerificationInput(group.id, 'a');
  const proof = await queue.recordGroupProof(group.id, 'a', 'durable-check');
  return { input, met: proof.met, synthesis: await queue.synthesizeGroup(group.id) };
}

export async function scheduledJob(store: SessionStore) {
  const queue = await store.openQueue('typed-schedules');
  const entry = await queue.createSchedule('daily', {
    schedule: { kind: 'cron', expression: '0 8 * * MON-FRI', timezone: 'America/Bogota' },
    job: { kind: 'fixture', lane: 'daily', payload: { prompt: 'Run' }, objective_id: 'root' }, max_runs: 10,
  });
  await queue.enableSchedule(entry.id);
  const tick = await queue.tickSchedule(entry.id);
  return { tick, next: await queue.nextOccurrence(entry.spec.schedule, Date.now()) };
}
