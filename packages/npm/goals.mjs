// Drives a goal with the host's agents. The SDK still runs no model on its own:
// `agentFor` supplies the Agent for every job, and this loop only claims jobs, runs
// them through `context.runAgent` (which charges the root objective) and ticks the goal.
const TERMINAL = new Set(['accepted', 'failed', 'cancelled']);
const JSON_STAGES = new Set(['plan', 'review']);

/** The JSON object in a model reply: bare, fenced, or surrounded by prose. */
export function extractJson(text) {
  if (typeof text !== 'string') return null;
  const attempts = [text.trim()];
  const fenced = /```(?:json)?\s*([\s\S]*?)```/i.exec(text);
  if (fenced) attempts.push(fenced[1].trim());
  const first = text.indexOf('{'), last = text.lastIndexOf('}');
  if (first >= 0 && last > first) attempts.push(text.slice(first, last + 1));
  for (const attempt of attempts) {
    try { const value = JSON.parse(attempt); if (value && typeof value === 'object' && !Array.isArray(value)) return value; } catch { /* next */ }
  }
  return null;
}

function executor(agentFor) {
  return async (job, context) => {
    const payload = job.input.payload;
    const binding = payload.delegation ?? payload.goal;
    const stage = binding.stage;
    const agent = await agentFor({ agentId: binding.agent_id, sessionId: binding.session_id, stage, round: binding.round ?? null, taskId: binding.task_id ?? null, job });
    try {
      const run = await context.runAgent(agent, payload.prompt);
      if (run.status !== 'completed' || run.content == null) return { type: 'failed', error: `the agent stopped without an answer (${run.stop_reason ?? 'no reply'})`, retryable: false };
      if (!JSON_STAGES.has(stage)) return { type: 'completed', result: { text: run.content } };
      const parsed = extractJson(run.content);
      return parsed ? { type: 'completed', result: parsed } : { type: 'failed', error: `the ${stage} answer was not a JSON object`, retryable: false };
    } finally { agent.close?.(); }
  };
}

/**
 * Run a goal until it is accepted, failed or cancelled, or until nothing can run:
 * it is waiting on jobs other workers own, or a job's outcome is uncertain (resolve it
 * with `queue.reconcile`; it is never replayed). Returns the last goal report.
 */
export async function runGoal(queue, goalId, { agentFor, signal, maxSteps = 200 } = {}) {
  if (typeof agentFor !== 'function') throw new TypeError('runGoal requires an agentFor function');
  for (let step = 0; step < maxSteps; step++) {
    if (signal?.aborted) return queue.goal(goalId);
    const report = await queue.tickGoal(goalId);
    if (TERMINAL.has(report.status) || report.blocked_uncertain) return report;
    const run = executor(agentFor);
    const executors = { [report.kinds.plan]: run, [report.kinds.review]: run };
    for (const round of report.kinds.rounds) { executors[round.specialist] = run; executors[round.synthesis] = run; }
    const jobs = await queue.runOnce(executors, { signal });
    if (jobs.length === 0) return report;
  }
  return queue.tickGoal(goalId);
}
