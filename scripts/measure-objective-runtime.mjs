// Concurrent local durable workload. Reported model usage is synthetic, never billed.
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, Objective, SessionStore } from '../packages/npm/index.mjs';
const usage = { input_tokens: 3, output_tokens: 2 };
const percentile = (values, q) => values.length ? values[Math.ceil(q * values.length) - 1] : null;
async function measure(concurrency, historyChars) {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-objective-latency-')); const store = await SessionStore.open({ path: directory });
  let timer;
  try {
    const objective = await Objective.open({ max_model_calls: 128, max_tool_calls: 64, max_output_tokens: 8, max_total_output_tokens: 256 }, { store, id: 'root' });
    const agents = []; const inputs = [];
    for (let i = 0; i < concurrency; i++) {
      const agent = await Agent.open({ model: 'fixture', store, objective, agentId: `a${i}`, sessionId: 's',
        policy: { max_context_bytes: 16384, context_overflow: 'drop_oldest_turns' },
        tools: [{ name: 'read', description: 'Read', parameters: { type: 'object' }, execute: args => ({ known: args.task }) }],
        provider: { complete(request) {
          const last = request.messages.at(-1);
          if (last.role === 'user' && last.content.startsWith('Task')) return { tool_calls: [{ id: 'c', name: 'read', arguments: { task: last.content } }], usage };
          return { content: last.role === 'tool' ? 'Known' : 'x'.repeat(historyChars), usage };
        } } });
      for (let j = 0; j < 4; j++) await agent.run(`History ${j}: ${'h'.repeat(historyChars)}`);
      const source = agent.captureContext(0, 2); inputs.push({ source, messages: agent.readContext(source) }); agents.push(agent);
    }
    const events = [];
    for (let i = 0; i < concurrency; i++) {
      const event = await objective.triggerAsync(`events${i}`, { kind: 'event' }, value => value); await event.enableAsync(); events.push(event);
    }
    const before = await objective.inspectAsync(); const delays = []; let previous = performance.now(); const start = previous;
    timer = setInterval(() => { const now = performance.now(); delays.push(Math.max(0, now - previous - 10)); previous = now; }, 10);
    const outcomes = await Promise.allSettled(agents.flatMap((agent, i) => [
      (async () => { for (let j = 0; j < 4; j++) await agent.run(`Task ${i}/${j}`); })(),
      (async () => { for (let j = 0; j < 2; j++) await objective.verify(`v${i}/${j}`, { known: true }, () => ({ passed: true })); })(),
      (async () => { for (let j = 0; j < 2; j++) await events[i].emit(`e${i}/${j}`, { known: true }); })(),
      objective.summarize(`s${i}`, inputs[i], () => 'Known'),
    ]));
    const failure = outcomes.find(outcome => outcome.status === 'rejected'); if (failure) throw failure.reason;
    const after = await objective.inspectAsync(); const elapsed = performance.now() - start; clearInterval(timer); delays.sort((a, b) => a - b);
    const measured = { model_calls: after.model_calls - before.model_calls, tool_calls: after.tool_calls - before.tool_calls,
      input_tokens: after.input_tokens - before.input_tokens, output_tokens: after.output_tokens - before.output_tokens };
    assert.equal(measured.model_calls, concurrency * 8); assert.equal(measured.tool_calls, concurrency * 9); assert.deepEqual(after.reservations, {});
    return { concurrency, history_turns_per_agent: 4, history_chars_per_message: historyChars, tasks_per_agent: 4,
      context_max_bytes: 16384, interval_ms: 10, elapsed_ms: elapsed, samples: delays.length,
      observed_lag_ms: { p50: percentile(delays, .5), p95: percentile(delays, .95), max: delays.at(-1) ?? null },
      warmup_model_calls: before.model_calls, measured_synthetic_usage: measured, total_model_calls: after.model_calls };
  } finally { clearInterval(timer); await store.closeAsync(); rmSync(directory, { recursive: true, force: true }); }
}
const cases = [await measure(1, 1024), await measure(4, 4096)];
mkdirSync(new URL('../evals/', import.meta.url), { recursive: true });
writeFileSync(new URL('../evals/objective-runtime-node.json', import.meta.url), `${JSON.stringify({ runtime: process.version,
  scope: 'Observed local event-loop lag with actual HiveDB and concurrent SDK control; synthetic callbacks/usage, no remote models. Not a production latency bound or billing estimate. Timings vary by machine/load.', cases }, null, 2)}\n`);
console.log(JSON.stringify(cases));
