export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export interface ToolCall { id: string; name: string; arguments: Record<string, Json> }
export interface ProviderState { provider: 'gemini'; version: 1; parts: Record<string, Json>[] }
export interface ToolDefinition {
  name: string;
  description: string;
  parameters: Record<string, Json>;
}
export interface Tool extends ToolDefinition {
  allowRepeatedResults?: boolean;
  execute(args: Record<string, Json>, context: ToolContext): Json | Promise<Json>;
}
export interface ToolContext {
  run_id: string | null; execution_id: string | null; tool_call_id: string; readonly signal: AbortSignal;
  delegate(child: Agent, prompt: string): Promise<RunResult>;
}
export interface ModelTool extends ToolDefinition { allow_repeated_results: boolean }
export interface Message {
  role: string;
  content: string | null;
  tool_calls?: ToolCall[];
  tool_call_id?: string;
  provider_state?: ProviderState;
}
export type TokenEncoding = 'cl100k_base' | 'o200k_base';
export type InputTokenFraming = 'neutral_json' | 'openai_chat_json' | 'anthropic_messages_json' | 'gemini_generate_content_json';
export interface InputTokenBudget {
  encoding: TokenEncoding; max_tokens: number; overhead_tokens?: number;
  framing?: InputTokenFraming; max_total_tokens?: number | null; context_window_tokens?: number | null;
  /** Tokens reserved per attachment (added to the estimate's overhead). Required to send attachments under a token budget. */
  attachment_tokens?: number | null;
}
export type AttachmentKind = 'image' | 'audio' | 'document';
/** Inline base64 `data` or a public `url` (exactly one). Providers accept different subsets; see docs/multimodal.md. */
export interface Attachment { kind: AttachmentKind; media_type: string; data?: string; url?: string; name?: string }
/** A user turn: plain text, or text plus attachments (the text stays required). */
export type Prompt = string | { text: string; attachments?: Attachment[] };
export interface TokenEstimate { encoding: TokenEncoding; framing: InputTokenFraming; serialized_tokens: number; overhead_tokens: number; total_tokens: number }
export interface ContextSource { start: number; end: number; sha256: string }
export interface ContextSummary { id: string; source: ContextSource; content: string }
export interface ContextPlan { summary?: ContextSummary | null; retrieved?: ContextSource[] }
export interface ContextHit { source: ContextSource; score: number; messages: Message[] }
export interface ContextSearchResult { hits: ContextHit[]; serialized_bytes: number }
export interface ContextReport {
  serialized_bytes: number; dropped_messages: number; token_estimate?: TokenEstimate;
  summary_id?: string; retrieved_sources?: ContextSource[];
}
export interface StructuredOutput { name: string; schema: Record<string, Json> }
export interface ModelRequest { model: string; messages: Message[]; tools: ModelTool[]; max_output_tokens?: number; deadline_unix_ms?: number; context?: ContextReport; structured_output?: StructuredOutput }
export interface ModelReply { content: string | null; tool_calls?: ToolCall[]; usage?: { input_tokens: number; output_tokens: number }; provider_state?: ProviderState }
export interface ProviderContext { signal: AbortSignal }
export type ModelStreamEvent = { type: 'text_delta'; text: string }
  | { type: 'tool_call_delta'; index: number; id: string | null; name: string | null; arguments: string }
  | { type: 'usage'; usage: { input_tokens: number; output_tokens: number } }
  | { type: 'reply'; reply: ModelReply };
export interface Provider {
  complete?(request: ModelRequest, context: ProviderContext): ModelReply | Promise<ModelReply>;
  stream?(request: ModelRequest, context: ProviderContext): AsyncIterable<ModelStreamEvent>;
}
export interface ValidationIssue { instance_path: string; schema_path: string; message: string }
export interface ToolValidationFeedback {
  error: { code: 'INVALID_TOOL_ARGUMENTS'; tool: string; message: string; issues: ValidationIssue[]; truncated: boolean };
}
export type Action = { type: 'idle' }
  | { type: 'model'; request: ModelRequest }
  | { type: 'tool'; call: ToolCall }
  | { type: 'tool_rejected'; call: ToolCall; result: ToolValidationFeedback }
  | { type: 'complete'; content: string; stats: RunStats }
  | { type: 'stopped'; reason: StopReason; stats: RunStats };
