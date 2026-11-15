// Copy to the isolated npm consumer described in docs/local-project-validation.md.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile, writeFile, realpath, mkdtemp } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Agent, Objective, OpenAICompatible, SessionStore } from '@johpaz/hive-sdk-rust';

const mode = process.env.HIVE_VALIDATION_MODE ?? 'offline';
assert.ok(['offline', 'live'].includes(mode), 'HIVE_VALIDATION_MODE must be offline or live');
assert.ok(process.env.HIVE_LOCAL_PROJECT && process.env.HIVE_TEST_ROOT, 'Set HIVE_LOCAL_PROJECT and HIVE_TEST_ROOT');
assert.ok(process.platform === 'linux' && process.arch === 'x64', 'This runbook currently validates Linux x64');
assert.ok(Number(process.versions.node.split('.')[0]) >= 24, 'Node >=24 required');
const project = await realpath(process.env.HIVE_LOCAL_PROJECT);
const testRoot = await realpath(process.env.HIVE_TEST_ROOT);
const relative = path.relative(project, testRoot);
assert.ok(relative === '..' || relative.startsWith('..' + path.sep) || path.isAbsolute(relative), 'Test directory must be outside the project');
const manifest = JSON.parse(await readFile(path.join(project, 'package.json'), 'utf8'));
const expected = { name: manifest.name, version: manifest.version };
assert.ok(typeof expected.name === 'string' && typeof expected.version === 'string', 'Project needs name/version');

let provider; let model = 'deterministic-fixture'; let endpointOrigin = null;
if (mode === 'live') {
  assert.ok(process.env.HIVE_VALIDATION_MODEL && process.env.HIVE_PROVIDER_BASE_URL, 'Live mode needs model and base URL');
  const endpoint = new URL(process.env.HIVE_PROVIDER_BASE_URL);
  assert.ok(['http:', 'https:'].includes(endpoint.protocol), 'Use an HTTP(S) endpoint');
  const outputTokenParameter = process.env.HIVE_OUTPUT_TOKEN_PARAMETER ?? 'max_completion_tokens';
  assert.ok(['max_completion_tokens', 'max_tokens'].includes(outputTokenParameter), 'Invalid output token parameter');
  model = process.env.HIVE_VALIDATION_MODEL; endpointOrigin = endpoint.origin;
  provider = new OpenAICompatible({ baseUrl: endpoint.href, apiKey: process.env.HIVE_PROVIDER_API_KEY,
    outputTokenParameter, timeoutMs: 10000, maxResponseBytes: 65536 });
} else {
  provider = { complete(request) {
    if (request.messages.at(-1).role === 'tool') {
      return { content: request.messages.at(-1).content, usage: { input_tokens: 1, output_tokens: 1 } };
    }
    return { content: null, tool_calls: [{ id: 'project-read', name: 'leer_proyecto', arguments: {} }],
      usage: { input_tokens: 1, output_tokens: 1 } };
  } };
}

const runDirectory = await mkdtemp(path.join(testRoot, 'run-'));
const sdkEntry = fileURLToPath(import.meta.resolve('@johpaz/hive-sdk-rust'));
const sdkManifest = JSON.parse(await readFile(path.join(path.dirname(sdkEntry), 'package.json'), 'utf8'));
const suffix = process.report.getReport().header.glibcVersionRuntime ? 'gnu' : 'musl';
const nativeEntry = fileURLToPath(import.meta.resolve(`@johpaz/hive-sdk-rust-linux-x64-${suffix}`));
const report = { mode, model, endpoint_origin: endpointOrigin, node: process.versions.node,
  sdk_version: sdkManifest.version, sdk_entry: sdkEntry,
  native_sha256: createHash('sha256').update(await readFile(nativeEntry)).digest('hex'),
  project: expected, run_directory: runDirectory, passed: false, tool_effects: 0, events: [] };
const policy = { max_tool_calls: 1, max_total_tokens: 1024, max_output_tokens: 128,
  max_total_output_tokens: 256, max_run_ms: 20000, max_context_bytes: 16384 };
const objectivePolicy = { max_model_calls: 2, max_tool_calls: 1, max_output_tokens: 128,
  max_total_output_tokens: 256, max_run_ms: 20000 };
report.policy = policy; report.objective_policy = objectivePolicy;
let store; let agent; const started = performance.now();
try {
  store = await SessionStore.open({ path: path.join(runDirectory, 'db'), tenantId: 'local-validation' });
  const objective = await Objective.open(objectivePolicy, { store, id: 'project-metadata' });
  const options = { model, provider, store, objective, agentId: 'project-reader', sessionId: 'metadata', maxTurns: 2, policy,
    system: 'Usa leer_proyecto exactamente una vez. Devuelve solo el JSON recibido con name y version, sin markdown.',
    tools: [{ name: 'leer_proyecto', description: 'Lee el nombre y versión del proyecto local configurado por el operador.',
      parameters: { type: 'object', properties: {}, additionalProperties: false },
      async execute() {
        report.tool_effects++;
        const current = JSON.parse(await readFile(path.join(project, 'package.json'), 'utf8'));
        return { name: current.name, version: current.version };
      } }], onEvent(event) { report.events.push({ type: event.type, elapsed_ms: Math.round(performance.now() - started) }); } };
  agent = await Agent.open(options);
  report.result = await agent.run('Consulta el proyecto configurado y devuelve su nombre y versión en JSON.');
  report.objective = await objective.inspectAsync();
  assert.equal(report.result.status, 'completed');
  assert.deepEqual(JSON.parse(report.result.content), expected);
  assert.equal(report.tool_effects, 1);
  assert.equal(report.result.stats.model_calls, 2); assert.equal(report.result.stats.tool_calls, 1);
  assert.equal(report.objective.model_calls, 2); assert.equal(report.objective.tool_calls, 1);
  agent.close(); agent = await Agent.open(options);
  assert.equal(agent.messages.at(-1).content, report.result.content);
  report.history_reopened = true; report.passed = true;
} catch (error) {
  const key = process.env.HIVE_PROVIDER_API_KEY;
  const message = String(error.message);
  report.error = { name: error.name, message: (key ? message.split(key).join('[redacted]') : message).slice(0, 1000) };
  process.exitCode = 1;
} finally {
  try { agent?.close(); if (store) await store.closeAsync(); }
  catch (error) { report.passed = false; report.cleanup_error = error.name; process.exitCode = 1; }
  report.elapsed_ms = Math.round(performance.now() - started);
  await writeFile(path.join(runDirectory, 'report.json'), JSON.stringify(report, null, 2) + '\n', { flag: 'wx' });
}
console.log(JSON.stringify({ passed: report.passed, mode, report: path.join(runDirectory, 'report.json') }));
