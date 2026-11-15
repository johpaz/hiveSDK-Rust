// Local JSON bridge measurement; no providers, tokens, billing or remote models.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { mkdirSync, writeFileSync } from 'node:fs';
import { performance } from 'node:perf_hooks';
const require = createRequire(import.meta.url);
const native = require('../packages/npm/native/hive.node');

function compare({ historyTurns, historyChars, toolRounds, contextBytes }) {
  const config = { op: 'create', model: 'fixture', max_turns: 32,
    policy: { max_tool_calls: 32, ...(contextBytes ? { max_context_bytes: contextBytes, context_overflow: 'drop_oldest_turns' } : {}) },
    tools: [{ name: 'read', description: 'Read', parameters: { type: 'object' }, allow_repeated_results: true }] };
  let legacy = JSON.parse(native.dispatch(JSON.stringify(config)));
  const handle = new native.NativeEngine(JSON.stringify(config));
  const totals = { legacy: { sent_bytes: 0, received_bytes: 0 }, handle: { sent_bytes: 0, received_bytes: 0 } };
  const elapsed = { legacy: 0, handle: 0 };
  let measured = false;
  const send = (path, input) => {
    const start = performance.now(); const encoded = JSON.stringify(input);
    const output = path === 'legacy' ? native.dispatch(encoded) : handle.dispatch(encoded);
    const decoded = JSON.parse(output);
    if (measured) {
      totals[path].sent_bytes += Buffer.byteLength(encoded);
      totals[path].received_bytes += Buffer.byteLength(output);
      elapsed[path] += performance.now() - start;
    }
    return decoded;
  };
  function step(command) {
    const owned = send('handle', command);
    legacy = send('legacy', { ...command, state: legacy.state });
    assert.deepEqual(owned.action, legacy.action);
    assert.deepEqual(Object.keys(owned.state), ['deadline_unix_ms']);
  }
  const finish = content => ({ op: 'reply', reply: { content, usage: { input_tokens: 3, output_tokens: 2 } } });
  for (let i = 0; i < historyTurns; i++) {
    step({ op: 'start', prompt: `Historical ${i}: ${'x'.repeat(historyChars)}` });
    step(finish(`Fact ${i}: ${'y'.repeat(historyChars)}`));
    send('handle', { op: 'commit' }); send('handle', { op: 'abort' });
  }
  measured = true;
  step({ op: 'start', prompt: 'Explicit tool task' });
  for (let i = 0; i < toolRounds; i++) {
    step({ op: 'reply', reply: { content: null, tool_calls: [{ id: `c${i}`, name: 'read', arguments: { i } }], usage: { input_tokens: 3, output_tokens: 2 } } });
    step({ op: 'tool_result', id: `c${i}`, result: { i, value: 'known' } });
  }
  step(finish('Finished 🦀'));
  send('handle', { op: 'commit' }); send('handle', { op: 'abort' });
  // Getter cost is separate: ordinary transitions do not need to export history.
  const committed = JSON.parse(handle.dispatch('{"op":"messages"}'));
  assert.deepEqual(committed, legacy.state.messages);
  console.log(`history=${historyTurns}, tools=${toolRounds}, local ms: legacy=${elapsed.legacy.toFixed(2)}, handle=${elapsed.handle.toFixed(2)} (informational)`);
  return { history_turns: historyTurns, history_chars_per_message: historyChars, tool_rounds: toolRounds,
    max_context_bytes: contextBytes ?? null, ...totals,
    history_getter_response_bytes: Buffer.byteLength(JSON.stringify(committed)),
    note: 'Includes handle commit/abort. Context carried by model actions remains included; setup/history ingestion and explicit getter are excluded.' };
}
const cases = [
  { historyTurns: 10, historyChars: 1024, toolRounds: 3 },
  { historyTurns: 100, historyChars: 1024, toolRounds: 3 },
  { historyTurns: 100, historyChars: 1024, toolRounds: 12 },
  { historyTurns: 100, historyChars: 8192, toolRounds: 12, contextBytes: 65536 },
].map(compare);
const destination = new URL('../evals/engine-bridge.json', import.meta.url);
mkdirSync(new URL('../evals/', import.meta.url), { recursive: true });
writeFileSync(destination, `${JSON.stringify({ scope: 'Local JSON bridge bytes, deterministic fixtures; no remote model calls or billing claims.', cases }, null, 2)}\n`);
console.log('Wrote evals/engine-bridge.json');