export type StopReason = 'model_call_limit' | 'tool_call_limit' | 'repeated_tool_result' | 'token_limit' | 'usage_unavailable' | 'output_limit' | 'deadline_exceeded' | 'context_limit' | 'input_limit' | 'invalid_structured_output';
export interface RunStats {
  model_calls: number; tool_calls: number; tool_rejections: number; input_tokens: number;
  output_tokens: number; unmetered_model_calls: number;
  reserved_output_tokens: number;
  reserved_input_tokens: number; estimated_input_tokens: number; charged_input_tokens: number;
}
export interface LoopPolicy {
  max_tool_calls?: number;
  max_identical_tool_results?: number | null;
  /** Tools whose result may carry a picture (`mime_type: image/*` plus base64 `data`, as computer_screenshot returns). The model is shown it as an image after the tool results instead of as base64 text. They must be tools of the agent. */
  image_tools?: string[];
  /** How many of those pictures stay in the context (1 to 16, default 2); older ones are replaced by a short note. */
  max_tool_images?: number;
  max_total_tokens?: number | null;
  max_output_tokens?: number | null;
  max_total_output_tokens?: number | null;
  max_run_ms?: number | null;
  max_context_bytes?: number | null;
  context_overflow?: 'stop' | 'drop_oldest_turns';
  input_token_budget?: InputTokenBudget | null;
  structured_output?: StructuredOutput | null;
}
export interface RunResult {
  status: 'completed' | 'stopped'; content: string | null;
  stop_reason: StopReason | null; stats: RunStats;
}
export interface RunOptions { signal?: AbortSignal }
export type AgentStreamEvent = Action | Exclude<ModelStreamEvent, {type: 'reply'}> | { type: 'result'; result: RunResult };
/** Declarative tool policy, evaluated natively: first matching rule wins, else `default` (allow). See docs/policy.md. */
export interface ToolPolicyCondition { arg: string; op: 'equals' | 'not_equals' | 'prefix' | 'suffix' | 'contains' | 'one_of' | 'exists'; value?: Json }
export interface ToolPolicyRule { tools: string[]; decision: 'allow' | 'deny' | 'ask'; reason?: string; when?: ToolPolicyCondition[] }
export interface ToolPolicy { default?: 'allow' | 'deny' | 'ask'; default_reason?: string; rules?: ToolPolicyRule[] }
export type ToolDecision = 'allow' | 'deny' | 'ask';
export interface AgentHooks {
  /** Before a tool runs, with the policy's verdict. Return {decision, reason} to tighten it (never to loosen). Throwing denies. */
  beforeTool?(context: { call: ToolCall; verdict: { decision: ToolDecision; reason: string | null }; signal: AbortSignal }): { decision?: ToolDecision; reason?: string } | void | Promise<{ decision?: ToolDecision; reason?: string } | void>;
  /** Asked when the decision is `ask`. Resolve true/false or {approved, reason}. With no hook, `ask` is denied. May wait for a person; cancelling the run cancels it. */
  approve?(context: { call: ToolCall; reason: string | null; signal: AbortSignal }): boolean | { approved: boolean; reason?: string } | Promise<boolean | { approved: boolean; reason?: string }>;
  /** After a tool ran: return {result} to replace what the model sees (redaction). Throwing withholds the output. */
  afterTool?(context: { call: ToolCall; result: Json; signal: AbortSignal }): { result?: Json } | void | Promise<{ result?: Json } | void>;
}
export interface AgentOptions {
  objective?: Objective;
  /** Allow/deny/ask rules for tool calls. Denied calls are not run; the model gets a POLICY_DENIED error. */
  toolPolicy?: ToolPolicy;
  hooks?: AgentHooks;
  model: string;
  system?: string;
  tools?: Tool[];
  provider?: Provider;
  maxTurns?: number;
  policy?: LoopPolicy;
  onEvent?: (event: Action) => void | Promise<void>;
  store?: SessionStore;
  agentId?: string;
  sessionId?: string;
  resume?: boolean;
}
export interface Operation { execution_id: string; kind: 'model' | 'tool'; tool_call: ToolCall | null; objective_permit?: number }
export interface ToolExecution { execution_id: string; call: ToolCall; status: 'started' | 'completed' | 'rejected'; result: Json }
export interface RunCheckpoint {
  run_id: string; state: Json; in_flight: Operation | null; tools: ToolExecution[]; abandoned: boolean;
  objective_depth?: number;
  summary_key?: string;
}
export interface SessionInspection { revision?: number; status: 'ready' | 'running'; run: RunCheckpoint | null }
export interface CheckpointFrame { revision: number; state: Json; action: Action; run_id: string; operation: Operation | null; objective_depth: number; summary_key?: string }
export interface ResolveOperation {
  agentId: string; sessionId: string; revision: number; executionId: string;
}
export class SessionStore {
  openQueue(id?: string, policy?: QueuePolicy): Promise<DurableQueue>;
  readonly services: CatalogServices;
  artifacts(root: string): ArtifactStore;
  openSecrets(options: { id?: string; keyId?: string; key: Uint8Array }): Promise<SecretVault>;
  static open(options: { path: string; tenantId?: string }): Promise<SessionStore>;
  constructor(options: { path: string; tenantId?: string });
  forTenant(tenantId: string): SessionStore;
  forTenantAsync(tenantId: string): Promise<SessionStore>;
  recover(options: { agentId: string; sessionId: string }): number;
  recoverAsync(options: { agentId: string; sessionId: string }): Promise<number>;
  inspect(options: { agentId: string; sessionId: string; runId?: string }): SessionInspection | null;
  inspectAsync(options: { agentId: string; sessionId: string; runId?: string }): Promise<SessionInspection | null>;
  resolveTool(options: ResolveOperation & { result: Json }): CheckpointFrame;
  resolveToolAsync(options: ResolveOperation & { result: Json }): Promise<CheckpointFrame>;
  resolveModel(options: ResolveOperation & { reply: ModelReply }): CheckpointFrame;
  resolveModelAsync(options: ResolveOperation & { reply: ModelReply }): Promise<CheckpointFrame>;
  close(): void;
  closeAsync(): Promise<void>;
}

export class OpenAICompatible implements Provider {
  constructor(options?: { baseUrl?: string; apiKey?: string; timeoutMs?: number; maxResponseBytes?: number;
    outputTokenParameter?: 'max_completion_tokens' | 'max_tokens'; resilience?: Resilience });
  complete(request: ModelRequest, context?: ProviderContext): Promise<ModelReply>;
  stream(request: ModelRequest, context?: ProviderContext): AsyncIterable<ModelStreamEvent>;
}
export type ProviderPreset = 'openai' | 'anthropic' | 'gemini' | 'groq' | 'mistral' | 'openrouter' | 'deepseek' | 'kimi'
  | 'nvidia' | 'qwen' | 'minimax' | 'opencode-go' | 'hiveagents' | 'z-ai' | 'modelscope' | 'ollama';
/** Body adjustments for an OpenAI-compatible provider. Presets fill these in; see docs/providers.md. */
export interface OpenAiProfile {
  normalize_tool_names?: boolean; ensure_array_items?: boolean; strip_additional_properties?: boolean;
  parallel_tool_calls?: boolean | null; tool_choice?: string | null;
  no_tool_models?: string[]; no_tool_r1_models?: boolean;
  session_header?: string | null; user_agent?: string | null;
}
/**
 * Explicit retries and a circuit breaker for a provider's HTTP calls; off unless given.
 * Only a request that provably was not processed is retried (connection never opened, HTTP 429/503);
 * timeouts, other errors and anything that began to arrive are never repeated. Retries cost no extra
 * model call or budget and stay inside the call timeout and turn deadline.
 */
