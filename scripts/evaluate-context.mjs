// Deterministic contract fixtures. No remote providers, embeddings or billing.
import assert from 'node:assert/strict';
import { mkdirSync, writeFileSync } from 'node:fs';
import { Agent, Objective } from '../packages/npm/index.mjs';

const tasks = [
  { id: 'old-fact', first: { key: 'R7', value: 'paid' }, recent: { key: 'R8', value: 'queued' }, query: 'R7', expected: 'paid' },
  { id: 'recent-fact', first: { key: 'R7', value: 'paid' }, recent: { key: 'R8', value: 'queued' }, query: 'R8', expected: 'queued' },
  { id: 'updated-fact', first: { key: 'R7', value: 'open' }, recent: { key: 'R7', value: 'paid' }, query: 'R7', expected: 'paid' },
];

function readFacts(messages) {
  const facts = {};
  for (const message of messages) {
    if (message.role !== 'user' || !message.content) continue;
    let value;
    try { value = JSON.parse(message.content); } catch { continue; }
    if (value.fact) facts[value.fact.key] = value.fact.value;
    if (value.context_summary) Object.assign(facts, JSON.parse(value.context_summary.content).facts);
  }
  return facts;
}

const rows = [];
for (const task of tasks) for (const strategy of ['full', 'drop', 'summary', 'retrieval']) {
  const objective = new Objective({ max_model_calls: 16, max_tool_calls: 16, max_output_tokens: 64, max_total_output_tokens: 1024 });
  const policy = { input_token_budget: { encoding: 'cl100k_base', max_tokens: 10000, overhead_tokens: 7 },
    ...(strategy === 'full' ? {} : { max_context_bytes: 600, context_overflow: 'drop_oldest_turns' }) };
  let questionRequest; let summaryCost = null;
  const agent = new Agent({ model: 'fixture-answer', objective, policy, provider: { complete(request) {
    const last = JSON.parse(request.messages.at(-1).content);
    if (last.question) questionRequest = request;
    return { content: last.question ? readFacts(request.messages)[last.question] ?? 'unknown' : 'ack',
      usage: { input_tokens: request.context.token_estimate.total_tokens, output_tokens: 2 } };
  } } });
  let summarizer;
  try {
    const prompts = [JSON.stringify({ fact: task.first }), ...[0, 1, 2].map(i => JSON.stringify({ noise: `${i}:` + 'x'.repeat(220) })), JSON.stringify({ fact: task.recent })];
    for (const prompt of prompts) assert.equal((await agent.run(prompt)).status, 'completed');
    const history = agent.messages;
    if (strategy === 'summary') {
      summarizer = new Agent({ model: 'fixture-summary', maxTurns: 1,
        policy: { input_token_budget: { encoding: 'cl100k_base', max_tokens: 10000, overhead_tokens: 7 } },
        provider: { complete(request) {
          const original = JSON.parse(request.messages.at(-1).content);
          return { content: JSON.stringify({ facts: readFacts(original) }),
            usage: { input_tokens: request.context.token_estimate.total_tokens, output_tokens: 2 } };
        } } });
      const receipt = await agent.summarize('history-summary', agent.captureContext(0, history.length), async (value, context) => {
        const result = await context.delegate(summarizer, JSON.stringify(value.messages));
        assert.equal(result.status, 'completed'); return result.content;
      });
      summaryCost = receipt.cost; agent.setContextPlan({ summary: receipt.summary });
    } else if (strategy === 'retrieval') {
      const found = agent.searchContext(task.query, { maxTurns: 2, maxBytes: 4096 });
      agent.setContextPlan({ retrieved: found.hits.map(hit => hit.source) });
    }
    const result = await agent.run(JSON.stringify({ question: task.query }));
    assert.equal(result.status, 'completed'); assert.deepEqual(agent.messages.slice(0, history.length), history);
    const usage = objective.inspect(); const auxiliaryCalls = summaryCost?.model_calls ?? 0;
    assert.equal(usage.model_calls, prompts.length + 1 + auxiliaryCalls);
    rows.push({ task: task.id, strategy, expected: task.expected, answer: result.content, passed: result.content === task.expected,
      model_calls: usage.model_calls, history_ingestion_model_calls: prompts.length, auxiliary_model_calls: auxiliaryCalls,
      input_tokens_synthetic: usage.input_tokens, output_tokens_synthetic: usage.output_tokens,
      question_input_estimate: questionRequest.context.token_estimate.total_tokens,
      question_json_bytes: questionRequest.context.serialized_bytes, dropped_messages: questionRequest.context.dropped_messages,
      summary_cost: summaryCost });
  } finally { summarizer?.close(); agent.close(); }
}

const aggregate = Object.fromEntries(['full', 'drop', 'summary', 'retrieval'].map(strategy => {
  const selected = rows.filter(row => row.strategy === strategy);
  return [strategy, { tasks: selected.length, correct: selected.filter(row => row.passed).length,
    model_calls: selected.reduce((sum, row) => sum + row.model_calls, 0),
    auxiliary_model_calls: selected.reduce((sum, row) => sum + row.auxiliary_model_calls, 0),
    input_tokens_synthetic: selected.reduce((sum, row) => sum + row.input_tokens_synthetic, 0),
    output_tokens_synthetic: selected.reduce((sum, row) => sum + row.output_tokens_synthetic, 0) }];
}));
assert.equal(aggregate.full.correct, tasks.length);
assert.equal(aggregate.drop.correct, tasks.length - 1);
assert.equal(aggregate.summary.correct, tasks.length);
assert.equal(aggregate.retrieval.correct, tasks.length);
const report = { fixture_version: 1, live_models: false, quality_scope: 'Exact facts extracted by deterministic fixture providers, not remote model quality',
  usage_scope: 'Synthetic reported input from local BPE, output fixed at 2 per reply; includes five history-ingestion calls per task',
  billing: 'No billing or production savings measured', aggregate, tasks: rows };
mkdirSync(new URL('../evals/', import.meta.url), { recursive: true });
writeFileSync(new URL('../evals/context-contract.json', import.meta.url), JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify(aggregate, null, 2));
