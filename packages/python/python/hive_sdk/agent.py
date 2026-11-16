from __future__ import annotations

import asyncio
import copy
import inspect
import json
import threading
import time
from dataclasses import dataclass
from typing import Any, AsyncGenerator, AsyncIterator, Awaitable, Callable, Literal, Protocol, TypedDict

from . import _native
from .objective import Objective, ObjectiveLimit
from ._storage import storage_call


class _ModelRequestBase(TypedDict):
    model: str
    messages: list[dict[str, Any]]
    tools: list[dict[str, Any]]


TokenEncoding = Literal['cl100k_base', 'o200k_base']
InputTokenFraming = Literal['neutral_json', 'openai_chat_json', 'anthropic_messages_json', 'gemini_generate_content_json']


AttachmentKind = Literal['image', 'audio', 'document']


class _AttachmentBase(TypedDict):
    kind: AttachmentKind
    media_type: str


class Attachment(_AttachmentBase, total=False):
    """Inline base64 ``data`` or a public ``url`` (exactly one)."""
    data: str
    url: str
    name: str


class PromptParts(TypedDict, total=False):
    text: str
    attachments: list[Attachment]


Prompt = str | PromptParts


class _InputTokenBudgetBase(TypedDict):
    encoding: TokenEncoding
    max_tokens: int


class InputTokenBudget(_InputTokenBudgetBase, total=False):
    overhead_tokens: int
    framing: InputTokenFraming
    max_total_tokens: int | None
    context_window_tokens: int | None
    attachment_tokens: int | None


class TokenEstimate(TypedDict):
    encoding: TokenEncoding
    framing: InputTokenFraming
    serialized_tokens: int
    overhead_tokens: int
    total_tokens: int


class _ContextReportBase(TypedDict):
    serialized_bytes: int
    dropped_messages: int


class ContextSource(TypedDict):
    start: int
    end: int
    sha256: str


class ContextSummary(TypedDict):
    id: str
    source: ContextSource
    content: str


class ContextPlan(TypedDict, total=False):
    summary: ContextSummary | None
    retrieved: list[ContextSource]


class ContextHit(TypedDict):
    source: ContextSource
    score: int
    messages: list[dict[str, Any]]


class ContextSearchResult(TypedDict):
    hits: list[ContextHit]
    serialized_bytes: int


class SummaryInput(TypedDict):
    source: ContextSource
    messages: list[dict[str, Any]]


class SummaryCost(TypedDict):
    model_calls: int
    tool_calls: int
    input_tokens: int
    output_tokens: int
    unmetered_model_calls: int
    pending_model_calls: int
    reserved_output_tokens: int


class SummaryResult(TypedDict):
    key: str
    execution_id: str
    summary: ContextSummary
    cost: SummaryCost


class ContextReport(_ContextReportBase, total=False):
    token_estimate: TokenEstimate
    summary_id: str
    retrieved_sources: list[ContextSource]


class StructuredOutput(TypedDict):
    name: str
    schema: dict[str, Any]


class ModelRequest(_ModelRequestBase, total=False):
    max_output_tokens: int
    deadline_unix_ms: int
    context: ContextReport
    structured_output: StructuredOutput


class ProviderState(TypedDict):
    provider: Literal['gemini']
    version: Literal[1]
    parts: list[dict[str, Any]]


class ModelReply(TypedDict, total=False):
    content: str | None
    tool_calls: list[dict[str, Any]]
    usage: dict[str, int]
    provider_state: ProviderState


class Provider(Protocol):
    def complete(self, request: ModelRequest) -> ModelReply | Awaitable[ModelReply]: ...


class StreamingProvider(Protocol):
    def stream(self, request: ModelRequest) -> AsyncIterator[dict[str, Any]]: ...


class ProviderContext(TypedDict):
    cancel_event: asyncio.Event


class DeadlineExceeded(TimeoutError):
    """The persisted deadline of the whole run has elapsed."""


class ToolOperationUncertain(RuntimeError):
    """The external tool outcome must be reconciled, never converted to model feedback."""


class VerificationUncertain(ToolOperationUncertain):
    """Inspect and reconcile a verification before repeating its callback."""
    def __init__(self, message='VERIFICATION_OPERATION_UNCERTAIN: inspect and reconcile the verification before repeating it'):
        super().__init__(message)


class SummaryUncertain(ToolOperationUncertain):
    """The summary result must be reconciled before generating it again."""
    def __init__(self, message='SUMMARY_OPERATION_UNCERTAIN: inspect and reconcile the summary before repeating it'):
        super().__init__(message)


class TriggerUncertain(ToolOperationUncertain):
    """A dispatched trigger occurrence needs explicit reconciliation."""
    def __init__(self, message='TRIGGER_OPERATION_UNCERTAIN: inspect and reconcile the trigger before dispatching another occurrence'):
        super().__init__(message)