export interface Resilience {
  retry?: { /** Total tries incl. the first, 1..5 (default 1 = none). */ max_attempts?: number; base_delay_ms?: number; /** Also the longest Retry-After honoured. */ max_delay_ms?: number };
  circuit_breaker?: { failure_threshold?: number; cooldown_ms?: number };
}
export class PresetProvider implements Provider {
  constructor(preset: ProviderPreset, options?: { apiKey?: string; baseUrl?: string; timeoutMs?: number; maxResponseBytes?: number; profile?: OpenAiProfile; resilience?: Resilience });
  complete(request: ModelRequest, context?: ProviderContext): Promise<ModelReply>;
  stream(request: ModelRequest, context?: ProviderContext): AsyncIterable<ModelStreamEvent>;
}
export class Anthropic implements Provider {
  constructor(options?: { baseUrl?: string; apiKey?: string; timeoutMs?: number; maxResponseBytes?: number;
    apiVersion?: string; defaultMaxOutputTokens?: number; contextWindow?: number; resilience?: Resilience });
  complete(request: ModelRequest, context?: ProviderContext): Promise<ModelReply>;
  stream(request: ModelRequest, context?: ProviderContext): AsyncIterable<ModelStreamEvent>;
}
export class Gemini implements Provider {
  /** Without `defaultMaxOutputTokens`, the output limit is derived from `contextWindow` (15%, at most 32768), else 4096. */
  constructor(options?: { baseUrl?: string; apiKey?: string; timeoutMs?: number; maxResponseBytes?: number; defaultMaxOutputTokens?: number; contextWindow?: number; resilience?: Resilience });
  complete(request: ModelRequest, context?: ProviderContext): Promise<ModelReply>;
  stream(request: ModelRequest, context?: ProviderContext): AsyncIterable<ModelStreamEvent>;
}
/** Ollama's native chat API: `numCtx` is the context window it is asked for (Ollama otherwise silently cuts the prompt to a small default), `keepAlive` a duration such as "10m", `think` turns thinking on or off. Images only as inline data. */
export class Ollama implements Provider {
  constructor(options?: { baseUrl?: string; apiKey?: string; timeoutMs?: number; maxResponseBytes?: number; defaultMaxOutputTokens?: number;
    contextWindow?: number; numCtx?: number; keepAlive?: string; think?: boolean; resilience?: Resilience });
  complete(request: ModelRequest, context?: ProviderContext): Promise<ModelReply>;
  stream(request: ModelRequest, context?: ProviderContext): AsyncIterable<ModelStreamEvent>;
}
export class ToolOperationUncertain extends Error { readonly code: string; constructor(message?: string) }
export class ObjectiveLimit extends Error { readonly code: 'OBJECTIVE_LIMIT'; constructor(message: string) }
export class VerificationUncertain extends ToolOperationUncertain { constructor(message?: string, cause?: unknown) }
export class SummaryUncertain extends ToolOperationUncertain { constructor(message?: string, cause?: unknown) }
export class TriggerUncertain extends ToolOperationUncertain { constructor(message?: string, cause?: unknown) }
export type EventTriggerSchedule = { kind: 'event' };
export type CalendarTriggerSchedule = { kind: 'at'; unixMs: number }
  | { kind: 'interval'; everyMs: number; anchorUnixMs?: number }
  /** A calendar expression (five fields, or six with seconds) in an IANA time zone, e.g. `{ kind: 'cron', expression: '0 8 * * MON-FRI', timezone: 'America/Bogota' }`. See docs/triggers.md. */
  | { kind: 'cron'; expression: string; timezone: string; domAndDow?: boolean };
