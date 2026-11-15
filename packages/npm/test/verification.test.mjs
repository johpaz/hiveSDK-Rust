import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { Agent, Objective, ObjectiveLimit, SessionStore, VerificationUncertain } from '../index.mjs';

const usage = { input_tokens: 2, output_tokens: 3 };
const reviewEvidence = { evaluationId: 'fixture-contract-v1', justification: 'Validate the budget contract, not production model quality' };
function setup(durable = false, policy = {}) {
  const dir = mkdtempSync(path.join(tmpdir(), 'hive-verification-'));
  const store = durable ? new SessionStore({ path: dir }) : undefined;
  const objective = new Objective({ max_output_tokens: 10, max_total_output_tokens: 15, ...policy }, store ? { store, id: 'root' } : undefined);
  return { objective, store, dir, close() { store?.close(); rmSync(dir, { recursive: true, force: true }); } };
}

test('verification is explicit, returns a factual verdict and caches even failed checks', async () => {
  for (const durable of [false, true]) {
    const fixture = setup(durable); const { objective } = fixture;
    try {
      let calls = 0; let checks = 0; let captured;
      const agent = new Agent({ model: 'test', objective, provider: { complete() { calls++; return { content: 'done', usage }; } } });
      await agent.run('Work'); agent.close();
      assert.equal(calls, 1); assert.deepEqual(objective.inspect().verifications, {});
      const input = { receipt: 'missing', expected: 5 };
      const receipt = await objective.verify('receipt-v1', input, (value, context) => {
        checks++; captured = context; value.expected = 999;
        assert.equal(typeof context.execution_id, 'string');
        assert.deepEqual(Object.keys(context).sort(), ['execution_id', 'objective_id', 'verification_key']);
        return { passed: false, reason: 'Receipt is absent', evidence: { receipt: null } };
      });
      const cached = await objective.verify('receipt-v1', input, () => assert.fail('must not rerun'));
      assert.deepEqual(cached, receipt); assert.equal(checks, 1); assert.equal(calls, 1);
      assert.equal(input.expected, 5); assert.equal(objective.inspect().tool_calls, 1);
      assert.throws(() => captured.delegate(agent, 'Late'), /CLOSED/);
      await assert.rejects(objective.verify('receipt-v1', { ...input, expected: 6 }, () => ({ passed: true })), /already bound/);
    } finally { fixture.close(); }
  }
});

test('invalid inputs and review options fail before callback or budget consumption', async () => {
  const fixture = setup(); const { objective } = fixture;
  try {
    let calls = 0; const callback = () => { calls++; return { passed: true }; };
    for (const [key, value, options] of [['', 1, {}], ['large', 'x'.repeat(65537), {}], ['review', 1, { modelReview: { evaluationId: '', justification: 'why' } }]]) {
      await assert.rejects(objective.verify(key, value, callback, options));
    }
    assert.equal(calls, 0); assert.equal(objective.inspect().tool_calls, 0);
  } finally { fixture.close(); }
});

test('malformed verdict remains uncertain until explicit fenced resolution', async () => {
  const fixture = setup(true); const { objective } = fixture;
  try {
    await assert.rejects(objective.verify('check', { n: 1 }, () => ({ passed: 'true' })), VerificationUncertain);
    await assert.rejects(objective.verify('check', { n: 1 }, () => assert.fail('no replay')), VerificationUncertain);
    const executionId = objective.inspect().verifications.check.execution_id;
    assert.throws(() => objective.resolveVerification({ key: 'check', executionId: 'stale', verdict: { passed: true } }), /SESSION_CONFLICT/);
    const result = objective.resolveVerification({ key: 'check', executionId, verdict: { passed: true, evidence: { receipt: 'verified' } } });
    assert.throws(() => objective.resolveVerification({ key: 'check', executionId, verdict: { passed: false } }), /SESSION_CONFLICT/);
    assert.deepEqual(await objective.verify('check', { n: 1 }, () => assert.fail('no replay')), result);
    assert.equal(objective.inspect().tool_calls, 1);
  } finally { fixture.close(); }
});

test('reviewer requires evaluation opt-in and shares the remaining model allowance', async () => {
  for (const allowed of [false, true]) {
    const fixture = setup(false, { max_model_calls: 1 }); const { objective } = fixture;
    let calls = 0;
    const reviewer = new Agent({ model: 'reviewer', provider: { complete(request) {
      calls++; assert.equal(request.max_output_tokens, 10); return { content: '{"passed":true,"reason":"fixture"}', usage };
    } } });
    try {
      const callback = async (value, context) => JSON.parse((await context.delegate(reviewer, 'Check the receipt')).content);
      if (!allowed) await assert.rejects(objective.verify('check', { n: 1 }, callback), VerificationUncertain);
      else {
        const result = await objective.verify('check', { n: 1 }, callback, { modelReview: reviewEvidence });
        assert.equal(result.verdict.passed, true);
        await assert.rejects(objective.verify('another', { n: 1 }, callback, { modelReview: reviewEvidence }), ObjectiveLimit);
        assert.equal(objective.inspect().verifications.check.model_review.evaluation_id, reviewEvidence.evaluationId);
        assert.equal(objective.inspect().output_tokens, 3);
      }
      assert.equal(calls, allowed ? 1 : 0);
    } finally { reviewer.close(); fixture.close(); }
  }
});