class ToolContext(dict[str, Any]):
    """JSON-serializable invocation IDs plus a non-persisted cancellation attribute."""
    delegate: Callable[[Agent, str], Awaitable[dict[str, Any]]]
    def __init__(self, *, run_id, execution_id, tool_call_id, cancel_event):
        super().__init__(run_id=run_id, execution_id=execution_id, tool_call_id=tool_call_id)
        self.cancel_event = cancel_event


@dataclass(frozen=True)
class Tool:
    name: str
    description: str
    parameters: dict[str, Any]
    execute: Callable[..., Any]
    allow_repeated_results: bool = False
    with_context: bool = False


class _NativeHttpProvider:
    with_context = True

    def __init__(self, config):
        self._config = json.dumps(config, allow_nan=False)
        self._native = _native.NativeProvider(self._config)

    async def _events(self, request, streaming, context=None):
        cancel_event = (context or {}).get("cancel_event")
        if cancel_event is not None and cancel_event.is_set():
            raise asyncio.CancelledError
        handle = self._native.start(json.dumps(request, allow_nan=False), streaming)
        watcher = None
        if cancel_event is not None:
            async def watch():
                await cancel_event.wait()
                handle.cancel()
            watcher = asyncio.create_task(watch())
        try:
            while True:
                try:
                    raw = await asyncio.to_thread(handle.next)
                except RuntimeError as error:
                    if str(error).startswith("CANCELLED:"):
                        raise asyncio.CancelledError from error
                    if str(error).startswith("TIMEOUT:"):
                        raise TimeoutError(str(error)) from error
                    if str(error).startswith("DEADLINE_EXCEEDED:"):
                        raise DeadlineExceeded(str(error)) from error
                    raise
                if raw is None:
                    break
                yield json.loads(raw)
        finally:
            handle.cancel()
            if watcher is not None:
                watcher.cancel()
                await asyncio.gather(watcher, return_exceptions=True)

    def stream(self, request, context=None) -> AsyncGenerator[dict[str, Any], None]:
        return self._events(request, True, context)

    async def complete(self, request: ModelRequest, context=None) -> ModelReply:
        reply = None
        async for event in self._events(request, False, context):
            if event["type"] == "reply":
                reply = event["reply"]
        if reply is None:
            raise RuntimeError("PROVIDER_ERROR: missing model reply")
        return reply


def _optional(**values: Any) -> dict[str, Any]:
    return {key: value for key, value in values.items() if value is not None}


class OpenAICompatible(_NativeHttpProvider):
    def __init__(self, *, base_url: str = "https://api.openai.com/v1",
                 api_key: str | None = None, timeout_ms: int = 60_000,
                 max_response_bytes: int = 16 * 1024 * 1024,
                 output_token_parameter: str = "max_completion_tokens",
                 resilience: dict[str, Any] | None = None):
        super().__init__({"kind": "openai_compatible", "base_url": base_url, "api_key": api_key,
            "timeout_ms": timeout_ms, "max_response_bytes": max_response_bytes,
            "output_token_parameter": output_token_parameter, **_optional(resilience=resilience)})


class HiveAgents(_NativeHttpProvider):
    def __init__(self, *, base_url: str, api_key: str | None = None,
                 timeout_ms: int = 120_000, max_response_bytes: int = 16 * 1024 * 1024,
                 context_window: int = 50_000):
        super().__init__({'kind': 'hiveagents', 'base_url': base_url, 'api_key': api_key,
                          'timeout_ms': timeout_ms, 'max_response_bytes': max_response_bytes,
                          'context_window': context_window})


class PresetProvider(_NativeHttpProvider):
    """A hosted provider by id (see docs/providers.md). Protocol, base URL and wire
    profile come from the preset; anything given here wins over it."""

    def __init__(self, preset: str, *, api_key: str | None = None, base_url: str | None = None,
                 timeout_ms: int | None = None, max_response_bytes: int | None = None,
                 profile: dict[str, Any] | None = None, resilience: dict[str, Any] | None = None):
        given = {'api_key': api_key, 'base_url': base_url, 'timeout_ms': timeout_ms,
                 'max_response_bytes': max_response_bytes, 'profile': profile, 'resilience': resilience}
        super().__init__({'preset': preset, **{key: value for key, value in given.items() if value is not None}})


class Anthropic(_NativeHttpProvider):
    def __init__(self, *, base_url: str = "https://api.anthropic.com/v1",
                 api_key: str | None = None, timeout_ms: int = 60_000,
                 max_response_bytes: int = 16 * 1024 * 1024,
                 api_version: str = "2026-10-21", default_max_output_tokens: int = 4096,
                 resilience: dict[str, Any] | None = None):
        super().__init__({"kind": "anthropic", "base_url": base_url, "api_key": api_key,
            "timeout_ms": timeout_ms, "max_response_bytes": max_response_bytes,
            "api_version": api_version, "default_max_output_tokens": default_max_output_tokens,
            **_optional(resilience=resilience)})