export type TriggerSchedule = EventTriggerSchedule | CalendarTriggerSchedule;
/** `skipped_is_lower_bound` appears (true) on a cron claim that missed more than 100 occurrences: `skipped_occurrences` is then "at least 100". */
export interface CalendarTriggerInput { scheduled_for_unix_ms: number; observed_unix_ms: number; skipped_occurrences: number; skipped_is_lower_bound?: true }
export interface TriggerContext {
  objective_id: string | null; trigger_id: string; occurrence_key: string; execution_id: string;
  readonly signal: AbortSignal;
  delegate(agent: Agent, prompt: string): Promise<RunResult>;
}
export interface TriggerReceipt<R = Json> { trigger_id: string; occurrence_key: string; execution_id: string; result: R }
export type TriggerResultData = Json | RunResult;
export type TriggerCallback<T, R extends TriggerResultData> = (value: T, context: TriggerContext) => R | Promise<R>;
export class Trigger<T = Json, R extends TriggerResultData = Json> {
  static open<T = Json, R extends TriggerResultData = Json>(objective: Objective, id: string, schedule: TriggerSchedule, callback: TriggerCallback<T, R>): Promise<Trigger<T, R>>;
  constructor(objective: Objective, id: string, schedule: TriggerSchedule, callback: TriggerCallback<T, R>);
  readonly id: string;
  inspect(): NativeTriggerSnapshot;
  inspectAsync(): Promise<NativeTriggerSnapshot>;
  enable(): void;
  enableAsync(): Promise<void>;
  disable(): void;
  disableAsync(): Promise<void>;
  cancel(): boolean;
  emit(key: string, value: T, options?: RunOptions): Promise<TriggerReceipt<R> | null>;
  tick(options?: RunOptions): Promise<TriggerReceipt<R> | null>;
  watch(options?: RunOptions): AsyncGenerator<TriggerReceipt<R>, void, unknown>;
  resolve(options: { occurrenceKey: string; executionId: string; result: R }): TriggerReceipt<R>;
  resolveAsync(options: { occurrenceKey: string; executionId: string; result: R }): Promise<TriggerReceipt<R>>;
}
export interface ModelReviewEvidence { evaluationId: string; justification: string }
export interface VerificationVerdict { passed: boolean; reason?: string; evidence?: Json }
export interface VerificationResult { key: string; execution_id: string; verdict: VerificationVerdict }
export interface VerificationOptions { signal?: AbortSignal; modelReview?: ModelReviewEvidence }
export interface VerificationContext {
  objective_id: string | null; verification_key: string; execution_id: string; readonly signal: AbortSignal;
  delegate(reviewer: Agent, prompt: string): Promise<RunResult>;
}
export interface VerificationEntry {
  input: Json; execution_id: string; verdict: VerificationVerdict | null;
  model_review: { evaluation_id: string; justification: string } | null;
}
/** Stored kernel state; timestamps and field names use the shared Rust contract. */
export interface NativeTriggerSnapshot {
  schedule: { kind: 'event' } | { kind: 'at'; unix_ms: number }
    | { kind: 'interval'; every_ms: number; anchor_unix_ms: number | null }
    | { kind: 'cron'; expression: string; timezone: string; dom_and_dow: boolean };
  enabled: boolean; anchor_unix_ms: number | null; next_due_unix_ms: number | null; active_occurrence: string | null;
  executions: Record<string, { execution_id: string; input: Json; outcome: { value: Json } | null }>;
}
export interface ObjectivePolicy {
  max_model_calls?: number; max_tool_calls?: number; max_depth?: number;
  max_output_tokens?: number; max_total_output_tokens?: number; max_run_ms?: number | null;
}
export interface ObjectiveSnapshot {
  id?: string; revision?: number;
  policy: Required<ObjectivePolicy>; deadline_unix_ms: number | null;
  model_calls: number; tool_calls: number; input_tokens: number; output_tokens: number;
  reservations: Record<string, number>; stopped: string | null;
  verifications: Record<string, VerificationEntry>;
  triggers: Record<string, NativeTriggerSnapshot>;
  summaries: Record<string, { input: SummaryInput; execution_id: string; content: string | null;
    models: Record<string, { input_tokens: number; output_tokens: number } | null>; tool_calls: number }>;
  summary_costs: Record<string, SummaryCost>;
}
export interface SummaryInput { source: ContextSource; messages: Message[] }
export interface SummaryContext {
  objective_id: string | null; summary_key: string; execution_id: string; readonly signal: AbortSignal;
  delegate(agent: Agent, prompt: string): Promise<RunResult>;
}
export type SummaryCallback = (input: SummaryInput, context: SummaryContext) => string | Promise<string>;
export interface SummaryCost {
  model_calls: number; tool_calls: number; input_tokens: number; output_tokens: number;
  unmetered_model_calls: number; pending_model_calls: number; reserved_output_tokens: number;
}
export interface SummaryResult { key: string; execution_id: string; summary: ContextSummary; cost: SummaryCost }
export class Objective {
  static open(policy?: ObjectivePolicy, options?: { store: SessionStore; id: string }): Promise<Objective>;
  constructor(policy?: ObjectivePolicy, options?: { store: SessionStore; id: string });
  readonly id: string | undefined;
  inspect(): ObjectiveSnapshot;
  inspectAsync(): Promise<ObjectiveSnapshot>;
  summarize(key: string, input: SummaryInput, callback: SummaryCallback, options?: RunOptions): Promise<SummaryResult>;
  resolveSummary(options: { key: string; executionId: string; content: string }): SummaryResult;
  resolveSummaryAsync(options: { key: string; executionId: string; content: string }): Promise<SummaryResult>;
  trigger<T = Json, R extends TriggerResultData = Json>(id: string, schedule: EventTriggerSchedule, callback: TriggerCallback<T, R>): Trigger<T, R>;
  trigger<R extends TriggerResultData = Json>(id: string, schedule: CalendarTriggerSchedule, callback: TriggerCallback<CalendarTriggerInput, R>): Trigger<CalendarTriggerInput, R>;
  triggerAsync<T = Json, R extends TriggerResultData = Json>(id: string, schedule: EventTriggerSchedule, callback: TriggerCallback<T, R>): Promise<Trigger<T, R>>;
  triggerAsync<R extends TriggerResultData = Json>(id: string, schedule: CalendarTriggerSchedule, callback: TriggerCallback<CalendarTriggerInput, R>): Promise<Trigger<CalendarTriggerInput, R>>;
  /** value must be JSON data; the callback receives its JSON snapshot. */
  verify<T>(key: string, value: T, callback: (value: T, context: VerificationContext) => VerificationVerdict | Promise<VerificationVerdict>, options?: VerificationOptions): Promise<VerificationResult>;
  resolveVerification(options: { key: string; executionId: string; verdict: VerificationVerdict }): VerificationResult;
  resolveVerificationAsync(options: { key: string; executionId: string; verdict: VerificationVerdict }): Promise<VerificationResult>;
  cancel(): void;
  cancelAsync(): Promise<void>;
}
export class McpOperationUncertain extends ToolOperationUncertain { constructor(message?: string) }
export interface McpConnectionOptions {
  protocolVersion?: '2024-11-05' | '2025-03-26' | '2025-06-18' | '2025-11-25'; timeoutMs?: number; maxMessageBytes?: number;
  maxTools?: number; maxPages?: number; maxMessages?: number;
}
export interface McpStdioOptions extends McpConnectionOptions {
  transport?: 'stdio'; command: string; args?: string[]; env?: Record<string, string>; cwd?: string;
}
export interface McpHttpOptions extends McpConnectionOptions { transport: 'http'; url: string; headers?: Record<string, string> }
/** The classic HTTP+SSE transport (a long-lived event stream plus POSTs), as n8n's MCP Server Trigger speaks it. The message endpoint the server names must be on the stream's origin. Default `protocolVersion` is `2025-03-26`. */
export interface McpSseOptions extends McpConnectionOptions { transport: 'sse'; url: string; headers?: Record<string, string> }
export interface McpRemoteTool { name: string; description: string; inputSchema: Record<string, Json>; outputSchema?: Record<string, Json>; annotations?: Json }
export type McpServerOptions = (McpStdioOptions | McpHttpOptions | McpSseOptions) & { /** false keeps the server in the set without connecting it. */ enabled?: boolean };
export interface McpServerStatus {
  name: string; state: 'connected' | 'failed' | 'disabled' | 'closed'; tools: number; tools_changed: boolean; error: string | null;
  config_hash: string; server: { name?: string; version?: string } | null;
}
/** Tools by name that appeared, disappeared or changed description/input schema. */
export interface McpToolsDiff { added: string[]; removed: string[]; changed: string[] }
export interface McpApplyReport {
  added: string[]; removed: string[]; restarted: string[]; unchanged: string[]; disabled: string[]; failed: Record<string, string>;
}
export interface McpManagedTool extends McpRemoteTool { server: string; tool: string }
/**
 * Several named MCP servers that can be reloaded while running; nothing reconnects or retries on its own
 * (an uncertain call closes its server). Tool names are `<server>__<tool>`. See docs/mcp.md.
 */
