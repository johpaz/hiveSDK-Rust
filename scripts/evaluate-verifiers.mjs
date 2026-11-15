// Contract evaluation with deterministic fixtures. No API keys or live models.
import assert from 'node:assert/strict';
import { mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { Agent, Objective } from '../packages/npm/index.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const { values } = parseArgs({ options: { output: { type: 'string', default: 'evals/verifier-contract.json' } } });
const fixtures = [
  { id: 'correct', expected: true, receipt: { state: 'committed', sum: 5 } },
  { id: 'wrong-sum', expected: false, receipt: { state: 'committed', sum: 7 } },
  { id: 'missing', expected: false, receipt: null },
];
const results = [];
for (const fixture of fixtures) {
  for (const mode of ['programmatic', 'fixture-model-review']) {
    const objective = new Objective({ max_model_calls: 2, max_output_tokens: 64, max_total_output_tokens: 128 });
    const worker = new Agent({ model: 'deterministic-worker', objective, provider: {
      complete: () => ({ content: 'Operation finished', usage: { input_tokens: 12, output_tokens: 8 } }),
    } });
    const reviewer = new Agent({ model: 'deterministic-reviewer', provider: {
      // Deliberately trusts prose rather than the receipt, exposing a false-positive fixture.
      complete: () => ({ content: '{"passed":true}', usage: { input_tokens: 32, output_tokens: 16 } }),
    } });
    try {
      await worker.run('Execute the fixture operation');
      const input = { receipt: fixture.receipt, expected_sum: 5 };
      const verdict = mode === 'programmatic'
        ? await objective.verify('receipt-v1', input, facts => ({
          passed: facts.receipt?.state === 'committed' && facts.receipt.sum === facts.expected_sum,
          reason: 'Compare the persisted receipt with the expected sum', evidence: facts.receipt,
        }))
        : await objective.verify('receipt-v1', input, async (facts, context) => {
          const result = await context.delegate(reviewer, JSON.stringify(facts));
          return JSON.parse(result.content);
        }, { modelReview: { evaluationId: 'deterministic-verifier-contract-v1',
          justification: 'Exercise shared accounting in fixtures; this does not justify a production model review' } });
      const stats = objective.inspect();
      assert.equal(stats.model_calls, mode === 'programmatic' ? 1 : 2);
      assert.equal(stats.tool_calls, 1);
      assert.deepEqual(stats.reservations, {});
      results.push({ fixture: fixture.id, mode, expected_passed: fixture.expected, passed: verdict.verdict.passed,
        matches_expected: verdict.verdict.passed === fixture.expected,
        model_calls: stats.model_calls, input_tokens: stats.input_tokens, output_tokens: stats.output_tokens });
    } finally { worker.close(); reviewer.close(); }
  }
}
const summary = Object.fromEntries(['programmatic', 'fixture-model-review'].map(mode => {
  const rows = results.filter(row => row.mode === mode);
  return [mode, { cases: rows.length, matches_expected: rows.filter(row => row.matches_expected).length,
    model_calls: rows.reduce((sum, row) => sum + row.model_calls, 0),
    input_tokens: rows.reduce((sum, row) => sum + row.input_tokens, 0), output_tokens: rows.reduce((sum, row) => sum + row.output_tokens, 0) }];
}));
const report = { version: 1, evaluation_id: 'deterministic-verifier-contract-v1', live_models: false,
  usage_source: 'deterministic fixture reports; not billed vendor tokens',
  scope: 'Budget/callback contracts and known fixture facts; not production model quality or savings',
  results, summary, production_model_review_justified: false };
assert.equal(summary.programmatic.matches_expected, 3);
assert.equal(summary['fixture-model-review'].matches_expected, 1);
const destination = path.resolve(root, values.output);
mkdirSync(path.dirname(destination), { recursive: true });
writeFileSync(destination, `${JSON.stringify(report, null, 2)}\n`);
console.log(`Verifier contract evaluation: ${destination}`);
console.log(JSON.stringify(summary));