class Gemini(_NativeHttpProvider):
    def __init__(self, *, base_url: str = "https://generativelanguage.googleapis.com/v1beta",
                 api_key: str | None = None, timeout_ms: int = 60_000,
                 max_response_bytes: int = 16 * 1024 * 1024, default_max_output_tokens: int = 4096,
                 resilience: dict[str, Any] | None = None):
        super().__init__({"kind": "gemini", "base_url": base_url, "api_key": api_key,
            "timeout_ms": timeout_ms, "max_response_bytes": max_response_bytes,
            "default_max_output_tokens": default_max_output_tokens, **_optional(resilience=resilience)})


async def _invoke(callback: Callable, *args: Any) -> Any:
    # Synchronous host callbacks must not block the application's asyncio loop.
    if inspect.iscoroutinefunction(callback):
        return await callback(*args)
    result = await asyncio.to_thread(callback, *args)
    return await result if inspect.isawaitable(result) else result


class SessionStore:
    async def open_secrets(self, *, key: bytes, id="default", key_id="primary"):
        from .secrets import SecretVault
        return await SecretVault.open(self, key=key, id=id, key_id=key_id)
    async def open_queue(self, id='default', *, policy=None):
        from .queue import DurableQueue
        return await DurableQueue.open(self, id=id, policy=policy)
    @property
    def services(self):
        from .services import CatalogServices
        return CatalogServices(self)

    def artifacts(self, root: str):
        """Tenant-scoped artifact store with its files under ``root``."""
        from .artifacts import ArtifactStore
        return ArtifactStore(self, root)

    def __init__(self, path: str, *, tenant_id: str = "default"):
        self._native = _native.NativeSessionStore(path, tenant_id)
        self._initialize_guards()

    def _initialize_guards(self):
        self._active = 0
        self._exclusive = False
        self._state_lock = threading.RLock()

    @classmethod
    async def open(cls, path: str, *, tenant_id: str = "default") -> SessionStore:
        handle = await storage_call(_native.NativeSessionStore, path, tenant_id, discard=lambda handle: handle.close())
        store = object.__new__(cls); store._native = handle; store._initialize_guards()
        return store

    def _available(self):
        with self._state_lock:
            if self._exclusive: raise RuntimeError('BUSY: store maintenance is pending')

    def _maintenance(self):
        with self._state_lock:
            self._available()
            if self._active: raise RuntimeError('BUSY: stop active operations before store maintenance')
            self._exclusive = True

    def _end_maintenance(self):
        with self._state_lock: self._exclusive = False

    def for_tenant(self, tenant_id: str) -> SessionStore:
        self._retain()
        try:
            store = object.__new__(SessionStore); store._native = self._native.for_tenant(tenant_id); store._initialize_guards()
            return store
        finally: self._release()

    async def for_tenant_async(self, tenant_id: str) -> SessionStore:
        self._retain()
        try:
            handle = await storage_call(self._native.for_tenant, tenant_id, discard=lambda handle: handle.close())
            store = object.__new__(SessionStore); store._native = handle; store._initialize_guards()
            return store
        finally: self._release()

    def _dispatch(self, command: dict[str, Any], exclusive=False) -> Any:
        encoded = json.dumps(command, allow_nan=False)
        if exclusive: self._maintenance()
        else: self._retain()
        try: return json.loads(self._native.dispatch(encoded))
        except RuntimeError as error:
            if str(error).startswith('OBJECTIVE_LIMIT:'): raise ObjectiveLimit(str(error)) from error
            raise
        finally:
            if exclusive: self._end_maintenance()
            else: self._release()

    async def _dispatch_async(self, command, exclusive=False, on_result=None):
        encoded = json.dumps(command, allow_nan=False)
        if exclusive: self._maintenance()
        else: self._retain()
        try:
            callback = None if on_result is None else lambda raw: on_result(json.loads(raw))
            raw = await storage_call(self._native.dispatch, encoded, on_result=callback)
            return json.loads(raw)
        except RuntimeError as error:
            if str(error).startswith('OBJECTIVE_LIMIT:'): raise ObjectiveLimit(str(error)) from error
            raise
        finally:
            if exclusive: self._end_maintenance()
            else: self._release()

    def _release(self) -> None:
        with self._state_lock: self._active -= 1

    def _retain(self) -> None:
        with self._state_lock:
            self._available(); self._active += 1

    def _open_agent(self, agent_id, session_id, config, resume):
        self._retain()
        try: return self._native.open_agent(agent_id, session_id, json.dumps(config, allow_nan=False), resume)
        finally: self._release()

    async def _open_agent_async(self, agent_id, session_id, config, resume):
        encoded = json.dumps(config, allow_nan=False)
        self._retain()
        try: return await storage_call(self._native.open_agent, agent_id, session_id, encoded, resume)
        finally: self._release()

    def recover(self, *, agent_id: str, session_id: str) -> int:
        return self._dispatch({"op": "recover", "agent_id": agent_id, "session_id": session_id}, exclusive=True)

    async def recover_async(self, *, agent_id: str, session_id: str) -> int:
        return await self._dispatch_async({"op": "recover", "agent_id": agent_id, "session_id": session_id}, exclusive=True)

    def inspect(self, *, agent_id: str, session_id: str, run_id: str | None = None) -> dict | None:
        return self._dispatch({"op": "inspect", "agent_id": agent_id, "session_id": session_id, "run_id": run_id})

    async def inspect_async(self, *, agent_id: str, session_id: str, run_id: str | None = None) -> dict | None:
        return await self._dispatch_async({"op": "inspect", "agent_id": agent_id, "session_id": session_id, "run_id": run_id})

    def resolve_tool(self, *, agent_id: str, session_id: str, revision: int, execution_id: str, result: Any) -> dict:
        return self._dispatch({"op": "resolve", "agent_id": agent_id, "session_id": session_id,
            "revision": revision, "execution_id": execution_id, "outcome": {"type": "tool", "result": result}}, exclusive=True)

    async def resolve_tool_async(self, *, agent_id: str, session_id: str, revision: int, execution_id: str, result: Any) -> dict:
        return await self._dispatch_async({"op": "resolve", "agent_id": agent_id, "session_id": session_id,
            "revision": revision, "execution_id": execution_id, "outcome": {"type": "tool", "result": result}}, exclusive=True)

    def resolve_model(self, *, agent_id: str, session_id: str, revision: int, execution_id: str, reply: ModelReply) -> dict:
        return self._dispatch({"op": "resolve", "agent_id": agent_id, "session_id": session_id,
            "revision": revision, "execution_id": execution_id, "outcome": {"type": "model", "reply": reply}}, exclusive=True)

    async def resolve_model_async(self, *, agent_id: str, session_id: str, revision: int, execution_id: str, reply: ModelReply) -> dict:
        return await self._dispatch_async({"op": "resolve", "agent_id": agent_id, "session_id": session_id,
            "revision": revision, "execution_id": execution_id, "outcome": {"type": "model", "reply": reply}}, exclusive=True)

    def close(self) -> None:
        self._maintenance()
        try: self._native.close()
        finally: self._end_maintenance()

    async def close_async(self) -> None:
        self._maintenance()
        try: await storage_call(self._native.close)
        finally: self._end_maintenance()