export class McpManager {
  constructor();
  /** Reconcile the running set with `servers` (the whole desired set). A server that fails to connect is reported in `failed`; an invalid set changes nothing. drainMs lets a running call finish before its server is closed. */
  apply(servers: Record<string, McpServerOptions>, options?: { drainMs?: number }): Promise<McpApplyReport>;
  reconnect(name: string): Promise<{ server: string; connected: true }>;
  /** List one connected server's tools again. `catalog` and `tools()` use the new list; an Agent already created keeps the tools it had. */
  refreshTools(name: string): Promise<McpToolsDiff & { server: string }>;
  readonly status: McpServerStatus[];
  readonly catalog: McpManagedTool[];
  tools(options?: { servers?: string[]; names?: string[] }): ExecutableTool[];
  call(server: string, tool: string, arguments: Record<string, Json>, options?: { signal?: AbortSignal; context?: Record<string, Json> }): Promise<Json>;
  close(): Promise<void>;
}
export class McpClient {
  private constructor();
  static connect(options: McpStdioOptions | McpHttpOptions | McpSseOptions): Promise<McpClient>;
  readonly info: Record<string, Json>;
  readonly catalog: McpRemoteTool[];
  /** The server announced a change of its tool list (seen after the next exchange; `refreshTools()` lists now). */
  readonly toolsChanged: boolean;
  /** List the tools again; `catalog` and `tools()` use the new list. */
  refreshTools(): Promise<McpToolsDiff>;
  listResources(options?: { cursor?: string; signal?: AbortSignal }): Promise<{ resources: McpResource[]; nextCursor?: string }>;
  readResource(uri: string, options?: { signal?: AbortSignal }): Promise<{ contents: McpResourceContent[] }>;
  listPrompts(options?: { cursor?: string; signal?: AbortSignal }): Promise<{ prompts: McpPrompt[]; nextCursor?: string }>;
  getPrompt(name: string, arguments_?: Record<string, string>, options?: { signal?: AbortSignal }): Promise<{ description?: string; messages: { role: 'user' | 'assistant'; content: Record<string, Json> }[] }>;
  tools(options?: { prefix?: string; names?: string[] }): Tool[];
  call(name: string, arguments_: Record<string, Json>, options?: { signal?: AbortSignal; context?: Partial<Pick<ToolContext, 'run_id' | 'execution_id' | 'tool_call_id'>> }): Promise<Json>;
  close(): Promise<void>;
}
export class Agent {
  static open(options: AgentOptions): Promise<Agent>;
  constructor(options: AgentOptions);
  readonly messages: Message[];
  readonly contextPlan: ContextPlan | null;
  captureContext(start: number, end: number): ContextSource;
  readContext(source: ContextSource): Message[];
  searchContext(query: string, options?: { maxTurns?: number; maxBytes?: number }): ContextSearchResult;
  setContextPlan(plan: ContextPlan | null): void;
  setContextPlanAsync(plan: ContextPlan | null): Promise<void>;
  summarize(key: string, source: ContextSource, callback: SummaryCallback, options?: RunOptions & { objective?: Objective }): Promise<SummaryResult>;
  run(prompt: Prompt, options?: RunOptions): Promise<RunResult>;
  resume(options?: RunOptions): Promise<RunResult>;
  stream(prompt: Prompt, options?: RunOptions): AsyncGenerator<AgentStreamEvent, void, unknown>;
  resumeStream(options?: RunOptions): AsyncGenerator<AgentStreamEvent, void, unknown>;
  cancel(): boolean;
  close(): void;
}

