import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, rmSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { Agent, SessionStore } from '../index.mjs';

const tool = execute => ({ name: 'pay', description: 'Payment', parameters: { type: 'object' }, execute });
const reply = { content: null, tool_calls: [{ id: 'payment', name: 'pay', arguments: { amount: 10 } }],
  usage: { input_tokens: 5, output_tokens: 3 } };
const options = (store, tools, provider, extra = {}) => ({ model: 'test', store, tools, provider,
  agentId: 'a', sessionId: 's', ...extra });

test('clean checkpoint resumes after a tool without paying twice or resetting counters', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-checkpoint-'));
  const store = new SessionStore({ path: directory });
  let calls = 0; let payments = 0; let context;
  const tools = [tool((_args, ctx) => { payments++; context = ctx; return { receipt: 'r1' }; })];
  const provider = { complete(request) {
    if (++calls === 1) return reply;
    assert.equal(JSON.parse(request.messages.at(-1).content).receipt, 'r1');
    return { content: 'Paid', usage: { input_tokens: 7, output_tokens: 2 } };
  } };
  try {
    let modelEvents = 0;
    const agent = new Agent(options(store, tools, provider, { onEvent(action) {
      if (action.type === 'model' && ++modelEvents === 2) throw new Error('interruption');
    } }));
    await assert.rejects(agent.run('Pay'), /interruption/);
    const checkpoint = store.inspect({ agentId: 'a', sessionId: 's' });
    assert.equal(checkpoint.run.in_flight, null);
    assert.equal(context.execution_id, checkpoint.run.tools[0].execution_id);
    const resumed = new Agent(options(store, tools, provider, { resume: true }));
    const result = await resumed.resume();
    assert.equal(result.content, 'Paid');
    assert.equal(result.stats.model_calls, 2);
    assert.equal(result.stats.input_tokens, 12);
    assert.equal(result.stats.tool_calls, 1);
    assert.equal(payments, 1);
    assert.equal(calls, 2);
    assert.equal(store.inspect({ agentId: 'a', sessionId: 's' }).run.run_id, context.run_id);
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('actual process exit during a tool requires reconciliation, never automatic replay', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-crash-'));
  const moduleUrl = new URL('../index.mjs', import.meta.url).href;
  const script = `
import { Agent, SessionStore } from ${JSON.stringify(moduleUrl)};
import { writeFileSync } from 'node:fs';
const store = new SessionStore({path:process.argv[1]});
const agent = new Agent({model:'test',store,agentId:'a',sessionId:'s',
  provider:{complete:()=>(${JSON.stringify(reply)})},
  tools:[{name:'pay',description:'Payment',parameters:{type:'object'},execute:(_args,ctx)=>{
    writeFileSync(process.argv[1]+'/receipt.json',JSON.stringify(ctx)); process.exit(17);
  }}]});
await agent.run('Pay');`;
  let store;
  try {
    const child = spawnSync(process.execPath, ['--input-type=module', '-e', script, directory], { timeout: 10_000, encoding: 'utf8' });
    assert.equal(child.status, 17, child.stderr);
    store = new SessionStore({ path: directory });
    let replays = 0;
    const tools = [tool(() => { replays++; return { receipt: 'unexpected' }; })];
    const provider = { complete: () => ({ content: 'Paid' }) };
    assert.throws(() => new Agent(options(store, tools, provider, { resume: true })), /SESSION_OPERATION_UNCERTAIN/);
    const snapshot = store.inspect({ agentId: 'a', sessionId: 's' });
    const receipt = JSON.parse(readFileSync(path.join(directory, 'receipt.json'), 'utf8'));
    assert.equal(snapshot.run.in_flight.execution_id, receipt.execution_id);
    store.resolveTool({ agentId: 'a', sessionId: 's', revision: snapshot.revision,
      executionId: receipt.execution_id, result: { receipt: 'verified-externally' } });
    const resumed = new Agent(options(store, tools, provider, { resume: true }));
    assert.equal((await resumed.resume()).content, 'Paid');
    assert.equal(replays, 0);
    assert.equal(store.inspect({ agentId: 'a', sessionId: 's' }).run.tools[0].status, 'completed');
  } finally { store?.close(); rmSync(directory, { recursive: true, force: true }); }
});

test('uncertain model outcome can be supplied without spending another model call', async () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-model-'));
  const store = new SessionStore({ path: directory });
  let calls = 0;
  const provider = { complete() { calls++; throw new Error('connection lost'); } };
  try {
    await assert.rejects(new Agent(options(store, [], provider)).run('Hello'), /connection lost/);
    assert.throws(() => new Agent(options(store, [], provider, { resume: true })), /SESSION_OPERATION_UNCERTAIN/);
    const snapshot = store.inspect({ agentId: 'a', sessionId: 's' });
    store.resolveModel({ agentId: 'a', sessionId: 's', revision: snapshot.revision,
      executionId: snapshot.run.in_flight.execution_id,
      reply: { content: 'Retrieved response', usage: { input_tokens: 4, output_tokens: 2 } } });
    const resumed = new Agent(options(store, [], provider, { resume: true }));
    assert.equal((await resumed.resume()).content, 'Retrieved response');
    assert.equal(calls, 1);
  } finally { store.close(); rmSync(directory, { recursive: true, force: true }); }
});
