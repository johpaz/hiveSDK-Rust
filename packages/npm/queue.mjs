import { randomUUID } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';

/** Queue decisions, leases, fencing, retries and accounting execute in Rust. */
export class DurableQueue {
  #store;
  #id;
  #policy;
  constructor(store, id, policy) { this.#store = store; this.#id = id; this.#policy = policy; }
  static async open({ store, id = 'default', policy = {} }) {
    const value = await store._dispatchAsync({ op: 'queue', id, command: { op: 'open', policy } });
    return new DurableQueue(store, id, value.policy);
  }
  get id() { return this.#id; }
  get policy() { return structuredClone(this.#policy); }
  #call(op, fields = {}) { return this.#store._dispatchAsync({ op: 'queue', id: this.#id, command: { ...fields, op } }); }
  inspect() { return this.#call('inspect'); }
  enqueue(input) { return this.#call('enqueue', { input }); }
  get(jobId) { return this.#call('get', { job_id: jobId }); }
  list({ limit = 50, cursor = null } = {}) { return this.#call('list', { limit, cursor }); }
  claim(workerId, kinds) { return this.#call('claim', { worker_id: workerId, kinds }); }
  heartbeat(jobId, executionId) { return this.#call('heartbeat', { job_id: jobId, execution_id: executionId }); }
  finish(jobId, executionId, outcome) { return this.#call('finish', { job_id: jobId, execution_id: executionId, outcome }); }
  cancel(jobId) { return this.#call('cancel', { job_id: jobId }); }
  cancelExecution(jobId, executionId) { return this.#call('cancel_execution', { job_id: jobId, execution_id: executionId }); }
  maintain() { return this.#call('maintain'); }
  reconcile(jobId, executionId, resolution) { return this.#call('reconcile', { job_id: jobId, execution_id: executionId, resolution }); }

  createGroup(groupId, spec) { return this.#call('group_create', { group_id: groupId, spec }); }
  group(groupId) { return this.#call('group_get', { group_id: groupId }); }
  synthesizeGroup(groupId) { return this.#call('group_synthesize', { group_id: groupId }); }
  cancelGroup(groupId) { return this.#call('group_cancel', { group_id: groupId }); }
  groupVerificationInput(groupId, taskId) { return this.#call('group_verification_input', { group_id: groupId, task_id: taskId }); }
  recordGroupProof(groupId, taskId, verificationKey) { return this.#call('group_record_proof', { group_id: groupId, task_id: taskId, verification_key: verificationKey }); }

  async createSchedule(scheduleId, spec) {
    JSON.stringify(spec, (_key, value) => {
      if (typeof value === 'number' && !Number.isFinite(value)) throw new TypeError('Schedule numbers must be finite');
      return value;
    });
    return this.#call('schedule_create', { schedule_id: scheduleId, spec });
  }
  schedule(scheduleId) { return this.#call('schedule_get', { schedule_id: scheduleId }); }
  enableSchedule(scheduleId, enabled = true) { return this.#call('schedule_enable', { schedule_id: scheduleId, enabled }); }
  async tickSchedule(scheduleId, observed = null) {
    if (observed !== null && (!Number.isSafeInteger(observed) || observed < 0)) throw new TypeError('Schedule observation must be a safe nonnegative integer');
    return this.#call('schedule_tick', { schedule_id: scheduleId, observed });
  }
  listSchedules({ limit = 50, cursor = null } = {}) { return this.#call('schedule_list', { limit, cursor }); }
  nextOccurrence(schedule, after) { return this.#call('schedule_next', { schedule, after }); }

  /** Explicit producer loop. Every calendar and publication decision executes in Rust. */
  async runScheduler({ pollMs = 1000, signal } = {}) {
    if (!Number.isSafeInteger(pollMs) || pollMs < 10 || pollMs > 60000) throw new TypeError('Scheduler poll interval must be 10..60000 ms');
    this.#store._retain();
    try {
      while (!signal?.aborted) {
        let cursor = null;
        do {
          if (signal?.aborted) return;
          const page = await this.listSchedules({ limit: 50, cursor });
          for (const entry of page.items) {
            if (signal?.aborted) return;
            await this.tickSchedule(entry.id);
          }
          cursor = page.next_cursor;
        } while (cursor !== null);
        try { await delay(pollMs, undefined, { signal }); }
        catch (error) { if (signal?.aborted) return; throw error; }
      }
    } finally { this.#store._release(); }
  }

  /** Run one bounded batch of host callbacks; no implicit background execution. */
  async runOnce(executors, { workerId = randomUUID(), signal } = {}) {
    const registry = new Map(Object.entries(executors));
    const kinds = [...registry.keys()];
    if (!kinds.length || kinds.length > 256 || [...registry.values()].some(execute => typeof execute !== 'function')) throw new TypeError('Register at least one job executor function');
    this.#store._retain();
    const tasks = []; let claimError;
    try {
      if (signal?.aborted) throw Object.assign(new Error('CANCELLED: worker batch cancelled'), { code: 'CANCELLED' });
      await this.maintain();
      for (let i = 0; i < this.#policy.max_concurrency && !signal?.aborted; i++) {
        let job;
        try { job = await this.claim(workerId, kinds); }
        catch (error) { claimError = error; break; }
        if (!job) break;
        // Attach rejection handling immediately; later claims can still be pending.
        tasks.push(this.#execute(job, registry.get(job.input.kind), signal).then(value => ({ value }), error => ({ error })));
      }
      const results = await Promise.all(tasks);
      const error = claimError ?? results.find(result => result.error)?.error;
      if (error) throw error;
      return results.map(result => result.value).sort((a, b) => a.sequence - b.sequence);
    } finally { this.#store._release(); }
  }
  async #execute(job, execute, externalSignal) {
    const controller = new AbortController();
    const renewalStop = new AbortController();
    let finished = false; let leaseError;
    const abort = () => controller.abort();
    externalSignal?.addEventListener('abort', abort, { once: true });
    if (externalSignal?.aborted) abort();
    const renew = async () => {
      try {
        while (!finished) {
          await delay(Math.max(1, Math.floor(Math.min(this.#policy.lease_ms, this.#policy.task_timeout_ms ?? Infinity) / 3)), undefined, { signal: renewalStop.signal });
          if (finished) break;
          if (externalSignal?.aborted) await this.cancelExecution(job.id, job.execution_id);
          const receipt = await this.heartbeat(job.id, job.execution_id);
          if (receipt.cancellation_requested) controller.abort();
        }
      } catch (error) {
        if (!renewalStop.signal.aborted) { leaseError = error; controller.abort(); }
      }
    };
    const heartbeat = renew();
    const context = Object.freeze({
      job_id: job.id, execution_id: job.execution_id, objective_id: job.input.objective_id,
      depth: job.input.depth, signal: controller.signal,
      runAgent: async (agent, prompt, { resume = false } = {}) => agent._runQueued(prompt, {
        store: this.#store, objectiveId: job.input.objective_id, depth: job.input.depth, signal: controller.signal, resume, binding: job.input.kind.startsWith('hive.specialist.') || job.input.kind.startsWith('hive.synthesis.') ? job.input.payload.delegation : undefined,
      }),
    });
    let outcome;
    try {
      outcome = controller.signal.aborted ? { type: 'cancelled' } : await execute(structuredClone(job), context);
    } catch {
      outcome = { type: 'uncertain', error: 'Executor failed without a confirmed outcome' };
    } finally {
      finished = true; renewalStop.abort(); await heartbeat;
      externalSignal?.removeEventListener('abort', abort);
    }
    if (leaseError) throw leaseError;
    try {
      if (externalSignal?.aborted) await this.cancelExecution(job.id, job.execution_id);
      return await this.finish(job.id, job.execution_id, outcome);
    } catch (error) {
      try { await this.finish(job.id, job.execution_id, { type: 'uncertain', error: 'Executor outcome could not be committed' }); } catch {}
      throw error;
    }
  }
}
