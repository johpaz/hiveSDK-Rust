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
}
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
export interface AgentOptions {
  objective?: Objective;
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
    outputTokenParameter?: 'max_completion_tokens' | 'max_tokens' });
  complete(request: ModelRequest, context?: ProviderContext): Promise<ModelReply>;
  stream(request: ModelRequest, context?: ProviderContext): AsyncIterable<ModelStreamEvent>;
}
export class Anthropic implements Provider {
  constructor(options?: { baseUrl?: string; apiKey?: string; timeoutMs?: number; maxResponseBytes?: number;
    apiVersion?: string; defaultMaxOutputTokens?: number });
  complete(request: ModelRequest, context?: ProviderContext): Promise<ModelReply>;
  stream(request: ModelRequest, context?: ProviderContext): AsyncIterable<ModelStreamEvent>;
}
export class Gemini implements Provider {
  constructor(options?: { baseUrl?: string; apiKey?: string; timeoutMs?: number; maxResponseBytes?: number; defaultMaxOutputTokens?: number });
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
  | { kind: 'interval'; everyMs: number; anchorUnixMs?: number };
export type TriggerSchedule = EventTriggerSchedule | CalendarTriggerSchedule;
export interface CalendarTriggerInput { scheduled_for_unix_ms: number; observed_unix_ms: number; skipped_occurrences: number }
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
    | { kind: 'interval'; every_ms: number; anchor_unix_ms: number | null };
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
  protocolVersion?: '2025-06-18' | '2025-11-25'; timeoutMs?: number; maxMessageBytes?: number;
  maxTools?: number; maxPages?: number; maxMessages?: number;
}
export interface McpStdioOptions extends McpConnectionOptions {
  transport?: 'stdio'; command: string; args?: string[]; env?: Record<string, string>; cwd?: string;
}
export interface McpHttpOptions extends McpConnectionOptions { transport: 'http'; url: string; headers?: Record<string, string> }
export interface McpRemoteTool { name: string; description: string; inputSchema: Record<string, Json>; outputSchema?: Record<string, Json>; annotations?: Json }
export class McpClient {
  private constructor();
  static connect(options: McpStdioOptions | McpHttpOptions): Promise<McpClient>;
  readonly info: Record<string, Json>;
  readonly catalog: McpRemoteTool[];
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
  run(prompt: string, options?: RunOptions): Promise<RunResult>;
  resume(options?: RunOptions): Promise<RunResult>;
  stream(prompt: string, options?: RunOptions): AsyncGenerator<AgentStreamEvent, void, unknown>;
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
