import { createRequire } from 'node:module';
import { CatalogServices } from './services.mjs';
import { DurableQueue } from './queue.mjs';
import { SecretVault, copySecretKey } from './secrets.mjs';
export { SecretVault } from './secrets.mjs';
export { DurableQueue } from './queue.mjs';
export { runGoal, extractJson } from './goals.mjs';
export { CatalogServices } from './services.mjs';
import { existsSync } from 'node:fs';
import { nativePackage } from './platform.mjs';
const require = createRequire(import.meta.url);
let binding;
const deferredLoad = Symbol('deferred native load');
function native() {
  if (Number(process.versions.node.split('.')[0]) < 24) throw new Error('UNSUPPORTED_RUNTIME: Hive SDK requires Node >=24');
  if (!binding) {
    const developmentBinding = new URL('./native/hive.node', import.meta.url);
    const packageName = existsSync(developmentBinding) ? './native/hive.node' : nativePackage();
    try { binding = require(packageName); }
    catch (cause) {
      throw new Error(`Hive native module could not load (${packageName}). Install with optional dependencies enabled. For source development, run npm run build:native in packages/npm.`, { cause });
    }
  }
  return binding;
}
export class SessionStore {
  #native;
  #active = 0;
  #exclusive = false;

  get services() { return new CatalogServices(this); }
  /** Tenant-scoped artifact store with its files under `root`. */
  artifacts(root) { return new ArtifactStore(this, root); }
  openQueue(id = 'default', policy = {}) { return DurableQueue.open({ store: this, id, policy }); }

  async openSecrets({ id = 'default', keyId = 'primary', key }) {
    const material = copySecretKey(key); let held = false;
    try {
      this._retain(); held = true;
      const handle = await this.#native.openSecretVault(id, keyId, material);
      return new SecretVault(this, handle);
    } catch (error) { if (held) this._release(); throw error; }
    finally { material.fill(0); }
  }

  static async open({ path, tenantId = 'default' }) {
    return new SessionStore({ path: '', tenantId }, await native().openSessionStore(path, tenantId));
  }
  #available() { if (this.#exclusive) throw new Error('BUSY: store maintenance is pending'); }
  #maintenance() {
    this.#available();
    if (this.#active) throw new Error('BUSY: stop active operations before store maintenance');
    this.#exclusive = true;
  }

  constructor({ path, tenantId = 'default' }, internal) {
    this.#native = internal ?? new (native().NativeSessionStore)(path, tenantId);
  }