/** Catalog metadata only: registration does not install an executor. */
export interface CatalogCommon { enabled?: boolean }
export interface CatalogAgent extends CatalogCommon { name: string; description?: string; system_prompt?: string; role?: 'coordinator' | 'worker'; provider_id?: string; model_id?: string; tools?: string[]; skills?: string[]; mcp_servers?: string[]; max_iterations?: number }
export interface CatalogSkillStep { step?: number; action: string; instruction: string; output?: string; params?: Record<string, Json> }
export interface CatalogSkill extends CatalogCommon {
  name: string; description: string; content: string; category?: string; version?: string;
  tools?: string[]; triggers?: string[]; author?: string; icon?: string; permissions?: string[];
  dependencies?: string[]; preferred_agents?: string[]; rules?: string[]; steps?: CatalogSkillStep[];
  output_format?: Record<string, Json>; examples?: Record<string, Json>[];
  requirements?: Record<string, Json>; metadata?: Record<string, Json>;
}
export interface CatalogTool extends CatalogCommon { name: string; description: string; parameters: Record<string, Json>; category?: string }
export interface CatalogMemory extends CatalogCommon { title: string; content: string; tags?: string[] }
export interface CatalogProvider extends CatalogCommon { name: string; protocol: 'openai_compatible' | 'anthropic' | 'gemini' | 'hiveagents'; base_url: string }
export interface CatalogModel extends CatalogCommon { name: string; provider_id: string; wire_id: string; context_window?: number; input_per_million?: number; output_per_million?: number; capabilities?: string[] }
export interface CatalogEthics extends CatalogCommon { name: string; content: string; priority?: number }
export type CatalogMcpServer = CatalogCommon & { name: string } & ({ transport: 'stdio'; command: string; args?: string[] } | { transport: 'http'; url: string });
export interface CatalogEndpoint extends CatalogCommon { name: string; url: string; method: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE' | 'HEAD' | 'OPTIONS'; description?: string }
export interface CatalogCron extends CatalogCommon { name: string; agent_id: string; schedule: string; timezone?: string; prompt: string }
export interface CatalogData { agents: CatalogAgent; skills: CatalogSkill; tools: CatalogTool; memory: CatalogMemory; providers: CatalogProvider; models: CatalogModel; ethics: CatalogEthics; mcp_servers: CatalogMcpServer; endpoints: CatalogEndpoint; cron: CatalogCron }
export type EntityKind = keyof CatalogData;
export interface CatalogRecord<K extends EntityKind = EntityKind> { id: string; kind: K; revision: number; data: CatalogData[K] }
export interface CatalogPage<K extends EntityKind = EntityKind> { items: CatalogRecord<K>[]; next_cursor: string | null }
export interface CatalogHit<K extends EntityKind = EntityKind> { record: CatalogRecord<K>; score: number }
export interface CatalogCollection<K extends EntityKind> {
  create(id: string, data: CatalogData[K]): Promise<CatalogRecord<K>>;
  get(id: string): Promise<CatalogRecord<K> | null>;
  list(options?: { limit?: number; cursor?: string | null }): Promise<CatalogPage<K>>;
  update(id: string, revision: number, patch: Partial<CatalogData[K]>): Promise<CatalogRecord<K>>;
  delete(id: string, revision: number): Promise<void>;
  setEnabled(id: string, revision: number, enabled: boolean): Promise<CatalogRecord<K>>;
  search(query: string, options?: { limit?: number }): Promise<CatalogHit<K>[]>;
}
export class CatalogServices {
  constructor(store: SessionStore);
  importSkill(path: string): Promise<CatalogRecord<'skills'>>;
  /** Skills that ship with the SDK (obscura_browser, office_document_manager, pdf_report_writer). Listing enables nothing. */
  bundledSkills(): Promise<{ name: string; description: string; version: string; tools: string[] }[]>;
  /** Import one bundled skill into the catalog by name. Its tools are the SDK's own (obscura_*, office_*). */
  importBundledSkill(name: string): Promise<CatalogRecord<'skills'>>;
  createTools(): Promise<Tool[]>;
  collection<K extends EntityKind>(kind: K): CatalogCollection<K>;
}

/** Attach to a CDP page WebSocket. Does not own or launch the browser process. */
export class Browser {
  private constructor();
  static connect(pageWebSocketUrl: string, options?: { timeoutMs?: number }): Promise<Browser>;
  navigate(url: string): Promise<Json>;
  evaluate(expression: string): Promise<Json>;
  click(selector: string): Promise<Json>;
  typeText(selector: string, text: string): Promise<Json>;
  extract(selector?: string): Promise<Json>;
  screenshot(): Promise<{ data: string }>;
  cancel(): void;
  close(): Promise<void>;
}

export interface CommandPolicy { allowed_programs: string[]; env?: Record<string, string>; max_timeout_ms?: number }
export type SearchProvider =
  | { kind: 'brave'; api_key: string; endpoint?: string }
  | { kind: 'custom'; endpoint: string; query_param: string; count_param?: string; headers?: Record<string, string> };
export interface WebPolicy { allow_private_hosts?: boolean; allowed_hosts?: string[]; timeout_ms?: number; search?: SearchProvider }
/** Limits for the office_* tools; they exist only when this is configured. See docs/office.md. */
export interface OfficePolicy { max_input_bytes?: number; max_sheets?: number; max_rows_per_sheet?: number; timeout_ms?: number }
/** Policy for the obscura_* browser tools (the Obscura headless browser); they exist only when this is configured. See docs/obscura.md. */
export interface ObscuraPolicy {
  /** Obscura binary; otherwise OBSCURA_PATH, then `obscura` on PATH, then ~/.hive/bin and ~/.hivecode/bin. */
  executable?: string;
  stealth?: boolean; proxy?: string; user_agent?: string;
  /** Let pages reach localhost and private networks. Obscura blocks them by default. */
  allow_private_network?: boolean;
  /** When non-empty, pages may only be on these exact hosts. */
  allowed_hosts?: string[];
  /** Offer obscura_evaluate (arbitrary page JavaScript). Off by default. */
  allow_evaluate?: boolean;
  /** Offer the cookie and storage-state tools, which expose session secrets. Off by default. */
  allow_credentials?: boolean;
  /** Download the latest Obscura when none is found (default true; also off when HIVE_OBSCURA_AUTO_INSTALL is 0 or false). An explicit executable/OBSCURA_PATH that is missing is never replaced by a download. */
  auto_install?: boolean;
  /** Absolute directory for a downloaded Obscura. Default ~/.hive/bin, shared with Hive. */
  install_dir?: string;
  startup_timeout_ms?: number; call_timeout_ms?: number;
}
/** Policy for the computer_* tools (screenshots, pointer, keyboard); they exist only when this is configured, and input is a separate grant. See docs/computer-use.md. */
export interface ComputerPolicy {
  /** `auto`: portals on a Wayland session, X11 otherwise. */
  backend?: 'auto' | 'x11' | 'portal';
  /** Offer computer_screenshot (default true). */
  allow_screenshot?: boolean;
  /** Offer the pointer and keyboard tools (default false). */
  allow_input?: boolean;
  /** Longest side of the screenshot the model sees, and so of its coordinate space (default 1280). */
  max_dimension?: number;
  /** Actions allowed until computerReset() (default 200). */
  max_actions?: number;
  action_timeout_ms?: number;
  /** Includes the person answering the desktop's consent dialog on Wayland (default 120000). */
  start_timeout_ms?: number;
  max_text_chars?: number;
  /** X11 display such as ":0"; $DISPLAY when omitted. */
  display?: string;
  /** Absolute file where the portal's restore token is kept (mode 0600) so the next start does not ask again. */
  restore_token_path?: string;
}
export interface ComputerStatus { stopped: boolean; actions: number }
export interface NativeToolsConfig {
  /** Existing directory that every filesystem path and command cwd must stay inside. */
  root: string;
  max_read_bytes?: number;
  max_write_bytes?: number;
  max_output_bytes?: number;
  /** Enables command_run for these bare program names. Disabled when omitted. */
  command?: CommandPolicy;
  /** Enables web_fetch (and web_search when `search` is set). Disabled when omitted. */
  web?: WebPolicy;
  /** Enables office_read/write_docx, _xlsx and _pptx inside the root. Disabled when omitted. */
  office?: OfficePolicy;
  /** Enables the obscura_* browser tools. Disabled when omitted. */
  obscura?: ObscuraPolicy;
  /** Enables the computer_* desktop tools. Disabled when omitted. */
  computer?: ComputerPolicy;
}
export interface NativeToolDefinition extends ToolDefinition { allow_repeated_results: boolean }
export interface ExecutableTool extends NativeToolDefinition { execute(arguments: Record<string, Json>, context?: ToolContext): Promise<Json> }

/** Sandboxed fs_*, command_run, web_fetch and web_search tools for an Agent. */
export class NativeTools {
  constructor(config: NativeToolsConfig);
  definitions(): Promise<NativeToolDefinition[]>;
  execute(name: string, arguments: Json): Promise<Json>;
  createTools(): Promise<ExecutableTool[]>;
  /** Download and install the latest Obscura (checked against its published SHA-256) into obscura.install_dir, ~/.hive/bin by default. An existing install is kept unless update is true. */
  installObscura(options?: { update?: boolean }): Promise<{ path: string; version: string; asset: string; sha256: string; already_current: boolean }>;
  cancel(): void;
  /** Host kill switch for computer_*: interrupts an action in progress (reported as uncertain, held keys released) and refuses every later one until computerResume(). */
  computerStop(): Promise<ComputerStatus>;
  computerResume(): Promise<ComputerStatus>;
  /** Gives the computer_* tools a fresh action budget. */
  computerReset(): Promise<ComputerStatus>;
  computerStatus(): Promise<ComputerStatus>;
  /** Stops the Obscura session the obscura_* tools started, if any, and ends the desktop session; the next call starts a new one. */
  close(): Promise<void>;
}

export interface ArtifactRecord {
  id: string; kind: string; mime_type: string; size: number; sha256: string;
  width: number | null; height: number | null; run_id: string | null; task_id: string | null;
  status: 'active' | 'expired' | 'deleted'; created_at: number; expires_at: number | null; expired_at: number | null;
}
/** Bounded artifact store; see docs/artifacts.md. */
export class ArtifactStore {
  private constructor();
  definitions(): Promise<NativeToolDefinition[]>;
  execute(name: string, arguments: Json): Promise<Json>;
  createTools(): Promise<ExecutableTool[]>;
  expire(options?: { now?: number }): Promise<string[]>;
}
export interface OracleConfig {
  endpoint: string; model: string; api_key?: string; timeout_ms?: number;
  self_hosted?: boolean; max_output_tokens?: number;
  /** Charged when the server reports no usage; without it such a call halts the Objective. */
  assumed_input_tokens?: number | null;
  cooldown_ms?: number; distrust_strikes?: number; distrust_ms?: number;
}
export type OracleQuestion =
  | { type: 'choice'; instructions: string; criteria: Record<string, string> }
  | { type: 'noul'; instructions: string; criteria?: { true: string; false: string } };
export type OracleAnswer =
  | { type: 'choice'; choice: string; confidence: number; probabilities: Record<string, number> }
  | { type: 'noul'; noul: number };
export type OracleReply =
  | { outcome: 'decision'; answers: Record<string, OracleAnswer>; charge: { input_tokens: number; output_tokens: number } | null; cost_usd: number | null; latency_ms: number }
  | { outcome: 'fallback'; reason: string; sent: boolean };
export interface OracleStatus { state: 'ready' | 'fallback'; cooling_down: boolean; last_error: string | null; last_success_at: number | null; decisions: number }
/** Opt-in decision oracle; see docs/oracle.md. */
export class Oracle {
  constructor(config: OracleConfig);
  ask(state: Json, questions: Record<string, OracleQuestion>, options?: { objective?: Objective; depth?: number; signal?: AbortSignal }): Promise<OracleReply>;
  status(): OracleStatus;
  recordOverruled(reason: string): boolean;
  recordAgreement(): void;
  reset(): void;
  cancel(): void;
}

export interface McpResource { uri: string; name: string; description?: string; mimeType?: string }
export type McpResourceContent = { uri: string; mimeType?: string } & ({ text: string; blob?: never } | { blob: string; text?: never });
export interface McpPrompt { name: string; description?: string; arguments?: { name: string; description?: string; required?: boolean }[] }

/** Managed GGUF provider: prepare the requested model, infer with the local alias. */
export class HiveAgents implements Provider {
  constructor(options: { baseUrl: string; apiKey?: string; timeoutMs?: number; maxResponseBytes?: number; contextWindow?: number });
  complete(request: ModelRequest, context?: ProviderContext): Promise<ModelReply>;
  stream(request: ModelRequest, context?: ProviderContext): AsyncIterable<ModelStreamEvent>;
}

export interface QueuePolicy {
  max_concurrency?: number; lease_ms?: number; retry_delay_ms?: number; task_timeout_ms?: number | null;
  max_jobs?: number; max_payload_bytes?: number;
}
export interface JobInput {
  kind: string; lane: string; payload: Record<string, Json>; priority?: number;
  not_before?: number | null; max_attempts?: number; retry_safe?: boolean;
  idempotency_key?: string | null; objective_id?: string | null; depth?: number;
}
export type JobResult = Json | RunResult;
export type JobStatus = 'pending' | 'running' | 'cancelling' | 'uncertain' | 'completed' | 'failed' | 'cancelled';
export interface Job {
  id: string; revision: number; input: Required<JobInput>; status: JobStatus;
  sequence: number; attempts: number; not_before: number; created_at: number;
  started_at: number | null; attempt_started_at: number | null; finished_at: number | null; execution_id: string | null;
  worker_id: string | null; lease_expires_at: number | null; cancellation_requested: boolean;
  result: JobResult | null; error: string | null;
}
export type JobOutcome = { type: 'completed'; result: JobResult }
  | { type: 'failed'; error: string; retryable?: boolean } | { type: 'cancelled' }
  | { type: 'uncertain'; error: string };
export type JobResolution = { type: 'completed'; result: JobResult }
  | { type: 'failed'; error: string } | { type: 'cancelled' } | { type: 'retry' };
export interface JobContext {
  readonly job_id: string; readonly execution_id: string; readonly objective_id: string | null;
  readonly depth: number; readonly signal: AbortSignal;
  runAgent(agent: Agent, prompt?: string, options?: { resume?: boolean }): Promise<RunResult>;
}
export type JobExecutor = (job: Job, context: JobContext) => JobOutcome | Promise<JobOutcome>;
export interface QueueInspection { id: string; policy: Required<QueuePolicy>; retained_jobs: number; active_counts: Partial<Record<JobStatus, number>> }
export interface SpecialistTask { id: string; specialist_id: string; prompt: string; }
export interface GroupSpec {
  objective_id: string; coordinator_agent_id: string; tasks: SpecialistTask[];
  depth?: number; require_verification?: boolean;
}
export interface GroupProof {
  input: Json; verification_key: string; verification_execution_id: string;
  verdict: { passed: boolean; reason?: string; evidence?: Json }; met: boolean; created_at: number;
}
export interface DelegationGroup {
  id: string; spec: Required<GroupSpec>; job_ids: string[];
  tasks: { task: SpecialistTask; job: Job; proof: GroupProof | null; verification: 'passed' | 'failed' | 'unchecked' }[];
  cancelled: boolean; ready: boolean;
  status: JobStatus | 'ready' | 'waiting' | 'blocked_uncertain'; synthesis: Job | null;
  specialist_kind: string; synthesis_kind: string;
}
export interface JobFilter { status?: JobStatus[]; kind?: string; kind_prefix?: string; lane?: string; objective_id?: string }
export interface QueueEvent {
  version: 1; tenant: string; queue: string; seq: number; at: number;
  type: 'job.status' | 'job.retried' | 'goal.transition';
  subject: Record<string, Json>;
}
export interface GoalSpec {
  objective_id: string; goal: string; planner_agent_id: string; coordinator_agent_id: string;
  reviewer_agent_id?: string | null; specialist_ids?: string[];
  /** 1..8, default 3. */ max_rounds?: number; /** 1..64, default 8. */ max_tasks_per_round?: number;
  depth?: number; require_verification?: boolean;
}
export type GoalStatus = 'planning' | 'running' | 'reviewing' | 'accepted' | 'failed' | 'cancelled';
type GoalJob = { id: string; status: JobStatus; error: string | null } | null;
export interface GoalReport {
  id: string; spec: Required<GoalSpec>; status: GoalStatus;
  /** A job the goal waits on has an uncertain outcome; it is never replayed. */
  blocked_uncertain: boolean; round: number; error: string | null; feedback: string | null;
  rounds: { index: number; plan_job: GoalJob; group: string | null; synthesis_job: GoalJob; review_job: GoalJob;
    verdict: { accepted: boolean; evidence: string; feedback: string } | null }[];
  group: DelegationGroup | null;
  /** Job kinds the host's workers must register: planner, reviewer and, per round, specialists and synthesis. */
  kinds: { plan: string; review: string; rounds: { round: number; specialist: string; synthesis: string }[] };
}
export interface GoalAgentRequest {
  agentId: string; sessionId: string; stage: 'plan' | 'specialist' | 'synthesis' | 'review';
  round: number | null; taskId: string | null; job: Job;
}
/** The JSON object in a model reply: bare, fenced, or surrounded by prose; null if there is none. */
export function extractJson(text: string | null | undefined): Record<string, Json> | null;
/**
 * Drive a goal with the host's agents until it is accepted, failed or cancelled, or nothing can run
 * (it waits on other workers' jobs, or a job's outcome is uncertain: reconcile it, it is never replayed).
 * `agentFor` returns the Agent for each job (bind it to `sessionId`); it is closed after the job.
 */
export function runGoal(queue: DurableQueue, goalId: string, options: {
  agentFor: (request: GoalAgentRequest) => Agent | Promise<Agent>; signal?: AbortSignal; maxSteps?: number;
}): Promise<GoalReport>;
export type ScheduleDefinition = { kind: 'at'; unix_ms: number }
  | { kind: 'interval'; every_ms: number; anchor_unix_ms?: number | null }
  | { kind: 'cron'; expression: string; timezone: string; dom_and_dow?: boolean };
export interface ScheduleSpec {
  schedule: ScheduleDefinition; job: Omit<JobInput, 'not_before' | 'idempotency_key'>;
  misfire_policy?: 'skip' | 'fire_once'; misfire_grace_ms?: number;
  start_at?: number | null; stop_at?: number | null; max_runs?: number | null;
}
export interface ScheduleInspection {
  id: string; spec: ScheduleSpec; enabled: boolean; created_at: number;
  next_due: number | null; last_tick: number | null; published: number;
  active_job: Job | null; status: 'paused' | 'active' | 'blocked' | 'completed';
}
export interface ScheduleTick { outcome: 'idle' | 'blocked' | 'skipped' | 'published'; job: Job | null; }
export class DurableQueue {
  private constructor();
  static open(options: { store: SessionStore; id?: string; policy?: QueuePolicy }): Promise<DurableQueue>;
  readonly id: string;
  readonly policy: Required<QueuePolicy>;
  inspect(): Promise<QueueInspection>;
  enqueue(input: JobInput): Promise<Job>;
  get(jobId: string): Promise<Job | null>;
  list(options?: { limit?: number; cursor?: string | null }): Promise<{ items: Job[]; next_cursor: string | null }>;
  claim(workerId: string, kinds: string[]): Promise<Job | null>;
  heartbeat(jobId: string, executionId: string): Promise<Job>;
  finish(jobId: string, executionId: string, outcome: JobOutcome): Promise<Job>;
  cancel(jobId: string): Promise<Job>;
  cancelExecution(jobId: string, executionId: string): Promise<Job>;
  maintain(): Promise<Job[]>;
  reconcile(jobId: string, executionId: string, resolution: JobResolution): Promise<Job>;
  createGroup(groupId: string, spec: GroupSpec): Promise<DelegationGroup>;
  group(groupId: string): Promise<DelegationGroup>;
  synthesizeGroup(groupId: string): Promise<Job>;
  cancelGroup(groupId: string): Promise<DelegationGroup>;
  /** Jobs matching the filter, oldest first. At most 5000 are examined per call; resume with next_cursor. */
  listJobs(filter?: JobFilter, options?: { limit?: number; cursor?: string | null }): Promise<{ items: Job[]; next_cursor: string | null; examined: number }>;
  /** Put a failed job back in the queue. Needs attempts left unless force; the objective is checked again and nothing is refunded. */
  retryJob(jobId: string, options?: { force?: boolean }): Promise<Job>;
  /** Cancel pending/running jobs matching the filter (finished states are ignored; at most 1000 per call). */
  cancelJobs(filter: JobFilter): Promise<{ cancelled: { id: string; status: JobStatus }[]; truncated: boolean }>;
  /** Permanently delete finished jobs of one explicit kind/kind_prefix that ended before finishedBefore (ms). Group/goal jobs (kind hive.*) and a schedule's active job are refused/kept. */
  purgeJobs(filter: JobFilter, finishedBefore: number, options?: { limit?: number }): Promise<{ purged: number }>;
  /** Durable event log, oldest first, committed atomically with the change each event describes. Resume from next_after. */
  events(options?: { after?: number; limit?: number }): Promise<{ items: QueueEvent[]; next_after: number; latest: number }>;
  /** Delete events up to and including a sequence. The log is never purged on its own. */
  pruneEvents(upTo: number): Promise<{ removed: number }>;
  createGoal(goalId: string, spec: GoalSpec): Promise<GoalReport>;
  goal(goalId: string): Promise<GoalReport>;
  /** Advance the goal as far as durable job results allow. Idempotent and safe to call concurrently. */
  tickGoal(goalId: string): Promise<GoalReport>;
  cancelGoal(goalId: string): Promise<GoalReport>;
  groupVerificationInput(groupId: string, taskId: string): Promise<Json>;
  recordGroupProof(groupId: string, taskId: string, verificationKey: string): Promise<GroupProof>;
  createSchedule(scheduleId: string, spec: ScheduleSpec): Promise<ScheduleInspection>;
  schedule(scheduleId: string): Promise<ScheduleInspection | null>;
  enableSchedule(scheduleId: string, enabled?: boolean): Promise<ScheduleInspection>;
  tickSchedule(scheduleId: string, observed?: number | null): Promise<ScheduleTick>;
  listSchedules(options?: { limit?: number; cursor?: string | null }): Promise<{ items: ScheduleInspection[]; next_cursor: string | null }>;
  nextOccurrence(schedule: ScheduleDefinition, after: number): Promise<number | null>;
  runScheduler(options?: { pollMs?: number; signal?: AbortSignal }): Promise<void>;
  runOnce(executors: Record<string, JobExecutor>, options?: { workerId?: string; signal?: AbortSignal }): Promise<Job[]>;
}

export interface SecretReference { vault_id: string; name: string; }
export interface SecretMetadata { name: string; revision: number; key_id: string; deleted: boolean; created_at: number; updated_at: number; }
export interface SecretVaultInfo { vault_id: string; key_id: string; revision: number; retained_records: number; max_records: number; encryptions: number; max_encryptions: number; }
export class SecretVault {
  private constructor();
  static open(options: { store: SessionStore; id?: string; keyId?: string; key: Uint8Array }): Promise<SecretVault>;
  info(): Promise<SecretVaultInfo>;
  inspect(name: string): Promise<SecretMetadata | null>;
  reveal(name: string): Promise<string | null>;
  put(name: string, value: string, options?: { revision?: number }): Promise<SecretMetadata>;
  delete(name: string, revision: number): Promise<SecretMetadata>;
  list(options?: { limit?: number; cursor?: string | null; includeDeleted?: boolean }): Promise<{ items: SecretMetadata[]; next_cursor: string | null }>;
  rekey(options: { keyId: string; key: Uint8Array }): Promise<SecretVaultInfo>;
  close(): Promise<void>;
}