test('unawaited reviewers are joined and a failing callback cancels its child', async () => {
  const fixture = setup(true); const { objective, store } = fixture;
  let enter; const entered = new Promise(resolve => { enter = resolve; }); let aborted = false;
  const reviewer = new Agent({ model: 'reviewer', provider: { async complete(_, { signal }) {
    enter(); await new Promise((_, reject) => signal.addEventListener('abort', () => { aborted = true; reject(new Error('aborted')); }, { once: true }));
  } } });
  try {
    await assert.rejects(objective.verify('check', {}, async (_, context) => {
      context.delegate(reviewer, 'Work'); await entered;
      assert.throws(() => store.close(), /BUSY/);
      throw new Error('callback failed');
    }, { modelReview: reviewEvidence }), VerificationUncertain);
    assert.equal(aborted, true); assert.equal(objective.inspect().model_calls, 1);
    assert.deepEqual(objective.inspect().reservations, { 1: 10 });
  } finally { reviewer.close(); fixture.close(); }
});

test('a known late verdict is committed before reporting cancellation', async () => {
  const fixture = setup(true); const { objective } = fixture;
  let enter; const entered = new Promise(resolve => { enter = resolve; });
  let release; const gate = new Promise(resolve => { release = resolve; });
  const controller = new AbortController();
  try {
    const pending = objective.verify('check', { n: 1 }, async () => { enter(); await gate; return { passed: true }; }, { signal: controller.signal });
    await entered;
    assert.throws(() => objective.resolveVerification({ key: 'check', executionId: 'x', verdict: { passed: false } }), /BUSY/);
    controller.abort(); release(); await assert.rejects(pending, /CANCELLED/);
    assert.equal(objective.inspect().verifications.check.verdict.passed, true);
    assert.equal((await objective.verify('check', { n: 1 }, () => assert.fail('no replay'))).verdict.passed, true);
    assert.equal(objective.inspect().tool_calls, 1);
  } finally { fixture.close(); }
});

test('same pending key cannot dispatch twice while the verifier is active', async () => {
  const fixture = setup(); const { objective } = fixture;
  let release; const gate = new Promise(resolve => { release = resolve; }); let checks = 0;
  try {
    const pending = objective.verify('check', {}, async () => { checks++; await gate; return { passed: true }; });
    await assert.rejects(objective.verify('check', {}, () => assert.fail('no replay')), VerificationUncertain);
    release(); await pending;
    assert.equal(checks, 1); assert.equal(objective.inspect().tool_calls, 1);
  } finally { fixture.close(); }
});

test('actual verifier process exit requires reconciliation and never repeats its effect', async () => {
  const fixture = setup(true); const { dir } = fixture; fixture.store.close(); let store;
  try {
    const module = new URL('../index.mjs', import.meta.url).href;
    const script = `import {writeFileSync} from 'node:fs';import {join} from 'node:path';
      import {Objective,SessionStore} from ${JSON.stringify(module)};
      const store=new SessionStore({path:process.argv[1]});
      const objective=new Objective({max_output_tokens:10,max_total_output_tokens:15},{store,id:'root'});
      await objective.verify('receipt-v1',{receipt:'r1'},()=>{writeFileSync(join(process.argv[1],'receipt'),'confirmed');process.exit(19)});`;
    const processResult = spawnSync(process.execPath, ['--input-type=module', '-e', script, dir], { encoding: 'utf8', timeout: 30_000 });
    assert.equal(processResult.status, 19, processResult.stderr);
    assert.equal(readFileSync(path.join(dir, 'receipt'), 'utf8'), 'confirmed');
    store = new SessionStore({ path: dir }); const objective = new Objective({ max_output_tokens: 10, max_total_output_tokens: 15 }, { store, id: 'root' });
    await assert.rejects(objective.verify('receipt-v1', { receipt: 'r1' }, () => assert.fail('no replay')), VerificationUncertain);
    const executionId = objective.inspect().verifications['receipt-v1'].execution_id;
    objective.resolveVerification({ key: 'receipt-v1', executionId, verdict: { passed: true, evidence: { receipt: 'r1' } } });
    assert.equal((await objective.verify('receipt-v1', { receipt: 'r1' }, () => assert.fail('no replay'))).verdict.passed, true);
    assert.equal(objective.inspect().tool_calls, 1); assert.equal(objective.inspect().model_calls, 0);
  } finally { store?.close(); fixture.close(); }
});

test('reviewers cannot bypass the root depth limit', async () => {
  const fixture = setup(false, { max_depth: 0 }); const { objective } = fixture; let calls = 0;
  const reviewer = new Agent({ model: 'reviewer', provider: { complete() { calls++; return { content: '{}', usage }; } } });
  try {
    await assert.rejects(objective.verify('check', {}, async (_, context) => {
      await context.delegate(reviewer, 'Work'); return { passed: true };
    }, { modelReview: reviewEvidence }), ObjectiveLimit);
    assert.equal(calls, 0); assert.equal(objective.inspect().model_calls, 0);
  } finally { reviewer.close(); fixture.close(); }
});

test('a successful callback also waits for every reviewer it starts', async () => {
  const fixture = setup(); const { objective } = fixture;
  let enter; const entered = new Promise(resolve => { enter = resolve; });
  let release; const gate = new Promise(resolve => { release = resolve; }); let settled = false;
  const reviewer = new Agent({ model: 'reviewer', provider: { async complete() { enter(); await gate; return { content: 'done', usage }; } } });
  try {
    const pending = objective.verify('check', {}, (_, context) => {
      context.delegate(reviewer, 'Work'); return { passed: true };
    }, { modelReview: reviewEvidence }).then(result => { settled = true; return result; });
    await entered; assert.equal(settled, false); release();
    assert.equal((await pending).verdict.passed, true); assert.equal(objective.inspect().model_calls, 1);
    assert.deepEqual(objective.inspect().reservations, {});
  } finally { reviewer.close(); fixture.close(); }
});
