// Offline durable bridge contract/byte comparison. Actual HiveDB writes in two temporary stores.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { mkdtempSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
const native = createRequire(import.meta.url)('../packages/npm/native/hive.node');

async function compare(historyTurns, toolRounds, contextBytes) {
  const directory = mkdtempSync(path.join(tmpdir(), 'hive-durable-bridge-'));
  const oldStore = new native.NativeSessionStore(path.join(directory, 'legacy'), 'default');
  const store = new native.NativeSessionStore(path.join(directory, 'handle'), 'default');
  const config = { op: 'create', model: 'fixture', max_turns: 32, tools: [{ name: 'read', description: 'Read', parameters: { type: 'object' }, allow_repeated_results: true }],
    policy: { max_tool_calls: 32, ...(contextBytes ? { max_context_bytes: contextBytes, context_overflow: 'drop_oldest_turns' } : {}) } };
  const handle = store.openAgent('a', 's', JSON.stringify(config), false);
  let legacy = JSON.parse(oldStore.dispatch(JSON.stringify({ op: 'load', agent_id: 'a', session_id: 's', initial: JSON.parse(native.dispatch(JSON.stringify(config))).state })));
  let owned; let measured = false; let ticks = 0;
  const totals = { legacy: { sent_bytes: 0, received_bytes: 0 }, handle: { sent_bytes: 0, received_bytes: 0 } };
  async function send(which, command) {
    const encoded = JSON.stringify(command);
    const raw = which === 'legacy' ? oldStore.dispatch(encoded) : await handle.dispatchAsync(encoded);
    if (measured) { totals[which].sent_bytes += Buffer.byteLength(encoded); totals[which].received_bytes += Buffer.byteLength(raw); }
    return JSON.parse(raw);
  }
  async function step(op, extra = {}) {
    let oldCommand = { op, agent_id: 'a', session_id: 's', revision: legacy.revision, ...extra };
    const newCommand = { op, ...extra };
    if (op === 'begin_run') oldCommand.initial = legacy.state;
    if (op === 'advance') { oldCommand.execution_id = legacy.operation.execution_id; newCommand.execution_id = owned.operation.execution_id; }
    if (op === 'commit') oldCommand.state = legacy.state;
    const oldResult = await send('legacy', oldCommand); const result = await send('handle', newCommand);
    if (op === 'commit') { assert.equal(result, oldResult); legacy.revision = oldResult; return; }
    legacy = oldResult; owned = result;
    assert.deepEqual(owned.action, legacy.action); assert.equal(owned.revision, legacy.revision);
    assert.equal(owned.state.messages, undefined);
  }
  const finish = content => ({ outcome: { type: 'model', reply: { content, usage: { input_tokens: 3, output_tokens: 2 } } } });
  let timer;
  try {
    for (let i = 0; i < historyTurns; i++) {
      await step('begin_run', { prompt: `Historical ${i}: ${'x'.repeat(1024)}` });
      await step('claim'); await step('advance', finish(`Fact ${i}: ${'y'.repeat(1024)}`)); await step('commit');
    }
    measured = true; timer = setInterval(() => { ticks++; }, 1); const start = performance.now();
    await step('begin_run', { prompt: 'Explicit tool task' });
    for (let i = 0; i < toolRounds; i++) {
      await step('claim'); await step('advance', { outcome: { type: 'model', reply: { tool_calls: [{ id: `c${i}`, name: 'read', arguments: { i } }], usage: { input_tokens: 3, output_tokens: 2 } } } });
      await step('claim'); await step('advance', { outcome: { type: 'tool', result: { i, value: 'known' } } });
    }
    await step('claim'); await step('advance', finish('Finished 🦀')); await step('commit');
    clearInterval(timer);
    assert.deepEqual(JSON.parse(handle.dispatch('{"op":"messages"}')), legacy.state.messages);
    console.log(`history=${historyTurns}, tools=${toolRounds}: timer advanced ${ticks} times during ${Math.round(performance.now() - start)} ms of combined fixture work (informational)`);
    return { history_turns: historyTurns, history_chars_per_message: 1024, tool_rounds: toolRounds, max_context_bytes: contextBytes ?? null, ...totals,
      note: 'Actual CAS/checkpoint writes, claims and commit included. Setup/history ingestion and explicit getter excluded; model action context included. Random IDs have fixed lengths.' };
  } finally { clearInterval(timer); oldStore.close(); store.close(); rmSync(directory, { recursive: true, force: true }); }
}
const cases = [await compare(20, 3), await compare(60, 12, 16384)];
mkdirSync(new URL('../evals/', import.meta.url), { recursive: true });
writeFileSync(new URL('../evals/durable-bridge.json', import.meta.url), `${JSON.stringify({ scope: 'Local durable JSON bridge bytes with deterministic replies; no remote calls or billing claims. Timer counts/timings are informational and excluded from this report.', cases }, null, 2)}\n`);
console.log('Wrote evals/durable-bridge.json');