  forTenant(tenantId) {
    this.#available();
    return new SessionStore({ path: '', tenantId }, this.#native.forTenant(tenantId));
  }
  async forTenantAsync(tenantId) {
    this._retain();
    try { return new SessionStore({ path: '', tenantId }, await this.#native.forTenantAsync(tenantId)); }
    finally { this._release(); }
  }

  _dispatch(command) {
    this.#available();
    try { return JSON.parse(this.#native.dispatch(JSON.stringify(command))); }
    catch (error) {
      if (error.message?.startsWith('OBJECTIVE_LIMIT:')) throw new ObjectiveLimit(error.message);
      throw error;
    }
  }
  async _dispatchAsync(command, exclusive = false) {
    const encoded = JSON.stringify(command);
    if (exclusive) this.#maintenance(); else this._retain();
    try { return JSON.parse(await this.#native.dispatchAsync(encoded)); }
    catch (error) { if (error.message?.startsWith('OBJECTIVE_LIMIT:')) throw new ObjectiveLimit(error.message); throw error; }
    finally { if (exclusive) this.#exclusive = false; else this._release(); }
  }

  _begin(command) {
    const frame = this._dispatch(command);
    this.#active++;
    return frame;
  }

  _release() { this.#active--; }
  _retain() { this.#available(); this.#active++; }
  _openAgent(agentId, sessionId, config, resume) {
    this.#available();
    return this.#native.openAgent(agentId, sessionId, JSON.stringify(config), resume);
  }
  async _openAgentAsync(agentId, sessionId, config, resume) {
    this._retain();
    try { return await this.#native.openAgentAsync(agentId, sessionId, JSON.stringify(config), resume); }
    finally { this._release(); }
  }

  recover({ agentId, sessionId }) {
    if (this.#active) throw new Error('BUSY: stop active agents before recovery');
    return this._dispatch({ op: 'recover', agent_id: agentId, session_id: sessionId });
  }

  inspect({ agentId, sessionId, runId }) {
    return this._dispatch({ op: 'inspect', agent_id: agentId, session_id: sessionId, run_id: runId });
  }
  inspectAsync({ agentId, sessionId, runId }) {
    return this._dispatchAsync({ op: 'inspect', agent_id: agentId, session_id: sessionId, run_id: runId });
  }
  recoverAsync({ agentId, sessionId }) {
    return this._dispatchAsync({ op: 'recover', agent_id: agentId, session_id: sessionId }, true);
  }
  resolveToolAsync({ agentId, sessionId, revision, executionId, result }) {
    return this._dispatchAsync({ op: 'resolve', agent_id: agentId, session_id: sessionId, revision,
      execution_id: executionId, outcome: { type: 'tool', result } }, true);
  }
  resolveModelAsync({ agentId, sessionId, revision, executionId, reply }) {
    return this._dispatchAsync({ op: 'resolve', agent_id: agentId, session_id: sessionId, revision,
      execution_id: executionId, outcome: { type: 'model', reply } }, true);
  }

  resolveTool({ agentId, sessionId, revision, executionId, result }) {
    if (this.#active) throw new Error('BUSY: stop active agents before resolving operations');
    return this._dispatch({ op: 'resolve', agent_id: agentId, session_id: sessionId,
      revision, execution_id: executionId, outcome: { type: 'tool', result } });
  }

  resolveModel({ agentId, sessionId, revision, executionId, reply }) {
    if (this.#active) throw new Error('BUSY: stop active agents before resolving operations');
    return this._dispatch({ op: 'resolve', agent_id: agentId, session_id: sessionId,
      revision, execution_id: executionId, outcome: { type: 'model', reply } });
  }

  close() {
    this.#available();
    if (this.#active) throw new Error('BUSY: cannot close a store with active runs');
    this.#native.close();
  }
  async closeAsync() {
    this.#maintenance();
    try { await this.#native.closeAsync(); }
    finally { this.#exclusive = false; }
  }
}

class NativeHttpProvider {
  #native;
  constructor(config) {
    this.config = config;
    this.#native = new (native().NativeProvider)(JSON.stringify(this.config));
  }
  async *#events(request, streaming, { signal } = {}) {
    checkCancelled(signal);
    const handle = await this.#native.start(JSON.stringify(request), streaming);
    const abort = () => handle.cancel();
    signal?.addEventListener('abort', abort, { once: true });
    if (signal?.aborted) abort();
    try {
      for (;;) {
        const raw = await handle.next();
        if (raw === null || raw === undefined) break;
        yield JSON.parse(raw);
      }
    } catch (error) {
      checkCancelled(signal);
      if (error.message?.startsWith('TIMEOUT:')) error.code = 'TIMEOUT';
      if (error.message?.startsWith('DEADLINE_EXCEEDED:')) error.code = 'DEADLINE_EXCEEDED';
      throw error;
    } finally {
      signal?.removeEventListener('abort', abort);
      handle.cancel();
    }
  }
  stream(request, context = {}) { return this.#events(request, true, context); }
  async complete(request, context = {}) {
    let reply;
    for await (const event of this.#events(request, false, context)) {
      if (event.type === 'reply') reply = event.reply;
    }
    if (!reply) throw new Error('PROVIDER_ERROR: missing model reply');
    return reply;
  }
}

export class OpenAICompatible extends NativeHttpProvider {
  constructor({ baseUrl = 'https://api.openai.com/v1', apiKey, timeoutMs = 60_000,
    maxResponseBytes = 16 * 1024 * 1024, outputTokenParameter = 'max_completion_tokens', resilience } = {}) {
    super({ kind: 'openai_compatible', base_url: baseUrl, api_key: apiKey, timeout_ms: timeoutMs,
      max_response_bytes: maxResponseBytes, output_token_parameter: outputTokenParameter, resilience });
  }
}

/**
 * A hosted provider by id (see docs/providers.md). Protocol, base URL and wire
 * profile come from the preset; anything given here wins over it.
 */
export class PresetProvider extends NativeHttpProvider {
  constructor(preset, { apiKey, baseUrl, timeoutMs, maxResponseBytes, profile, resilience } = {}) {
    super({ preset, api_key: apiKey, base_url: baseUrl, timeout_ms: timeoutMs, max_response_bytes: maxResponseBytes, profile, resilience });
  }
}

export class Anthropic extends NativeHttpProvider {
  constructor({ baseUrl = 'https://api.anthropic.com/v1', apiKey, timeoutMs = 60_000,
    maxResponseBytes = 16 * 1024 * 1024, apiVersion = '2026-10-21', defaultMaxOutputTokens, contextWindow, resilience } = {}) {
    super({ kind: 'anthropic', base_url: baseUrl, api_key: apiKey, timeout_ms: timeoutMs,
      max_response_bytes: maxResponseBytes, api_version: apiVersion, default_max_output_tokens: defaultMaxOutputTokens,
      context_window: contextWindow, resilience });
  }
}

export class Gemini extends NativeHttpProvider {
  constructor({ baseUrl = 'https://generativelanguage.googleapis.com/v1beta', apiKey, timeoutMs = 60_000,
    maxResponseBytes = 16 * 1024 * 1024, defaultMaxOutputTokens, contextWindow, resilience } = {}) {
    super({ kind: 'gemini', base_url: baseUrl, api_key: apiKey, timeout_ms: timeoutMs,
      max_response_bytes: maxResponseBytes, default_max_output_tokens: defaultMaxOutputTokens,
      context_window: contextWindow, resilience });
  }
}

/** Ollama's native chat API (`/api/chat`): the context size, keep-alive and thinking are options only it takes. */
export class Ollama extends NativeHttpProvider {
  constructor({ baseUrl = 'http://localhost:11434/api', apiKey, timeoutMs = 60_000, maxResponseBytes = 16 * 1024 * 1024,
    defaultMaxOutputTokens, contextWindow, numCtx, keepAlive, think, resilience } = {}) {
    super({ kind: 'ollama', base_url: baseUrl, api_key: apiKey, timeout_ms: timeoutMs, max_response_bytes: maxResponseBytes,
      default_max_output_tokens: defaultMaxOutputTokens, context_window: contextWindow, num_ctx: numCtx,
      keep_alive: keepAlive, think, resilience });
  }
}

export class HiveAgents extends NativeHttpProvider {
  constructor({ baseUrl, apiKey, timeoutMs = 120_000, maxResponseBytes = 16 * 1024 * 1024,
    contextWindow = 50_000 } = {}) {
    super({ kind: 'hiveagents', base_url: baseUrl, api_key: apiKey, timeout_ms: timeoutMs,
      max_response_bytes: maxResponseBytes, context_window: contextWindow });
  }
}

export class ToolOperationUncertain extends Error {
  constructor(message = 'TOOL_OPERATION_UNCERTAIN: external tool result must be reconciled') {
    super(message); this.name = 'ToolOperationUncertain'; this.code = 'TOOL_OPERATION_UNCERTAIN';
  }
}
export class McpOperationUncertain extends ToolOperationUncertain {
  constructor(message = 'MCP_OPERATION_UNCERTAIN: tool response was not confirmed') {
    super(message); this.name = 'McpOperationUncertain'; this.code = 'MCP_OPERATION_UNCERTAIN';
  }
}
export class ObjectiveLimit extends Error {
  constructor(message) { super(message); this.name = 'ObjectiveLimit'; this.code = 'OBJECTIVE_LIMIT'; }
}
export class VerificationUncertain extends ToolOperationUncertain {
  constructor(message = 'VERIFICATION_OPERATION_UNCERTAIN: inspect and reconcile the verification before repeating it', cause) {
    super(message); this.name = 'VerificationUncertain'; this.code = 'VERIFICATION_OPERATION_UNCERTAIN';
    if (cause !== undefined) this.cause = cause;
  }
}
export class SummaryUncertain extends ToolOperationUncertain {
  constructor(message = 'SUMMARY_OPERATION_UNCERTAIN: inspect and reconcile the summary before repeating it', cause) {
    super(message); this.name = 'SummaryUncertain'; this.code = 'SUMMARY_OPERATION_UNCERTAIN';
    if (cause !== undefined) this.cause = cause;
  }
}
export class TriggerUncertain extends ToolOperationUncertain {
  constructor(message = 'TRIGGER_OPERATION_UNCERTAIN: inspect and reconcile the trigger before dispatching another occurrence', cause) {
    super(message); this.name = 'TriggerUncertain'; this.code = 'TRIGGER_OPERATION_UNCERTAIN';
    if (cause !== undefined) this.cause = cause;
  }
}
let delegateAgent;
function startDelegate(agent, prompt, objective, depth, parentSignal, summaryKey) {
  const controller = new AbortController();
  const abort = () => controller.abort(parentSignal.reason);
  parentSignal.addEventListener('abort', abort, { once: true });
  if (parentSignal.aborted) abort();
  const cleanup = () => parentSignal.removeEventListener('abort', abort);
  try {
    const promise = delegateAgent(agent, prompt, objective, depth, controller.signal, summaryKey);
    promise.then(cleanup, cleanup);
    return { promise, cancel: () => controller.abort() };
  } catch (error) { cleanup(); throw error; }
}
export class Objective {
  #native;
  #store;
  #id;
  #controllers = new Set();
  #exclusive = false;
  static async open(policy = {}, options = {}) {
    const objective = new Objective(policy, options, deferredLoad);
    if (options.store !== undefined) await options.store._dispatchAsync({ op: 'open_objective', id: options.id, policy });
    return objective;
  }
  constructor(policy = {}, { store, id } = {}, internal) {
    if (store !== undefined) {
      if (!(store instanceof SessionStore) || typeof id !== 'string') throw new TypeError('Durable Objective requires a SessionStore and id');
      if (internal !== deferredLoad) store._dispatch({ op: 'open_objective', id, policy });
      this.#store = store; this.#id = id;
    } else {
      if (id !== undefined) throw new TypeError('Objective id requires a SessionStore');
      this.#native = new (native().NativeObjective)(JSON.stringify(policy));
    }
  }
  get id() { return this.#id; }
  _same(other) { return this === other || (this.#store !== undefined && this.#store === other.#store && this.#id === other.#id); }
  _validateStore(store) {
    if (store && store !== this.#store) throw new TypeError('Process-local Objective cannot use SessionStore; durable Objective requires the same SessionStore');
  }
  _dispatch(command) {
    if (this.#exclusive) throw new Error('BUSY: objective reconciliation is pending');
    try { return this.#store ? this.#store._dispatch({ op: 'objective', id: this.#id, command })
      : JSON.parse(this.#native.dispatch(JSON.stringify(command))); }
    catch (error) {
      if (error.message?.startsWith('OBJECTIVE_LIMIT:')) throw new ObjectiveLimit(error.message);
      if (error.message?.startsWith('VERIFICATION_OPERATION_UNCERTAIN:')) throw new VerificationUncertain(error.message, error);
      if (error.message?.startsWith('TRIGGER_OPERATION_UNCERTAIN:')) throw new TriggerUncertain(error.message, error);
      if (error.message?.startsWith('SUMMARY_OPERATION_UNCERTAIN:')) throw new SummaryUncertain(error.message, error);
      throw error;
    }
  }
  async _dispatchAsync(command) {
    try { return this.#store ? await this.#store._dispatchAsync({ op: 'objective', id: this.#id, command }) : JSON.parse(this.#native.dispatch(JSON.stringify(command))); }
    catch (error) {
      if (error.message?.startsWith('OBJECTIVE_LIMIT:')) throw new ObjectiveLimit(error.message);
      if (error.message?.startsWith('VERIFICATION_OPERATION_UNCERTAIN:')) throw new VerificationUncertain(error.message, error);
      if (error.message?.startsWith('TRIGGER_OPERATION_UNCERTAIN:')) throw new TriggerUncertain(error.message, error);
      if (error.message?.startsWith('SUMMARY_OPERATION_UNCERTAIN:')) throw new SummaryUncertain(error.message, error);
      throw error;
    }
  }
  async #reconcile(command) {
    if (this.#exclusive || this.#controllers.size) throw new Error('BUSY: stop objective executors before reconciliation');
    this.#exclusive = true;
    try {
      return await this._dispatchAsync(command);
    } finally { this.#exclusive = false; }
  }
  inspectAsync() {
    if (this.#exclusive) return Promise.reject(new Error('BUSY: objective reconciliation is pending'));
    return this._dispatchAsync({ op: 'inspect' });
  }
  resolveSummaryAsync({ key, executionId, content }) { return this.#reconcile({ op: 'summary_finish', key, execution_id: executionId, content }); }
  resolveVerificationAsync({ key, executionId, verdict }) { return this.#reconcile({ op: 'verify_finish', key, execution_id: executionId, verdict }); }
  async cancelAsync() {
    if (this.#exclusive) throw new Error('BUSY: objective reconciliation is pending');
    await this._dispatchAsync({ op: 'cancel' });
    for (const controller of this.#controllers) controller.abort();
  }
  inspect() { return this._dispatch({ op: 'inspect' }); }
  trigger(id, schedule, callback) { return new Trigger(this, id, schedule, callback); }
  triggerAsync(id, schedule, callback) { return Trigger.open(this, id, schedule, callback); }
  async _scope(externalSignal, work, trackController) {
    checkCancelled(externalSignal);
    const controller = new AbortController();
    const abort = () => controller.abort(externalSignal.reason);
    externalSignal?.addEventListener('abort', abort, { once: true });
    if (externalSignal?.aborted) abort();
    let detach = () => {}; let untrack = () => {};
    try {
      detach = this._attach(controller, 0, true);
      if (trackController) untrack = trackController(controller);
      checkCancelled(controller.signal);
      const result = await work(controller.signal);
      checkCancelled(controller.signal);
      return result;
    } catch (error) {
      checkCancelled(controller.signal); throw error;
    } finally {
      controller.abort(); untrack(); detach(); externalSignal?.removeEventListener('abort', abort);
    }
  }
  async summarize(key, input, callback, { signal } = {}) {
    if (typeof callback !== 'function') throw new TypeError('Summary needs a callback');
    checkCancelled(signal);
    input = JSON.parse(JSON.stringify(input));
    return this._scope(signal, async scopedSignal => {
      const claim = await this._dispatchAsync({ op: 'summary_begin', key, input });
      checkCancelled(scopedSignal);
      if (claim.result) return claim.result;
      return this._execute(input, callback, { externalSignal: scopedSignal,
        ids: { summary_key: key, execution_id: claim.execution_id }, allowDelegate: true, summaryKey: key,
        finish: content => this._dispatchAsync({ op: 'summary_finish', key, execution_id: claim.execution_id, content }),
        uncertainClass: SummaryUncertain });
    });
  }
  resolveSummary({ key, executionId, content }) {
    if (this.#controllers.size) throw new Error('BUSY: stop objective executors before resolving a summary');
    return this._dispatch({ op: 'summary_finish', key, execution_id: executionId, content });
  }
  async verify(key, value, callback, { signal: externalSignal, modelReview } = {}) {
    if (typeof callback !== 'function') throw new TypeError('Verification needs a callback');
    checkCancelled(externalSignal);
    const encoded = JSON.stringify(value);
    if (encoded === undefined) throw new TypeError('Verification input must be JSON');
    const input = JSON.parse(encoded);
    const review = modelReview == null ? null : { evaluation_id: modelReview.evaluationId, justification: modelReview.justification };
    return this._scope(externalSignal, async scopedSignal => {
      const claim = await this._dispatchAsync({ op: 'verify_begin', key, input, model_review: review });
      checkCancelled(scopedSignal);
      if (claim.result) return claim.result;
      return this._execute(input, callback, { externalSignal: scopedSignal,
        ids: { verification_key: key, execution_id: claim.execution_id }, allowDelegate: !!review,
        finish: verdict => this._dispatchAsync({ op: 'verify_finish', key, execution_id: claim.execution_id, verdict }),
        uncertainClass: VerificationUncertain });
    });
  }
  async _execute(input, callback, { externalSignal, ids, finish, allowDelegate, uncertainClass, trackController, summaryKey }) {
    const controller = new AbortController();
    const signal = controller.signal;
    const abort = () => controller.abort(externalSignal.reason);
    externalSignal?.addEventListener('abort', abort, { once: true });
    if (externalSignal?.aborted) abort();
    const children = []; const runs = [];
    const cancelChildren = () => { for (const run of runs) run.cancel(); };
    let open = true;
    let detach = () => {}; let untrack = () => {}; let clearDeadline = () => {};
    try {
      detach = this._attach(controller, 0);
      await this._dispatchAsync({ op: 'check', depth: 0 });
      if (trackController) untrack = trackController(controller);
      clearDeadline = armDeadline(controller, (await this.inspectAsync()).deadline_unix_ms);
      checkCancelled(signal);
      const context = { objective_id: this.#id ?? null, ...ids };
      Object.defineProperty(context, 'signal', { value: signal });
      Object.defineProperty(context, 'delegate', { value: (agent, prompt) => {
        if (!open) throw new Error('CLOSED: callback has ended');
        if (!allowDelegate) throw new Error('VERIFIER_MODEL_DISABLED: provide modelReview evaluation evidence before delegating');
        checkCancelled(signal);
        const run = startDelegate(agent, prompt, this, 1, signal, summaryKey);
        runs.push(run); children.push(run.promise);
        return run.promise;
      } });
      const value = await callback(structuredClone(input), context);
      open = false;
      await Promise.all(children);
      const result = await finish(value);
      // A known verdict is saved before cancellation is reported to the caller.
      checkCancelled(signal);
      return result;
    } catch (error) {
      cancelChildren();
      checkCancelled(signal);
      if (error instanceof ObjectiveLimit || error instanceof uncertainClass) throw error;
      throw new uncertainClass(undefined, error);
    } finally {
      open = false;
      cancelChildren();
      await Promise.allSettled(children);
      controller.abort(); clearDeadline(); untrack(); detach();
      externalSignal?.removeEventListener('abort', abort);
    }
  }
  resolveVerification({ key, executionId, verdict }) {
    if (this.#controllers.size) throw new Error('BUSY: stop objective executors before resolving a verification');
    return this._dispatch({ op: 'verify_finish', key, execution_id: executionId, verdict });
  }
  cancel() {
    this._dispatch({ op: 'cancel' });
    for (const controller of this.#controllers) controller.abort();
  }
  _attach(controller, depth, terminal = false) {
    // Registration only. Each executor awaits its kernel check before callbacks.
    if (this.#exclusive) throw new Error('BUSY: objective reconciliation is pending');
    this.#store?._retain();
    this.#controllers.add(controller);
    return () => { this.#controllers.delete(controller); this.#store?._release(); };
  }
}
async function waitForTrigger(deadline, signal, wakers, recheck) {
  let finishWait;
  const pending = new Promise((resolve, reject) => {
    let timer; let settled = false;
    const finish = error => {
      if (settled) return; settled = true;
      clearTimeout(timer); wakers.delete(wake); signal.removeEventListener('abort', abort);
      if (error) reject(error); else resolve();
    };
    const wake = () => finish();
    finishWait = wake;
    const abort = () => { try { checkCancelled(signal); } catch (error) { finish(error); } };
    const check = () => {
      const remaining = deadline - Date.now();
      if (remaining <= 0) finish();
      else timer = setTimeout(check, Math.min(remaining, 2_147_483_647));
    };
    wakers.add(wake); signal.addEventListener('abort', abort, { once: true });
    if (signal.aborted) abort(); else check();
  });
  // A disable can complete after the previous async inspection. Register first,
  // then recheck; join that read even if cancellation has already settled pending.
  pending.catch(() => {});
  try {
    if (!(await recheck())) finishWait();
    await pending;
  } finally { finishWait(); }
}
export class Trigger {
  #objective; #id; #callback;
  #schedule; #exclusive = false;
  #controllers = new Set(); #wakers = new Set(); #watching = false;
  static async open(objective, id, schedule, callback) {
    const trigger = new Trigger(objective, id, schedule, callback, deferredLoad);
    await objective._scope(undefined, () => objective._dispatchAsync({ op: 'trigger_register', id, schedule: trigger.#schedule }));
    return trigger;
  }
  constructor(objective, id, schedule, callback, internal) {
    if (!(objective instanceof Objective)) throw new TypeError('Trigger requires an Objective');
    if (typeof callback !== 'function') throw new TypeError('Trigger requires a callback');
    let nativeSchedule;
    if (schedule?.kind === 'event') nativeSchedule = { kind: 'event' };
    else if (schedule?.kind === 'at') nativeSchedule = { kind: 'at', unix_ms: schedule.unixMs };
    else if (schedule?.kind === 'interval') nativeSchedule = { kind: 'interval', every_ms: schedule.everyMs, anchor_unix_ms: schedule.anchorUnixMs ?? null };
    else if (schedule?.kind === 'cron') nativeSchedule = { kind: 'cron', expression: schedule.expression, timezone: schedule.timezone, dom_and_dow: schedule.domAndDow ?? false };
    else throw new TypeError('Unsupported trigger schedule');
    if (internal !== deferredLoad) objective._dispatch({ op: 'trigger_register', id, schedule: nativeSchedule });
    this.#schedule = nativeSchedule;
    this.#objective = objective; this.#id = id; this.#callback = callback;
  }
  get id() { return this.#id; }
  inspect() { return this.#objective._dispatch({ op: 'trigger_inspect', id: this.#id }); }
  inspectAsync() { return this.#objective._dispatchAsync({ op: 'trigger_inspect', id: this.#id }); }
  enable() { this.#objective._dispatch({ op: 'trigger_enable', id: this.#id, enabled: true }); }
  async enableAsync() { await this.#objective._scope(undefined, () => this.#objective._dispatchAsync({ op: 'trigger_enable', id: this.#id, enabled: true })); }
  async disableAsync() {
    await this.#objective._scope(undefined, () => this.#objective._dispatchAsync({ op: 'trigger_enable', id: this.#id, enabled: false }));
    for (const wake of this.#wakers) wake();
  }
  disable() {
    this.#objective._dispatch({ op: 'trigger_enable', id: this.#id, enabled: false });
    for (const wake of this.#wakers) wake();
  }
  cancel() {
    if (!this.#controllers.size) return false;
    for (const controller of this.#controllers) controller.abort();
    return true;
  }
  #track = controller => { this.#controllers.add(controller); return () => this.#controllers.delete(controller); };
  async #execute(claim, signal) {
    if (claim === null) return null;
    if (claim.result) return claim.result;
    return this.#objective._execute(claim.input, this.#callback, { externalSignal: signal,
      ids: { trigger_id: this.#id, occurrence_key: claim.occurrence_key, execution_id: claim.execution_id },
      finish: result => this.#objective._dispatchAsync({ op: 'trigger_finish', id: this.#id,
        key: claim.occurrence_key, execution_id: claim.execution_id, result }),
      allowDelegate: true, uncertainClass: TriggerUncertain, trackController: this.#track });
  }
  async emit(key, value, { signal } = {}) {
    if (this.#exclusive) throw new Error('BUSY: trigger reconciliation is pending');
    checkCancelled(signal);
    const encoded = JSON.stringify(value);
    if (encoded === undefined) throw new TypeError('Trigger input must be JSON');
    const input = JSON.parse(encoded);
    return this.#objective._scope(signal, async scopedSignal => {
      if ((await this.inspectAsync()).schedule.kind !== 'event') throw new TypeError('emit requires an event trigger');
      checkCancelled(scopedSignal);
      const claim = await this.#objective._dispatchAsync({ op: 'trigger_emit', id: this.#id, key, input });
      checkCancelled(scopedSignal);
      return this.#execute(claim, scopedSignal);
    }, this.#track);
  }
  async tick({ signal } = {}) {
    if (this.#exclusive) throw new Error('BUSY: trigger reconciliation is pending');
    checkCancelled(signal);
    return this.#objective._scope(signal, async scopedSignal => {
      if ((await this.inspectAsync()).schedule.kind === 'event') throw new TypeError('tick requires a calendar trigger');
      checkCancelled(scopedSignal);
      const claim = await this.#objective._dispatchAsync({ op: 'trigger_tick', id: this.#id });
      checkCancelled(scopedSignal);
      return this.#execute(claim, scopedSignal);
    }, this.#track);
  }
  resolve({ occurrenceKey, executionId, result }) {
    if (this.#exclusive || this.#controllers.size) throw new Error('BUSY: stop trigger executors before resolving an occurrence');
    return this.#objective._dispatch({ op: 'trigger_finish', id: this.#id, key: occurrenceKey, execution_id: executionId, result });
  }
  async resolveAsync({ occurrenceKey, executionId, result }) {
    if (this.#exclusive || this.#controllers.size) throw new Error('BUSY: stop trigger executors before resolving an occurrence');
    this.#exclusive = true;
    try { return await this.#objective._scope(undefined, () => this.#objective._dispatchAsync({ op: 'trigger_finish', id: this.#id, key: occurrenceKey, execution_id: executionId, result })); }
    finally { this.#exclusive = false; }
  }
  async *watch({ signal: externalSignal } = {}) {
    if (this.#watching || this.#exclusive) throw new Error('BUSY: trigger already has an active watcher or reconciliation');
    checkCancelled(externalSignal);
    this.#watching = true;
    const controller = new AbortController(); const signal = controller.signal;
    const abort = () => controller.abort(externalSignal.reason);
    externalSignal?.addEventListener('abort', abort, { once: true });
    if (externalSignal?.aborted) abort();
    let detach = () => {}; let untrack = () => {}; let clearDeadline = () => {};
    try {
      detach = this.#objective._attach(controller, 0, true); untrack = this.#track(controller);
      const initial = await this.inspectAsync(); checkCancelled(signal);
      if (initial.schedule.kind === 'event') throw new TypeError('Event triggers use emit instead of watch');
      if (!initial.enabled || (initial.next_due_unix_ms === null && initial.active_occurrence === null)) return;
      await this.#objective._dispatchAsync({ op: 'check', depth: 0 });
      clearDeadline = armDeadline(controller, (await this.#objective.inspectAsync()).deadline_unix_ms);
      for (;;) {
        checkCancelled(signal);
        const receipt = await this.tick({ signal });
        if (receipt !== null) {
          const state = await this.inspectAsync();
          yield receipt;
          if (!state.enabled || state.next_due_unix_ms === null) return;
          continue;
        }
        const state = await this.inspectAsync();
        if (!state.enabled || state.next_due_unix_ms === null) return;
        await waitForTrigger(state.next_due_unix_ms, signal, this.#wakers, async () => (await this.inspectAsync()).enabled);
      }
    } catch (error) {
      checkCancelled(signal); throw error;
    } finally {
      controller.abort(); clearDeadline(); untrack(); detach(); this.#watching = false;
      externalSignal?.removeEventListener('abort', abort);
    }
  }
}
const delegationContext = Symbol('delegationContext');
const mcpClientToken = Symbol('McpClient');
/** The connection options of McpClient.connect as the native config. */
function mcpConfig(options) {
  const { transport = 'stdio', timeoutMs = 30_000,
    maxMessageBytes = 8 * 1024 * 1024, maxTools = 256, maxPages = 16, maxMessages = 128 } = options;
  let connection;
  if (transport === 'stdio') {
    if (options.url !== undefined || options.headers !== undefined) throw new TypeError('URL options require transport: http or sse');
    connection = { command: options.command, args: options.args ?? [], env: options.env ?? {}, cwd: options.cwd };
  } else if (transport === 'http' || transport === 'sse') {
    if (['command', 'args', 'env', 'cwd'].some(key => options[key] !== undefined)) throw new TypeError('Process options require transport: stdio');
    connection = { url: options.url, headers: options.headers ?? {} };
  } else throw new TypeError('Unsupported MCP transport');
  // The classic HTTP+SSE transport speaks the 2025-03-26 revision unless told otherwise.
  const protocolVersion = options.protocolVersion ?? (transport === 'sse' ? '2025-03-26' : '2025-11-25');
  return { transport, ...connection, protocol_version: protocolVersion, timeout_ms: timeoutMs,
    max_message_bytes: maxMessageBytes, max_tools: maxTools, max_pages: maxPages, max_messages: maxMessages };
}

/**
 * Several named MCP servers that can be reloaded while the application runs. `apply` takes the whole
 * desired set and reconciles it (unchanged connected servers are kept, new or changed ones are connected,
 * removed ones are closed); a server that fails is reported and the rest are unaffected. Nothing reconnects
 * or retries on its own: an uncertain call closes its server, and `reconnect` or the next `apply` starts a
 * new session for later calls. See docs/mcp.md.
 */
export class McpManager {
  #native;
  constructor() { this.#native = native().createMcpManager(); }
  /** `servers` maps a name to the options of McpClient.connect, plus `enabled: false` to keep one unconnected. */
  async apply(servers, { drainMs = 0 } = {}) {
    if (servers === null || typeof servers !== 'object' || Array.isArray(servers)) throw new TypeError('servers must be an object of name to options');
    const desired = {};
    for (const [name, { enabled, ...options }] of Object.entries(servers)) {
      desired[name] = { ...mcpConfig(options), ...(enabled === undefined ? {} : { enabled }) };
    }
    return JSON.parse(await this.#native.apply(JSON.stringify(desired), drainMs));
  }
  async reconnect(name) { return JSON.parse(await this.#native.reconnect(name)); }
  /** List one connected server's tools again: `{ server, added, removed, changed }`. `catalog` and `tools()` use the new list. */
  async refreshTools(name) { return JSON.parse(await this.#native.refreshTools(name)); }
  get status() { return JSON.parse(this.#native.status()); }
  get catalog() { return JSON.parse(this.#native.catalog()); }
  /** Executable tools for the connected servers, named `<server>__<tool>`. Take a new list after `apply` to see new tools. */
  tools({ servers, names } = {}) {
    const rows = this.catalog.filter(row => (servers === undefined || servers.includes(row.server)) && (names === undefined || names.includes(row.name)));
    return rows.map(row => ({ name: row.name, description: row.description, parameters: structuredClone(row.inputSchema),
      execute: (arguments_, context) => this.call(row.server, row.tool, arguments_, { signal: context.signal, context }) }));
  }
  async call(server, tool, arguments_, { signal, context = {} } = {}) {
    checkCancelled(signal);
    const handle = await this.#native.startCall(server, tool, JSON.stringify(arguments_), JSON.stringify(context));
    const abort = () => handle.cancel(); signal?.addEventListener('abort', abort, { once: true });
    if (signal?.aborted) abort();
    try { return JSON.parse(await handle.result()); }
    catch (error) {
      if (error.message?.startsWith('MCP_OPERATION_UNCERTAIN:')) throw new McpOperationUncertain(error.message);
      throw error;
    } finally { signal?.removeEventListener('abort', abort); handle.cancel(); }
  }
  async close() { await this.#native.close(); }
}

export class McpClient {
  #native;
  #catalog;
  constructor(handle, token) {
    if (token !== mcpClientToken) throw new TypeError('Use await McpClient.connect(options)');
    this.#native = handle; this.#catalog = JSON.parse(handle.catalog());
  }
  static async connect(options) {
    const handle = await native().connectMcp(JSON.stringify(mcpConfig(options)));
    return new McpClient(handle, mcpClientToken);
  }
  get info() { return JSON.parse(this.#native.info()); }
  get catalog() { return structuredClone(this.#catalog); }
  /** The server announced that its tool list changed. Only seen after the next exchange; `refreshTools()` lists now. */
  get toolsChanged() { return this.#native.toolsChanged(); }
  /** List the tools again: `{ added, removed, changed }` by name. `tools()` and `catalog` use the new list from now on; an Agent already created keeps the tools it was created with. */
  async refreshTools() {
    const diff = JSON.parse(await this.#native.refreshTools());
    this.#catalog = JSON.parse(this.#native.catalog());
    return diff;
  }
  listResources({ cursor, signal } = {}) { return this.#catalogRequest('resources/list', cursor === undefined ? {} : { cursor }, signal); }
  readResource(uri, { signal } = {}) { return this.#catalogRequest('resources/read', { uri }, signal); }
  listPrompts({ cursor, signal } = {}) { return this.#catalogRequest('prompts/list', cursor === undefined ? {} : { cursor }, signal); }
  getPrompt(name, arguments_ = {}, { signal } = {}) { return this.#catalogRequest('prompts/get', { name, arguments: arguments_ }, signal); }
  async #catalogRequest(method, params, signal) {
    checkCancelled(signal);
    const handle = await this.#native.startCatalogRequest(method, JSON.stringify(params));
    const abort = () => handle.cancel(); signal?.addEventListener('abort', abort, { once: true });
    if (signal?.aborted) abort();
    try { return JSON.parse(await handle.result()); }
    catch (error) {
      if (error.message?.startsWith('MCP_OPERATION_UNCERTAIN:')) throw new McpOperationUncertain(error.message);
      throw error;
    } finally { signal?.removeEventListener('abort', abort); handle.cancel(); }
  }
  tools({ prefix = 'mcp_', names } = {}) {
    if (typeof prefix !== 'string') throw new TypeError('MCP tool prefix must be a string');
    const selected = names ?? this.#catalog.map(tool => tool.name);
    if (new Set(selected).size !== selected.length) throw new TypeError('MCP tool names must be unique');
    return selected.map(name => {
      const tool = this.#catalog.find(tool => tool.name === name);
      if (!tool) throw new Error('MCP_ERROR: unknown selected tool');
      return { name: prefix + name, description: tool.description, parameters: structuredClone(tool.inputSchema),
        execute: (arguments_, context) => this.call(name, arguments_, { signal: context.signal, context }) };
    });
  }
  async call(name, arguments_, { signal, context = {} } = {}) {
    checkCancelled(signal);
    const handle = await this.#native.startCall(name, JSON.stringify(arguments_), JSON.stringify(context));
    const abort = () => handle.cancel(); signal?.addEventListener('abort', abort, { once: true });
    if (signal?.aborted) abort();
    try { return JSON.parse(await handle.result()); }
    catch (error) {
      if (error.message?.startsWith('MCP_OPERATION_UNCERTAIN:')) throw new McpOperationUncertain(error.message);
      throw error;
    } finally { signal?.removeEventListener('abort', abort); handle.cancel(); }
  }
  async close() { await this.#native.close(); }
}

function cancelled() { return Object.assign(new Error('CANCELLED: execution was cancelled'), { code: 'CANCELLED' }); }
function deadlineExceeded() { return Object.assign(new Error('DEADLINE_EXCEEDED: run deadline elapsed'), { code: 'DEADLINE_EXCEEDED' }); }
function checkCancelled(signal, terminal = false) {
  if (!signal?.aborted) return;
  if (signal.reason?.code === 'DEADLINE_EXCEEDED') { if (!terminal) throw deadlineExceeded(); }
  else throw cancelled();
}
function isTerminal(action) { return action.type === 'complete' || action.type === 'stopped'; }
function armDeadline(controller, deadline) {
  if (deadline == null) return () => {};
  let timer;
  const check = () => {
    const remaining = deadline - Date.now();
    if (remaining <= 0) { if (!controller.signal.aborted) controller.abort(deadlineExceeded()); }
    else timer = setTimeout(check, Math.min(remaining, 2_147_483_647));
  };
  check();
  return () => clearTimeout(timer);
}

export class Agent {
  static async open(options) {
    const agent = new Agent(options, deferredLoad);
    if (agent.#loadOptions) {
      const { config, resume, agentId, sessionId } = agent.#loadOptions;
      agent.#store._retain();
      try {
        agent.#durable = await agent.#store._openAgentAsync(agentId, sessionId, config, resume);
        agent.#loaded(await agent.#persist({ op: 'frame' }), resume);
      } finally { agent.#store._release(); agent.#loadOptions = undefined; }
    }
    return agent;
  }
  static {
    delegateAgent = (agent, prompt, objective, depth, signal, summaryKey) => {
      if (!(agent instanceof Agent)) throw new TypeError('delegate requires an Agent');
      if (agent.#busy) throw new Error('BUSY: child agent already has an active run');
      return agent.#collect(agent.#drive(prompt, false, false, { signal, [delegationContext]: { objective, depth, summaryKey } }));
    };
  }
  #frame;
  #provider;
  #tools;
  #busy = false;
  #closed = false;
  #onEvent;
  #store;
  #session;
  #revision = 0;
  #resumePending = false;
  #controller;
  #objective;
  #resumeDepth = 0;
  #resumeSummaryKey;
  #engine;
  #durable;
  #loadOptions;
  #toolPolicy;
  #hooks;

  #loaded(loaded, resume) {
    this.#revision = loaded.revision; this.#frame = loaded; this.#resumePending = resume;
    this.#resumeDepth = loaded.objective_depth ?? 0; this.#resumeSummaryKey = loaded.summary_key;
  }

  #memory(command) { return JSON.parse(this.#engine.dispatch(JSON.stringify(command))); }
  #query(command) {
    try { return this.#durable ? JSON.parse(this.#durable.dispatch(JSON.stringify(command))) : this.#memory(command); }
    catch (error) { if (error.message?.startsWith('OBJECTIVE_LIMIT:')) throw new ObjectiveLimit(error.message); throw error; }
  }
  /**
   * Decide whether a tool call may run: the declarative policy (evaluated natively), then the
   * beforeTool hook, then human approval when either says `ask`. The strictest answer wins, and any
   * hook failure denies. Runs before the operation is claimed, so waiting for approval never leaves
   * a claimed-but-unrun operation behind if the process dies.
   */
  async #authorize(call, signal) {
    if (!this.#toolPolicy && !this.#hooks) return { allowed: true };
    const rank = { allow: 0, ask: 1, deny: 2 };
    let decision = 'allow', reason = null;
    const tighten = (next, why) => { if (rank[next] > rank[decision]) { decision = next; reason = why; } };
    const raced = promise => new Promise((resolve, reject) => {
      if (signal?.aborted) return reject(new Error('CANCELLED'));
      const abort = () => reject(new Error('CANCELLED'));
      signal?.addEventListener('abort', abort, { once: true });
      Promise.resolve(promise).then(resolve, reject).finally(() => signal?.removeEventListener('abort', abort));
    });
    if (this.#toolPolicy) {
      const verdict = JSON.parse(native().evaluateToolPolicy(this.#toolPolicy, call.name, JSON.stringify(call.arguments ?? {})));
      tighten(verdict.decision, verdict.reason);
    }
    if (this.#hooks?.beforeTool) {
      try {
        const answer = await raced(this.#hooks.beforeTool({ call: structuredClone(call), verdict: { decision, reason }, signal }));
        if (answer?.decision !== undefined) {
          if (!(answer.decision in rank)) tighten('deny', 'POLICY_HOOK_FAILED: beforeTool returned an unknown decision');
          else tighten(answer.decision, answer.reason ?? 'beforeTool hook');
        }
      } catch (error) {
        if (signal?.aborted) throw error;
        tighten('deny', 'POLICY_HOOK_FAILED: beforeTool threw');
      }
    }
    if (decision === 'ask') {
      if (!this.#hooks?.approve) {
        decision = 'deny'; reason = `APPROVAL_UNAVAILABLE: ${reason ?? 'approval required'} (no approve hook is configured)`;
      } else {
        let approved = false, why = null;
        try {
          const answer = await raced(this.#hooks.approve({ call: structuredClone(call), reason, signal }));
          approved = answer === true || answer?.approved === true;
          why = typeof answer === 'object' ? answer?.reason ?? null : null;
        } catch (error) {
          if (signal?.aborted) throw error;
          why = 'APPROVAL_FAILED: the approval hook threw';
        }
        if (approved) decision = 'allow';
        else { decision = 'deny'; reason = why ?? `approval refused: ${reason ?? 'approval required'}`; }
      }
    }
    return { allowed: decision === 'allow', reason };
  }

  async #persist(command) {
    try { return JSON.parse(await this.#durable.dispatchAsync(JSON.stringify(command))); }
    catch (error) { if (error.message?.startsWith('OBJECTIVE_LIMIT:')) throw new ObjectiveLimit(error.message); throw error; }
  }

  constructor({ model, system, tools = [], provider, maxTurns = 8, policy = {}, onEvent,
    store, agentId, sessionId, resume = false, objective, toolPolicy, hooks }, internal) {
    if (toolPolicy !== undefined) {
      // Validate now so a bad policy fails at construction, not mid-run.
      native().evaluateToolPolicy(JSON.stringify(toolPolicy), '_', '{}');
      this.#toolPolicy = JSON.stringify(toolPolicy);
    }
    if (hooks !== undefined) {
      for (const name of Object.keys(hooks)) {
        if (!['beforeTool', 'afterTool', 'approve'].includes(name)) throw new TypeError(`Unknown hook ${name}`);
        if (typeof hooks[name] !== 'function') throw new TypeError(`Hook ${name} must be a function`);
      }
      this.#hooks = { ...hooks };
    }
    if (objective !== undefined && !(objective instanceof Objective)) throw new TypeError('objective must be an Objective');
    objective?._validateStore(store);
    this.#objective = objective;
    this.#tools = new Map(tools.map(tool => [tool.name, tool]));
    for (const tool of tools) {
      if (typeof tool.execute !== 'function') throw new TypeError(`Tool ${tool.name} needs execute`);
    }
    this.#provider = provider ?? new OpenAICompatible();
    if (typeof this.#provider.complete !== 'function' && typeof this.#provider.stream !== 'function') {
      throw new TypeError('Provider needs complete or stream');
    }
    this.#onEvent = onEvent;
    const config = { op: 'create', model, system, max_turns: maxTurns, policy, objective_id: objective?.id,
      tools: tools.map(({ name, description, parameters, allowRepeatedResults = false }) =>
        ({ name, description, parameters, allow_repeated_results: allowRepeatedResults })) };
    if (store === undefined) {
      this.#engine = new (native().NativeEngine)(JSON.stringify(config));
      this.#frame = this.#memory({ op: 'frame' });
    }
    if (store !== undefined) {
      if (typeof agentId !== 'string' || typeof sessionId !== 'string') {
        throw new TypeError('Persistent agents require explicit agentId and sessionId');
      }
      if (!(store instanceof SessionStore)) throw new TypeError('store must be a SessionStore');
      this.#store = store;
      this.#session = { agent_id: agentId, session_id: sessionId };
      if (internal === deferredLoad) { this.#loadOptions = { config, resume, agentId, sessionId }; return; }
      this.#durable = store._openAgent(agentId, sessionId, config, resume);
      this.#loaded(this.#query({ op: 'frame' }), resume);
    } else if (agentId !== undefined || sessionId !== undefined || resume) {
      throw new TypeError('agentId and sessionId require a SessionStore');
    }
  }

  get messages() { return this.#query({ op: 'messages' }); }
  get contextPlan() { return this.#query({ op: 'context_plan' }); }

  #checkContext() {
    if (this.#closed) throw new Error('CLOSED: agent is closed');
    if (this.#busy) throw new Error('BUSY: cannot inspect or change context during an active run');
  }
  captureContext(start, end) {
    this.#checkContext();
    return this.#query({ op: 'capture_context', start, end });
  }
  readContext(source) {
    this.#checkContext();
    return this.#query({ op: 'read_context', source });
  }
  searchContext(query, { maxTurns, maxBytes } = {}) {
    this.#checkContext();
    const command = { op: 'search_context', query, options: { max_turns: maxTurns, max_bytes: maxBytes } };
    return this.#query(command);
  }
  setContextPlan(plan) {
    this.#checkContext();
    if (this.#resumePending) throw new Error('INVALID_STATE: context cannot change while a checkpoint awaits resume');
    this.#store?._retain();
    try {
      this.#frame = this.#query({ op: 'set_context_plan', plan });
      if (this.#store) this.#revision = this.#frame.revision;
    } finally { this.#store?._release(); }
  }
  async setContextPlanAsync(plan) {
    this.#checkContext();
    if (this.#resumePending) throw new Error('INVALID_STATE: context cannot change while a checkpoint awaits resume');
    this.#store?._retain(); this.#busy = true;
    try {
      this.#frame = this.#store ? await this.#persist({ op: 'set_context_plan', plan }) : this.#memory({ op: 'set_context_plan', plan });
      if (this.#store) this.#revision = this.#frame.revision;
    } finally { this.#busy = false; this.#store?._release(); }
  }
  async summarize(key, source, callback, { objective = this.#objective, signal: externalSignal } = {}) {
    this.#checkContext();
    if (this.#resumePending) throw new Error('INVALID_STATE: summary generation requires finishing the pending checkpoint');
    if (!(objective instanceof Objective)) throw new TypeError('Summary generation requires an Objective');
    if (this.#objective && !this.#objective._same(objective)) throw new ObjectiveLimit('OBJECTIVE_LIMIT: summary cannot change objective');
    objective._validateStore(this.#store);
    if (source.start !== this.messages.findIndex(message => message.role === 'user')) throw new TypeError('Summary source must start at the first user turn');
    const input = { source, messages: this.readContext(source) };
    checkCancelled(externalSignal);
    const controller = new AbortController();
    const abort = () => controller.abort(externalSignal.reason);
    externalSignal?.addEventListener('abort', abort, { once: true });
    if (externalSignal?.aborted) abort();
    this.#busy = true; this.#controller = controller;
    try { return await objective.summarize(key, input, callback, { signal: controller.signal }); }
    finally { controller.abort(); externalSignal?.removeEventListener('abort', abort); this.#busy = false; this.#controller = undefined; }
  }

  async run(prompt, options = {}) {
    return this.#collect(this.#drive(prompt, false, false, options));
  }
  _runQueued(prompt, { store, objectiveId, depth, signal, resume = false, binding }) {
    if (!objectiveId || this.#store !== store || this.#objective?.id !== objectiveId) {
      throw new ObjectiveLimit('OBJECTIVE_LIMIT: queued agents must bind the job root and SessionStore');
    }
    if (binding && (this.#session?.agent_id !== binding.agent_id || this.#session?.session_id !== binding.session_id)) {
      throw new Error('DELEGATION_BINDING_MISMATCH: bind the assigned specialist and durable session');
    }
    return this.#collect(this.#drive(prompt, resume, false, { signal, [delegationContext]: { objective: this.#objective, depth } }));
  }

  async resume(options = {}) {
    return this.#collect(this.#drive(undefined, true, false, options));
  }

  stream(prompt, options = {}) { return this.#drive(prompt, false, true, options); }
  resumeStream(options = {}) { return this.#drive(undefined, true, true, options); }
  cancel() {
    if (!this.#busy || !this.#controller) return false;
    this.#controller.abort();
    return true;
  }

  async #collect(events) {
    let result;
    for await (const event of events) { if (event.type === 'result') result = event.result; }
    if (!result) throw new Error('INVALID_STATE: execution ended without a result');
    return result;
  }

  async *#drive(prompt, resume, streaming, { signal: externalSignal, [delegationContext]: inherited } = {}) {
    if (this.#closed) throw new Error('CLOSED: agent is closed');
    if (this.#busy) throw new Error('BUSY: agent already has an active run');
    if (resume !== this.#resumePending) throw new Error('INVALID_STATE: use resume() only with an agent opened with resume: true');
    checkCancelled(externalSignal);
    const objective = inherited?.objective ?? this.#objective;
    const depth = Math.max(inherited?.depth ?? 0, resume ? this.#resumeDepth : 0);
    const summaryKey = inherited?.summaryKey ?? (resume ? this.#resumeSummaryKey : undefined);
    if (inherited && this.#objective && !this.#objective._same(objective)) throw new ObjectiveLimit('OBJECTIVE_LIMIT: child cannot change objective');
    objective?._validateStore(this.#store);
    if (this.#store && objective?.id !== this.#frame.state.objective_id) {
      // JSON omits an absent objective id.
      if (objective?.id != null || this.#frame.state.objective_id != null) throw new ObjectiveLimit('OBJECTIVE_LIMIT: persistent child must bind the inherited objective at construction');
    }
    const terminalResume = resume && isTerminal(this.#frame.action);
    const controller = new AbortController();
    const signal = controller.signal;
    const abort = () => controller.abort();
    externalSignal?.addEventListener('abort', abort, { once: true });
    if (externalSignal?.aborted) abort();
    this.#controller = controller;
    this.#busy = true;
    let claimed = false;
    let clearDeadline = () => {};
    let detachObjective = () => {};
    try {
      if (objective) {
        detachObjective = objective._attach(controller, depth, terminalResume);
        if (!terminalResume) await objective._dispatchAsync({ op: 'check', depth });
        if (!terminalResume && summaryKey !== undefined) await objective._dispatchAsync({ op: 'summary_check', key: summaryKey, depth });
      }
      checkCancelled(signal);
      let frame;
      if (this.#store) {
        this.#store._retain();
        claimed = true;
        frame = await this.#persist(resume ? { op: 'resume_run' } : { op: 'begin_run', prompt, objective_depth: depth, summary_key: summaryKey });
        this.#revision = frame.revision;
      } else { frame = this.#memory({ op: 'start', prompt }); }
      this.#resumePending = false;
      if (!isTerminal(frame.action)) {
        const deadlines = [frame.state.deadline_unix_ms, (objective ? (await objective.inspectAsync()).deadline_unix_ms : undefined)].filter(value => value != null);
        clearDeadline = armDeadline(controller, deadlines.length ? Math.min(...deadlines) : undefined);
      }
      const preflight = async () => {
        if (frame.state.deadline_unix_ms == null || isTerminal(frame.action)) return;
        frame = this.#store ? await this.#persist({ op: 'check' }) : this.#memory({ op: 'tick' });
        if (this.#store) this.#revision = frame.revision;
      };
      for (;;) {
        if (objective && (!this.#store || !isTerminal(frame.action))) await objective._dispatchAsync({ op: 'check', depth });
        await preflight();
        let action = frame.action;
        if (isTerminal(action)) clearDeadline();
        checkCancelled(signal, isTerminal(action));
        await this.#onEvent?.(structuredClone(action));
        await preflight();
        if (frame.action.type !== action.type) continue;
        action = frame.action;
        checkCancelled(signal, isTerminal(action));
        yield structuredClone(action);
        await preflight();
        if (frame.action.type !== action.type) continue;
        action = frame.action;
        checkCancelled(signal, isTerminal(action));
        if (action.type === 'complete' || action.type === 'stopped') {
          if (this.#store) {
            this.#revision = await this.#persist({ op: 'commit' });
          } else { this.#memory({ op: 'commit' }); }
          this.#frame = frame;
          const result = { status: action.type === 'complete' ? 'completed' : 'stopped',
            content: action.content ?? null, stop_reason: action.reason ?? null, stats: action.stats };
          yield { type: 'result', result };
          return;
        }
        if (action.type === 'model') {
          const permit = !this.#store && objective ? await objective._dispatchAsync({ op: summaryKey === undefined ? 'model' : 'summary_model', key: summaryKey, depth, cap: action.request.max_output_tokens ?? null }) : undefined;
          if (this.#store) {
            frame = await this.#persist({ op: 'claim' });
            this.#revision = frame.revision;
            if (frame.action.type !== action.type) continue;
            action = frame.action;
          }
          checkCancelled(signal);
          const request = structuredClone(action.request);
          if (permit) {
            request.max_output_tokens = permit.max_output_tokens;
            if (permit.deadline_unix_ms != null) request.deadline_unix_ms = Math.min(request.deadline_unix_ms ?? Infinity, permit.deadline_unix_ms);
          }
          let reply;
          if (typeof this.#provider.stream === 'function' && (streaming || typeof this.#provider.complete !== 'function')) {
            for await (const event of this.#provider.stream(request, { signal })) {
              checkCancelled(signal);
              if (reply) throw new Error('PROVIDER_ERROR: events after final reply');
              if (event.type === 'reply') reply = event.reply;
              else if (['text_delta', 'tool_call_delta', 'usage'].includes(event.type)) yield structuredClone(event);
              else throw new Error('PROVIDER_ERROR: unknown stream event');
            }
            if (!reply) throw new Error('PROVIDER_ERROR: stream ended without a model reply');
          } else { reply = await this.#provider.complete(request, { signal }); }
          if (permit) await objective._dispatchAsync({ op: 'settle', id: permit.id, usage: reply.usage ?? null });
          frame = this.#store ? await this.#persist({ op: 'advance', execution_id: frame.operation.execution_id,
            outcome: { type: 'model', reply } }) : this.#memory({ op: 'reply', reply });
          if (this.#store) this.#revision = frame.revision;
        } else if (action.type === 'tool_rejected') {
          if (!this.#store && objective) await objective._dispatchAsync({ op: summaryKey === undefined ? 'tool' : 'summary_tool', key: summaryKey, depth });
          frame = this.#store ? await this.#persist({ op: 'reject' }) : this.#memory({ op: 'reject' });
          if (this.#store) this.#revision = frame.revision;
        } else if (action.type === 'tool') {
          const authorization = await this.#authorize(action.call, signal);
          checkCancelled(signal);
          if (!this.#store && objective) await objective._dispatchAsync({ op: summaryKey === undefined ? 'tool' : 'summary_tool', key: summaryKey, depth });
          if (this.#store) {
            frame = await this.#persist({ op: 'claim' });
            this.#revision = frame.revision;
            if (frame.action.type !== action.type) continue;
            action = frame.action;
          }
          checkCancelled(signal);
          let result;
          let acceptingChildren = true;
          const children = [];
          const childRuns = [];
          try {
            const context = {
              run_id: frame.run_id ?? null, execution_id: frame.operation?.execution_id ?? null,
              tool_call_id: action.call.id,
            };
            Object.defineProperty(context, 'signal', { value: signal });
            Object.defineProperty(context, 'delegate', { value: (child, prompt) => {
              if (!acceptingChildren) throw new Error('CLOSED: tool invocation has ended');
              if (!objective) throw new ObjectiveLimit('OBJECTIVE_LIMIT: delegation requires an Objective');
              if (!(child instanceof Agent)) throw new TypeError('delegate requires an Agent');
              if (child.#busy) throw new Error('BUSY: child agent already has an active run');
              const run = startDelegate(child, prompt, objective, depth + 1, signal, summaryKey);
              children.push(run.promise); childRuns.push(run);
              return run.promise;
            } });
            if (!authorization.allowed) {
              // Denied: the tool is not run; the model is told why, like any tool error.
              result = { error: { message: authorization.reason?.startsWith('POLICY_') || authorization.reason?.startsWith('APPROVAL_')
                ? authorization.reason : `POLICY_DENIED: ${authorization.reason ?? 'not allowed'}` } };
            } else {
              result = await this.#tools.get(action.call.name).execute(action.call.arguments, context);
              acceptingChildren = false;
              await Promise.all(children);
              // Ensure host values can cross the shared JSON contract before accepting them.
              if (JSON.stringify(result) === undefined) throw new TypeError('Tool must return a JSON value');
              result = JSON.parse(JSON.stringify(result));
              if (this.#hooks?.afterTool) {
                // The tool already ran. If the filter fails its output is withheld rather than passed on unchecked.
                try {
                  const filtered = await this.#hooks.afterTool({ call: structuredClone(action.call), result: structuredClone(result), signal });
                  if (filtered && Object.hasOwn(filtered, 'result')) {
                    if (JSON.stringify(filtered.result) === undefined) throw new TypeError('afterTool must return a JSON result');
                    result = JSON.parse(JSON.stringify(filtered.result));
                  }
                } catch (error) {
                  checkCancelled(signal);
                  result = { error: { message: 'POLICY_HOOK_FAILED: afterTool failed; the tool ran but its output was withheld' } };
                }
              }
            }
          } catch (error) {
            for (const run of childRuns) run.cancel();
            if (error instanceof ToolOperationUncertain || error instanceof ObjectiveLimit) throw error;
            if (children.length) throw new ToolOperationUncertain('DELEGATION_FAILED: inspect child outcomes before continuing');
            checkCancelled(signal);
            result = { error: { message: error instanceof Error ? error.message : String(error) } };
          } finally {
            acceptingChildren = false;
            if (children.length) {
              // A failed parent must cancel and join children before releasing the run.
              await Promise.allSettled(children);
            }
          }
          frame = this.#store ? await this.#persist({ op: 'advance', execution_id: frame.operation.execution_id,
            outcome: { type: 'tool', result } }) : this.#memory({ op: 'tool_result', id: action.call.id, result });
          if (this.#store) this.#revision = frame.revision;
        } else { throw new Error(`INVALID_STATE: unexpected action ${action.type}`); }
      }
    } catch (error) {
      checkCancelled(signal);
      throw error;
    } finally {
      controller.abort();
      clearDeadline();
      detachObjective();
      externalSignal?.removeEventListener('abort', abort);
      if (claimed) this.#store._release();
      if (this.#engine) this.#memory({ op: 'abort' });
      this.#busy = false;
      this.#controller = undefined;
    }
  }

  close() {
    if (this.#busy) throw new Error('BUSY: cannot close an active agent');
    this.#closed = true;
  }
}

/** Native CDP page connection. A browser process must already be running. */
export class Browser {
  #native;
  static async connect(pageWebSocketUrl, { timeoutMs = 10000 } = {}) {
    return new Browser(await native().connectBrowser(pageWebSocketUrl, timeoutMs));
  }
  constructor(handle) { this.#native = handle; }
  async #call(op, fields = {}) { return JSON.parse(await this.#native.dispatch(JSON.stringify({ ...fields, op }))); }
  navigate(url) { return this.#call('navigate', { url }); }
  evaluate(expression) { return this.#call('evaluate', { expression }); }
  click(selector) { return this.#call('click', { selector }); }
  typeText(selector, text) { return this.#call('type_text', { selector, text }); }
  extract(selector = 'body') { return this.#call('extract', { selector }); }
  screenshot() { return this.#call('screenshot'); }
  cancel() { this.#native.cancel(); }
  async close() { await this.#native.close(); }
}

/**
 * Sandboxed filesystem, command and web tools. Command and web tools exist only
 * when their policy is configured; nothing runs through a shell.
 */
/** Bounded artifact store: bytes under `root`, metadata in the tenant's HiveDB. See docs/artifacts.md. */
export class ArtifactStore {
  #store;
  #root;
  constructor(store, root) {
    if (typeof root !== 'string' || root === '') throw new TypeError('ArtifactStore needs a root directory');
    this.#store = store; this.#root = root;
  }
  #call(command) { return this.#store._dispatchAsync({ op: 'artifacts', root: this.#root, command }); }
  definitions() { return this.#call({ op: 'tool_definitions' }); }
  execute(name, arguments_) { return this.#call({ op: 'execute_tool', name, arguments: arguments_ }); }
  /** Agent-ready tools: artifact_save/inspect/read/list/delete, image_info and artifact_transform_image. */
  async createTools() {
    return (await this.definitions()).map(definition => ({ ...definition, execute: arguments_ => this.execute(definition.name, arguments_) }));
  }
  /** Removes the content of every active artifact whose retention elapsed; returns their ids. */
  async expire({ now } = {}) { return (await this.#call({ op: 'expire', now })).expired; }
}

/**
 * Opt-in decision oracle (Jev/Kev protocol). Advisory only: a failure returns a
 * `fallback` reply and never throws. With an `objective`, each call is reserved
 * before it is sent and its usage settled afterwards, like any model call.
 */
export class Oracle {
  #native;
  #cap;
  constructor(config) {
    this.#cap = config.max_output_tokens ?? 64;
    this.#native = new (native().NativeOracle)(JSON.stringify(config));
  }
  async ask(state, questions, { objective, depth = 0, signal } = {}) {
    if (signal?.aborted) throw new Error('CANCELLED');
    let permit = null;
    if (objective && Object.keys(questions).length > 0 && !this.status().cooling_down) {
      permit = await objective._dispatchAsync({ op: 'model', depth, cap: this.#cap });
    }
    const abort = () => this.#native.cancel();
    signal?.addEventListener('abort', abort, { once: true });
    let reply;
    try { reply = JSON.parse(await this.#native.ask(JSON.stringify({ state, questions }))); }
    finally { signal?.removeEventListener('abort', abort); }
    if (permit && reply.outcome === 'decision') await objective._dispatchAsync({ op: 'settle', id: permit.id, usage: reply.charge ?? null });
    return reply;
  }
  status() { return JSON.parse(this.#native.status()); }
  recordOverruled(reason) { return this.#native.recordOverruled(reason); }
  recordAgreement() { this.#native.recordAgreement(); }
  reset() { this.#native.reset(); }
  cancel() { this.#native.cancel(); }
}

export class NativeTools {
  #native;
  #active = new Set();
  constructor(config) {
    this.#native = new (native().NativeTools)(JSON.stringify(config));
    // A Node process that exits does not drop native objects, so an Obscura
    // process the tools started would outlive it; stop it when the process exits.
    if (config.obscura) process.once('exit', () => this.#native.closeNow());
  }
  async #call(command, signal) {
    if (signal?.aborted) throw new Error('CANCELLED');
    const request = this.#native.fork();
    const abort = () => request.cancel();
    this.#active.add(request);
    signal?.addEventListener('abort', abort, { once: true });
    try { return JSON.parse(await request.dispatch(JSON.stringify(command))); }
    finally { this.#active.delete(request); signal?.removeEventListener('abort', abort); }
  }
  definitions() { return this.#call({ op: 'tool_definitions' }); }
  execute(name, arguments_) { return this.#call({ op: 'execute_tool', name, arguments: arguments_ }); }
  /** Agent-ready tools: definitions plus bound executors. */
  async createTools() {
    return (await this.definitions()).map(definition => ({ ...definition, execute: (arguments_, context) =>
      this.#call({ op: 'execute_tool', name: definition.name, arguments: arguments_ }, context?.signal) }));
  }
  /**
   * Downloads and installs the latest Obscura into `obscura.install_dir` (`~/.hive/bin` by default) after
   * checking its published SHA-256. Keeps an existing install unless `update` is true.
   */
  installObscura({ update = false } = {}) { return this.#call({ op: 'install_obscura', update }); }
  /** Aborts calls in flight; later calls are unaffected. */
  cancel() { for (const request of this.#active) request.cancel(); }
  /** Host kill switch for the computer_* tools: interrupts an action in progress and refuses later ones until `computerResume()`. */
  computerStop() { return this.#call({ op: 'computer_stop' }); }
  computerResume() { return this.#call({ op: 'computer_resume' }); }
  /** Gives the computer_* tools a fresh action budget. */
  computerReset() { return this.#call({ op: 'computer_reset' }); }
  computerStatus() { return this.#call({ op: 'computer_status' }); }
  /** Stops the Obscura session the obscura_* tools started, if any. The next call starts a new one. */
  async close() { await this.#native.close(); }
}