class Agent:
    def __init__(self, *, model: str, system: str | None = None,
                 tools: list[Tool] | None = None, provider: Provider | StreamingProvider | None = None,
                 max_turns: int = 8, policy: dict[str, Any] | None = None,
                 on_event: Callable | None = None, store: SessionStore | None = None,
                 agent_id: str | None = None, session_id: str | None = None, resume: bool = False,
                 objective: Objective | None = None):
        self._initialize(False, model=model, system=system, tools=tools, provider=provider, max_turns=max_turns,
            policy=policy, on_event=on_event, store=store, agent_id=agent_id, session_id=session_id, resume=resume, objective=objective)

    @classmethod
    async def open(cls, **options) -> Agent:
        agent = object.__new__(cls)
        agent._initialize(True, **options)
        if agent._load_options is not None:
            config, resume = agent._load_options
            agent._store._retain()
            try:
                agent._durable = await agent._store._open_agent_async(agent._session['agent_id'], agent._session['session_id'], config, resume)
                frame = await storage_call(agent._durable.dispatch, '{"op":"frame"}')
                agent._loaded(json.loads(frame), resume)
            finally:
                agent._store._release(); agent._load_options = None
        return agent

    def _loaded(self, loaded, resume):
        self._revision = loaded['revision']; self._frame = loaded; self._resume_pending = resume
        self._resume_depth = loaded.get('objective_depth', 0); self._resume_summary_key = loaded.get('summary_key')

    def _initialize(self, defer_load, /, *, model: str, system: str | None = None,
                 tools: list[Tool] | None = None, provider: Provider | StreamingProvider | None = None,
                 max_turns: int = 8, policy: dict[str, Any] | None = None,
                 on_event: Callable | None = None, store: SessionStore | None = None,
                 agent_id: str | None = None, session_id: str | None = None, resume: bool = False,
                 objective: Objective | None = None):
        if objective is not None and not isinstance(objective, Objective):
            raise TypeError('objective must be an Objective')
        if objective is not None: objective._validate_store(store)
        self._objective = objective
        tools = list(tools or [])
        for tool in tools:
            if not callable(tool.execute):
                raise TypeError(f"Tool {tool.name} needs execute")
        self._tools = {tool.name: tool for tool in tools}
        self._provider = provider if provider is not None else OpenAICompatible()
        if not callable(getattr(self._provider, "complete", None)) and not callable(getattr(self._provider, "stream", None)):
            raise TypeError("Provider needs complete or stream")
        self._on_event = on_event
        self._busy = False
        self._closed = False
        config = {"op": "create", "model": model, "system": system,
            "max_turns": max_turns, "policy": policy if policy is not None else {},
            "objective_id": objective.id if objective is not None else None,
            "tools": [{"name": t.name, "description": t.description,
                "parameters": t.parameters, "allow_repeated_results": t.allow_repeated_results} for t in tools]}
        self._engine = _native.NativeEngine(json.dumps(config, allow_nan=False)) if store is None else None
        self._durable = None
        self._frame = self._memory({'op': 'frame'}) if self._engine is not None else None
        self._store = store
        self._session = {"agent_id": agent_id, "session_id": session_id}
        self._revision = 0
        self._resume_pending = False
        self._resume_depth = 0
        self._resume_summary_key = None
        self._summary_cancel = None
        self._cancel_event = None
        self._operation_task = None
        self._cancel_reason = None
        self._load_options = None
        if store is not None:
            if not isinstance(agent_id, str) or not isinstance(session_id, str):
                raise TypeError("Persistent agents require explicit agent_id and session_id")
            if not isinstance(store, SessionStore):
                raise TypeError("store must be a SessionStore")
            if defer_load:
                self._load_options = (config, resume)
                return
            self._durable = store._open_agent(agent_id, session_id, config, resume)
            self._loaded(self._query({'op': 'frame'}), resume)
        elif agent_id is not None or session_id is not None or resume:
            raise TypeError("agent_id and session_id require a SessionStore")

    @property
    def messages(self) -> list[dict[str, Any]]:
        return self._query({'op': 'messages'})

    @property
    def context_plan(self) -> ContextPlan | None:
        return self._query({'op': 'context_plan'})

    def _memory(self, command):
        return json.loads(self._engine.dispatch(json.dumps(command, allow_nan=False)))

    def _query(self, command):
        handle = self._durable if self._durable is not None else self._engine
        try: return json.loads(handle.dispatch(json.dumps(command, allow_nan=False)))
        except RuntimeError as error:
            if str(error).startswith('OBJECTIVE_LIMIT:'): raise ObjectiveLimit(str(error)) from error
            raise

    async def _persist(self, command):
        try: raw = await storage_call(self._durable.dispatch, json.dumps(command, allow_nan=False))
        except RuntimeError as error:
            if isinstance(error, RuntimeError) and str(error).startswith('OBJECTIVE_LIMIT:'): raise ObjectiveLimit(str(error)) from error
            raise
        return json.loads(raw)

    def _check_context(self):
        if self._closed: raise RuntimeError('CLOSED: agent is closed')
        if self._busy: raise RuntimeError('BUSY: cannot inspect or change context during an active run')

    def capture_context(self, start: int, end: int) -> ContextSource:
        self._check_context()
        return self._query({'op': 'capture_context', 'start': start, 'end': end})

    def read_context(self, source: ContextSource) -> list[dict[str, Any]]:
        self._check_context()
        return self._query({'op': 'read_context', 'source': source})

    def search_context(self, query: str, *, max_turns: int = 4, max_bytes: int = 64 * 1024) -> ContextSearchResult:
        self._check_context()
        return self._query({'op': 'search_context', 'query': query, 'options': {'max_turns': max_turns, 'max_bytes': max_bytes}})

    def set_context_plan(self, plan: ContextPlan | None) -> None:
        self._check_context()
        if self._resume_pending: raise RuntimeError('INVALID_STATE: context cannot change while a checkpoint awaits resume')
        if self._store is not None: self._store._retain()
        try:
            self._frame = self._query({'op': 'set_context_plan', 'plan': plan})
            if self._store is not None: self._revision = self._frame['revision']
        finally:
            if self._store is not None: self._store._release()

    async def set_context_plan_async(self, plan: ContextPlan | None) -> None:
        self._check_context()
        if self._resume_pending: raise RuntimeError('INVALID_STATE: context cannot change while a checkpoint awaits resume')
        encoded = json.dumps({'op': 'set_context_plan', 'plan': plan}, allow_nan=False)
        if self._store is not None: self._store._retain()
        self._busy = True
        def accept(raw):
            self._frame = json.loads(raw)
            if self._store is not None: self._revision = self._frame['revision']
        handle = self._durable if self._durable is not None else self._engine
        try: await storage_call(handle.dispatch, encoded, on_result=accept)
        finally:
            self._busy = False
            if self._store is not None: self._store._release()

    async def summarize(self, key: str, source: ContextSource, callback: Callable, *, objective=None, cancel_event=None) -> SummaryResult:
        self._check_context()
        if self._resume_pending: raise RuntimeError('INVALID_STATE: summary generation requires finishing the pending checkpoint')
        objective = objective if objective is not None else self._objective
        if not isinstance(objective, Objective): raise TypeError('Summary generation requires an Objective')
        if self._objective is not None and not self._objective._same(objective): raise ObjectiveLimit('OBJECTIVE_LIMIT: summary cannot change objective')
        objective._validate_store(self._store)
        first = next((i for i, message in enumerate(self.messages) if message['role'] == 'user'), None)
        if source['start'] != first: raise ValueError('Summary source must start at the first user turn')
        value = {'source': source, 'messages': self.read_context(source)}
        def track(cancel):
            self._summary_cancel = cancel
            def detach(): self._summary_cancel = None
            return detach
        self._busy = True
        try: return await objective.summarize(key, value, callback, cancel_event=cancel_event, _track_cancel=track)
        finally: self._busy = False; self._summary_cancel = None

    async def run(self, prompt: Prompt, *, cancel_event=None) -> dict[str, Any]:
        return await self._collect(self._drive(prompt, resume=False, streaming=False, external_cancel=cancel_event))

    async def _run_queued(self, prompt, *, store, objective_id, depth, cancel_event, resume=False, binding=None):
        if not objective_id or self._store is not store or self._objective is None or self._objective.id != objective_id:
            raise ObjectiveLimit('OBJECTIVE_LIMIT: queued agents must bind the job root and SessionStore')
        if binding and (self._session is None or self._session.get('agent_id') != binding['agent_id'] or self._session.get('session_id') != binding['session_id']):
            raise ValueError('DELEGATION_BINDING_MISMATCH: bind the assigned specialist and durable session')
        return await self._collect(self._drive(prompt, resume=resume, streaming=False, external_cancel=cancel_event,
                                               inherited_objective=self._objective, depth=depth))

    async def resume(self, *, cancel_event=None) -> dict[str, Any]:
        return await self._collect(self._drive(None, resume=True, streaming=False, external_cancel=cancel_event))

    def stream(self, prompt: Prompt, *, cancel_event=None) -> AsyncGenerator[dict[str, Any], None]:
        return self._drive(prompt, resume=False, streaming=True, external_cancel=cancel_event)

    def resume_stream(self, *, cancel_event=None) -> AsyncGenerator[dict[str, Any], None]:
        return self._drive(None, resume=True, streaming=True, external_cancel=cancel_event)

    def cancel(self) -> bool:
        if not self._busy:
            return False
        if self._summary_cancel is None and self._cancel_event is None: return False
        if self._summary_cancel is not None: self._summary_cancel('cancelled')
        else: self._request_cancel("cancelled")
        return True

    def _request_cancel(self, reason):
        if self._cancel_reason is None:
            self._cancel_reason = reason
        self._cancel_event.set()
        if self._operation_task is not None:
            self._operation_task.cancel()

    async def _operation(self, callback, *args):
        cancellable = inspect.iscoroutinefunction(callback)
        task = asyncio.create_task(_invoke(callback, *args))
        self._operation_task = task if cancellable else None
        try:
            if cancellable:
                return await task
            try:
                return await asyncio.shield(task)
            except asyncio.CancelledError:
                if self._cancel_reason is None:
                    self._cancel_reason = "cancelled"
                self._cancel_event.set()
                return await asyncio.shield(task)
        finally:
            self._operation_task = None

    async def _collect(self, events):
        result = None
        async for event in events:
            if event["type"] == "result":
                result = event["result"]
        if result is None:
            raise RuntimeError("INVALID_STATE: execution ended without a result")
        return result

    def _check_cancel(self, terminal=False):
        if self._cancel_event.is_set():
            if self._cancel_reason == "deadline":
                if terminal: return
                raise DeadlineExceeded("DEADLINE_EXCEEDED: run deadline elapsed")
            raise asyncio.CancelledError

    async def _drive(self, prompt: str | None, *, resume: bool, streaming: bool, external_cancel=None,
                     inherited_objective=None, depth=0, summary_key=None):
        if self._closed:
            raise RuntimeError("CLOSED: agent is closed")
        if self._busy:
            raise RuntimeError("BUSY: agent already has an active run")
        if resume != self._resume_pending:
            raise RuntimeError("INVALID_STATE: use resume() only with an agent opened with resume=True")
        if external_cancel is not None and external_cancel.is_set():
            raise asyncio.CancelledError
        objective = inherited_objective if inherited_objective is not None else self._objective
        depth = max(depth, self._resume_depth if resume else 0)
        summary_key = summary_key if summary_key is not None else (self._resume_summary_key if resume else None)
        def scoped_command(op, **fields):
            command = {'op': 'summary_' + op if summary_key is not None else op, 'depth': depth, **fields}
            if summary_key is not None: command['key'] = summary_key
            return command
        if inherited_objective is not None and self._objective is not None and not self._objective._same(objective):
            raise ObjectiveLimit('OBJECTIVE_LIMIT: child cannot change objective')
        terminal_resume = resume and self._frame['action']['type'] in ('complete', 'stopped')
        if objective is not None:
            objective._validate_store(self._store)
            if self._store is not None and objective.id != self._frame['state'].get('objective_id'):
                raise ObjectiveLimit('OBJECTIVE_LIMIT: persistent child must bind the inherited objective at construction')
        self._busy = True
        self._cancel_event = asyncio.Event()
        self._cancel_reason = None
        watcher = None
        deadline_task = None
        if external_cancel is not None:
            async def watch():
                await external_cancel.wait()
                self.cancel()
            watcher = asyncio.create_task(watch())
        claimed = False
        detach_objective = lambda: None
        try:
            if objective is not None:
                detach_objective = objective._attach(self.cancel, depth, terminal_resume)
                if not terminal_resume: await objective._dispatch_async({'op': 'check', 'depth': depth})
                if not terminal_resume and summary_key is not None: await objective._dispatch_async(scoped_command('check'))
            self._check_cancel()
            if self._store is not None:
                self._store._retain()
                claimed = True
                frame = await self._persist({'op': 'resume_run'} if resume else {'op': 'begin_run', 'prompt': prompt, 'objective_depth': depth, 'summary_key': summary_key})
                self._revision = frame["revision"]
            else:
                frame = self._memory({"op": "start", "prompt": prompt})
            self._resume_pending = False
            deadlines = [frame['state'].get('deadline_unix_ms')]
            if objective is not None: deadlines.append((await objective.inspect_async())['deadline_unix_ms'])
            deadlines = [value for value in deadlines if value is not None]
            if deadlines and frame["action"]["type"] not in ("complete", "stopped"):
                deadline = min(deadlines)
                async def watch_deadline():
                    while (remaining := deadline - time.time_ns() // 1_000_000) > 0:
                        await asyncio.sleep(remaining / 1000)
                    if self._busy: self._request_cancel("deadline")
                deadline_task = asyncio.create_task(watch_deadline())

            async def preflight():
                nonlocal frame
                if frame["state"].get("deadline_unix_ms") is None or frame["action"]["type"] in ("complete", "stopped"):
                    return
                if self._store is not None:
                    frame = await self._persist({"op": "check"})
                    self._revision = frame["revision"]
                else:
                    frame = self._memory({"op": "tick"})
            while True:
                if objective is not None and (self._store is None or frame['action']['type'] not in ('complete', 'stopped')):
                    await objective._dispatch_async({'op': 'check', 'depth': depth})
                await preflight()
                action = frame["action"]
                terminal = action["type"] in ("complete", "stopped")
                if terminal and deadline_task is not None: deadline_task.cancel()
                self._check_cancel(terminal)
                if self._on_event is not None:
                    await _invoke(self._on_event, copy.deepcopy(action))
                await preflight()
                if frame["action"]["type"] != action["type"]: continue
                action = frame["action"]
                self._check_cancel(terminal)
                yield copy.deepcopy(action)
                await preflight()
                if frame["action"]["type"] != action["type"]: continue
                action = frame["action"]
                self._check_cancel(terminal)
                if action["type"] in ("complete", "stopped"):
                    if self._store is not None:
                        self._revision = await self._persist({"op": "commit"})
                    else: self._memory({'op': 'commit'})
                    self._frame = frame
                    result = {"status": "completed" if action["type"] == "complete" else "stopped",
                            "content": action.get("content"), "stop_reason": action.get("reason"),
                            "stats": action["stats"]}
                    yield {"type": "result", "result": result}
                    return
                if action["type"] == "model":
                    permit = None
                    request = copy.deepcopy(action['request'])
                    if objective is not None and self._store is None:
                        permit = await objective._dispatch_async(scoped_command('model', cap=request.get('max_output_tokens')))
                        request['max_output_tokens'] = permit['max_output_tokens']
                        if permit['deadline_unix_ms'] is not None:
                            request['deadline_unix_ms'] = min(request.get('deadline_unix_ms', permit['deadline_unix_ms']), permit['deadline_unix_ms'])
                    if self._store is not None:
                        frame = await self._persist({"op": "claim"})
                        self._revision = frame["revision"]
                        if frame["action"]["type"] != action["type"]: continue
                        action = frame["action"]
                        request = copy.deepcopy(action['request'])
                    self._check_cancel()
                    context = {"cancel_event": self._cancel_event}
                    args = (request, context) if getattr(self._provider, "with_context", False) else (request,)
                    has_stream = callable(getattr(self._provider, "stream", None))
                    if has_stream and (streaming or not callable(getattr(self._provider, "complete", None))):
                        iterator = self._provider.stream(*args).__aiter__()
                        reply = None
                        async def next_event():
                            return await iterator.__anext__()
                        try:
                            while True:
                                try:
                                    event = await self._operation(next_event)
                                except StopAsyncIteration:
                                    break
                                self._check_cancel()
                                if reply is not None:
                                    raise RuntimeError("PROVIDER_ERROR: events after final reply")
                                if event["type"] == "reply":
                                    reply = event["reply"]
                                elif event["type"] in ("text_delta", "tool_call_delta", "usage"):
                                    yield copy.deepcopy(event)
                                else:
                                    raise RuntimeError("PROVIDER_ERROR: unknown stream event")
                        finally:
                            close = getattr(iterator, "aclose", None)
                            if close is not None:
                                await close()
                        if reply is None:
                            raise RuntimeError("PROVIDER_ERROR: stream ended without a model reply")
                    else:
                        reply = await self._operation(self._provider.complete, *args)
                    if permit is not None:
                        await objective._dispatch_async({'op': 'settle', 'id': permit['id'], 'usage': reply.get('usage')})
                    if self._store is not None:
                        frame = await self._persist({"op": "advance", "execution_id": frame["operation"]["execution_id"],
                            "outcome": {"type": "model", "reply": reply}})
                        self._revision = frame["revision"]
                    else:
                        frame = self._memory({"op": "reply", "reply": reply})
                elif action["type"] == "tool_rejected":
                    if objective is not None and self._store is None: await objective._dispatch_async(scoped_command('tool'))
                    if self._store is not None:
                        frame = await self._persist({"op": "reject"})
                        self._revision = frame["revision"]
                    else:
                        frame = self._memory({"op": "reject"})
                elif action["type"] == "tool":
                    if objective is not None and self._store is None: await objective._dispatch_async(scoped_command('tool'))
                    call = action["call"]
                    if self._store is not None:
                        frame = await self._persist({"op": "claim"})
                        self._revision = frame["revision"]
                        if frame["action"]["type"] != action["type"]: continue
                        action = frame["action"]
                    self._check_cancel()
                    children = []
                    accepting_children = True
                    def delegate(child, child_prompt):
                        if not accepting_children:
                            raise RuntimeError('CLOSED: tool invocation has ended')
                        if objective is None:
                            raise ObjectiveLimit('OBJECTIVE_LIMIT: delegation requires an Objective')
                        if not isinstance(child, Agent):
                            raise TypeError('delegate requires an Agent')
                        if child._busy: raise RuntimeError('BUSY: child agent already has an active run')
                        # Fail before creating coroutine objects in a synchronous host thread.
                        asyncio.get_running_loop()
                        task = asyncio.create_task(child._collect(child._drive(child_prompt, resume=False, streaming=False,
                            external_cancel=self._cancel_event, inherited_objective=objective, depth=depth + 1, summary_key=summary_key)))
                        children.append((child, task))
                        return task
                    try:
                        tool = self._tools[call["name"]]
                        context = ToolContext(run_id=frame.get("run_id"),
                            execution_id=(frame.get("operation") or {}).get("execution_id"),
                            tool_call_id=call["id"], cancel_event=self._cancel_event)
                        context.delegate = delegate
                        args = (call["arguments"], context) if tool.with_context else (call["arguments"],)
                        result = await self._operation(tool.execute, *args)
                        accepting_children = False
                        if children: await asyncio.gather(*(task for _, task in children))
                        result = json.loads(json.dumps(result, allow_nan=False))
                    except (ToolOperationUncertain, ObjectiveLimit):
                        for child, task in children:
                            if not task.done() and not child.cancel(): task.cancel()
                        raise
                    except Exception as error:
                        for child, task in children:
                            if not task.done() and not child.cancel(): task.cancel()
                        if children:
                            raise ToolOperationUncertain('DELEGATION_FAILED: inspect child outcomes before continuing') from error
                        self._check_cancel()
                        result = {"error": {"message": str(error)}}
                    finally:
                        accepting_children = False
                        if children:
                            for child, task in children:
                                if not task.done() and not child.cancel(): task.cancel()
                            await asyncio.gather(*(task for _, task in children), return_exceptions=True)
                    if self._store is not None:
                        frame = await self._persist({"op": "advance", "execution_id": frame["operation"]["execution_id"],
                            "outcome": {"type": "tool", "result": result}})
                        self._revision = frame["revision"]
                    else:
                        frame = self._memory({"op": "tool_result",
                                             "id": call["id"], "result": result})
                else:
                    raise RuntimeError(f"INVALID_STATE: unexpected action {action['type']}")
        except asyncio.CancelledError:
            if self._cancel_reason == "deadline":
                raise DeadlineExceeded("DEADLINE_EXCEEDED: run deadline elapsed") from None
            raise
        except Exception:
            self._check_cancel()
            raise
        finally:
            detach_objective()
            self._cancel_event.set()
            if claimed:
                self._store._release()
            if self._engine is not None: self._memory({'op': 'abort'})
            self._busy = False
            self._cancel_event = None
            tasks = [task for task in (watcher, deadline_task) if task is not None]
            for task in tasks: task.cancel()
            if tasks: await asyncio.gather(*tasks, return_exceptions=True)

    def close(self) -> None:
        if self._busy:
            raise RuntimeError("BUSY: cannot close an active agent")
        self._closed = True
