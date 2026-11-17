import test from 'node:test';
import assert from 'node:assert/strict';
import { Agent } from '../index.mjs';

/** A provider that asks for each scripted tool call in turn, then answers with what the tools returned. */
function scripted(calls) {
  let step = 0; const seen = [];
  return { seen, complete(request) {
    const last = request.messages.at(-1);
    if (last.role === 'tool') seen.push(last.content);
    if (step < calls.length) {
      const call = calls[step++];
      return { content: null, tool_calls: [{ id: `c${step}`, name: call.name, arguments: call.arguments ?? {} }], usage: { input_tokens: 1, output_tokens: 1 } };
    }
    return { content: 'done', usage: { input_tokens: 1, output_tokens: 1 } };
  } };
}
const tool = (name, log) => ({ name, description: name, parameters: { type: 'object' }, execute: args => { log.push([name, args]); return { ok: name, args }; } });

test('a policy denies, allows and asks; denied tools never run and the model is told why', async () => {
  const ran = [];
  const provider = scripted([{ name: 'fs_write', arguments: { path: '/etc/passwd' } }, { name: 'fs_write', arguments: { path: 'reports/a.md' } }, { name: 'danger' }]);
  const agent = new Agent({ model: 'm', provider, tools: [tool('fs_write', ran), tool('danger', ran)], maxTurns: 6,
    toolPolicy: { default: 'allow', rules: [
      { tools: ['fs_write'], decision: 'allow', when: [{ arg: '/path', op: 'prefix', value: 'reports/' }] },
      { tools: ['fs_write'], decision: 'deny', reason: 'writes only under reports/' },
      { tools: ['danger'], decision: 'ask', reason: 'dangerous' }] } });
  const result = await agent.run('go');
  assert.equal(result.content, 'done');
  assert.deepEqual(ran, [['fs_write', { path: 'reports/a.md' }]], 'only the allowed call ran; ask without an approve hook is denied');
  assert.match(provider.seen[0], /POLICY_DENIED: writes only under reports\//);
  assert.match(provider.seen[2], /APPROVAL_UNAVAILABLE: dangerous/);
  agent.close();
});

test('approval hooks decide ask, the strictest of policy and hook wins, and failures deny', async () => {
  const ran = [], asked = [];
  const provider = scripted([{ name: 'a' }, { name: 'b' }, { name: 'c' }, { name: 'd' }, { name: 'e' }]);
  const agent = new Agent({ model: 'm', provider, maxTurns: 10, tools: ['a', 'b', 'c', 'd', 'e'].map(name => tool(name, ran)),
    toolPolicy: { rules: [{ tools: ['a', 'b'], decision: 'ask', reason: 'needs a person' }, { tools: ['e'], decision: 'deny', reason: 'never' }] },
    hooks: {
      approve: ({ call, reason }) => { asked.push([call.name, reason]); return call.name === 'a' ? true : { approved: false, reason: 'not today' }; },
      beforeTool: ({ call }) => call.name === 'c' ? { decision: 'deny', reason: 'hook says no' } : call.name === 'd' ? { decision: 'allow' } : undefined,
    } });
  await agent.run('go');
  assert.deepEqual(ran.map(([name]) => name), ['a', 'd'], 'a approved, b refused, c denied by the hook, d allowed, e denied by policy');
  assert.deepEqual(asked, [['a', 'needs a person'], ['b', 'needs a person']]);
  assert.match(provider.seen[1], /not today/);
  assert.match(provider.seen[2], /hook says no/);
  assert.match(provider.seen[4], /never/, 'a hook can tighten but never loosen: its allow does not override a policy deny');
  agent.close();
  // Hooks that throw or answer nonsense deny rather than let the call through.
  const failing = [];
  const broken = new Agent({ model: 'm', provider: scripted([{ name: 'a' }, { name: 'b' }, { name: 'c' }]), maxTurns: 6, tools: ['a', 'b', 'c'].map(name => tool(name, failing)),
    hooks: { beforeTool: ({ call }) => { if (call.name === 'a') throw new Error('boom'); return call.name === 'b' ? { decision: 'maybe' } : { decision: 'ask' }; }, approve: () => { throw new Error('ui crashed'); } } });
  await broken.run('go');
  assert.deepEqual(failing, [], 'nothing ran');
  broken.close();
});

test('afterTool redacts what the model sees and a failing filter withholds the output', async () => {
  const ran = [];
  const provider = scripted([{ name: 'a' }, { name: 'b' }]);
  const agent = new Agent({ model: 'm', provider, maxTurns: 6, tools: [tool('a', ran), tool('b', ran)],
    hooks: { afterTool: ({ call, result }) => { if (call.name === 'a') return { result: { ...result, args: 'REDACTED' } }; throw new Error('filter crashed'); } } });
  await agent.run('go');
  assert.equal(ran.length, 2, 'both tools ran');
  assert.match(provider.seen[0], /REDACTED/);
  assert.match(provider.seen[1], /POLICY_HOOK_FAILED: afterTool failed/);
  agent.close();
});

test('approval can wait for a person and cancelling the run cancels the wait; bad options are rejected', async () => {
  const ran = [];
  let release;
  const agent = new Agent({ model: 'm', provider: scripted([{ name: 'a' }]), tools: [tool('a', ran)], toolPolicy: { default: 'ask' },
    hooks: { approve: () => new Promise(resolve => { release = resolve; }) } });
  const run = agent.run('go');
  await new Promise(resolve => setTimeout(resolve, 50));
  assert.ok(release, 'the approval hook is waiting');
  agent.cancel();
  await assert.rejects(run, /CANCELLED/);
  assert.deepEqual(ran, []);
  agent.close();
  const waiting = new Agent({ model: 'm', provider: scripted([{ name: 'a' }]), tools: [tool('a', ran)], toolPolicy: { default: 'ask' },
    hooks: { approve: () => new Promise(resolve => setTimeout(() => resolve(true), 30)) } });
  assert.equal((await waiting.run('go')).content, 'done');
  assert.equal(ran.length, 1, 'approved after a wait, then it ran');
  waiting.close();
  assert.throws(() => new Agent({ model: 'm', toolPolicy: { default: 'maybe' } }), /invalid tool policy/);
  assert.throws(() => new Agent({ model: 'm', hooks: { beforeTool: 1 } }), /must be a function/);
  assert.throws(() => new Agent({ model: 'm', hooks: { afterwards() {} } }), /Unknown hook/);
});

test('with a durable store a denial is recorded as a normal tool result and an approval wait leaves nothing claimed', async () => {
  const { SessionStore } = await import('../index.mjs');
  const { mkdtempSync, rmSync } = await import('node:fs');
  const { tmpdir } = await import('node:os');
  const path = await import('node:path');
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-policy-'));
  const store = await SessionStore.open({ path: directory });
  try {
    const ran = [];
    const provider = scripted([{ name: 'a' }, { name: 'b' }]);
    const denied = await Agent.open({ store, agentId: 'one', sessionId: 's', model: 'm', provider, maxTurns: 6, tools: [tool('a', ran), tool('b', ran)],
      toolPolicy: { rules: [{ tools: ['a'], decision: 'deny', reason: 'no a' }] } });
    assert.equal((await denied.run('go')).content, 'done');
    assert.deepEqual(ran.map(([name]) => name), ['b']);
    assert.match(provider.seen[0], /POLICY_DENIED: no a/);
    denied.close();

    // Cancel while a person has not answered yet. Nothing was claimed, so the run is
    // simply resumable (not uncertain) and the next attempt asks again.
    const toolRuns = [];
    let asks = 0;
    const hooks = { approve: () => { asks++; return asks === 1 ? new Promise(() => {}) : true; } };
    const options = { store, agentId: 'two', sessionId: 's', model: 'm', maxTurns: 6, tools: [tool('a', toolRuns)], toolPolicy: { default: 'ask' }, hooks };
    const first = await Agent.open({ ...options, provider: scripted([{ name: 'a' }]) });
    const run = first.run('go');
    await new Promise(resolve => setTimeout(resolve, 100));
    first.cancel();
    await assert.rejects(run, /CANCELLED/);
    assert.equal(toolRuns.length, 0);
    first.close();
    const second = await Agent.open({ ...options, provider: scripted([]), resume: true });
    const resumed = await second.resume();
    assert.equal(resumed.status, 'completed');
    assert.equal(asks, 2, 'the approval was asked again after the restart');
    assert.equal(toolRuns.length, 1, 'and the tool ran exactly once');
    second.close();
  } finally { await store.closeAsync(); rmSync(directory, { recursive: true, force: true }); }
});
