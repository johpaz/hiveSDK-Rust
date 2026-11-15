"""Shared objective accounting, optionally durable through the Rust SessionStore."""
from __future__ import annotations

import asyncio
import json
import threading
import time
from typing import Any, Awaitable, Callable
from . import _native
from ._storage import _join


class ObjectiveLimit(RuntimeError):
    """The shared objective denied dispatch before any new operation."""


class VerificationContext(dict[str, Any]):
    """Serializable invocation IDs with cancellation/delegation as attributes."""
    cancel_event: asyncio.Event
    delegate: Callable[[Any, str], Awaitable[dict[str, Any]]]


class SummaryContext(VerificationContext):
    """Summary execution identity with scoped, budgeted delegation."""


class Objective:
    def __init__(self, policy: dict | None = None, *, store=None, objective_id: str | None = None):
        self._initialize(False, policy, store=store, objective_id=objective_id)

    @classmethod
    async def open(cls, policy: dict | None = None, *, store=None, objective_id: str | None = None):
        objective = object.__new__(cls)
        objective._initialize(True, policy, store=store, objective_id=objective_id)
        if store is not None:
            await store._dispatch_async({'op': 'open_objective', 'id': objective_id, 'policy': policy if policy is not None else {}})
        return objective

    def _initialize(self, defer_load, policy, *, store, objective_id):
        self._store = store
        self._id = objective_id
        if store is not None:
            from .agent import SessionStore
            if not isinstance(store, SessionStore) or not isinstance(objective_id, str):
                raise TypeError('Durable Objective requires a SessionStore and objective_id')
            if not defer_load: store._dispatch({'op': 'open_objective', 'id': objective_id, 'policy': policy if policy is not None else {}})
        else:
            if objective_id is not None: raise TypeError('objective_id requires a SessionStore')
            self._native = _native.NativeObjective(json.dumps(policy if policy is not None else {}, allow_nan=False))
        self._callbacks = set()
        self._lock = threading.RLock()
        self._exclusive = False

    @property
    def id(self) -> str | None:
        return self._id

    def _validate_store(self, store):
        if store is not None and store is not self._store:
            raise TypeError('Process-local Objective cannot use SessionStore; durable Objective requires the same SessionStore')

    def _same(self, other):
        return self is other or (self._store is not None and self._store is other._store and self.id == other.id)

    def _dispatch(self, command):
        with self._lock:
            if self._exclusive: raise RuntimeError('BUSY: objective reconciliation is pending')
        try:
            if self._store is not None:
                return self._store._dispatch({'op': 'objective', 'id': self.id, 'command': command})
            return json.loads(self._native.dispatch(json.dumps(command, allow_nan=False)))
        except RuntimeError as error:
            if str(error).startswith('OBJECTIVE_LIMIT:'):
                raise ObjectiveLimit(str(error)) from error
            if str(error).startswith('VERIFICATION_OPERATION_UNCERTAIN:'):
                from .agent import VerificationUncertain
                raise VerificationUncertain(str(error)) from error
            if str(error).startswith('TRIGGER_OPERATION_UNCERTAIN:'):
                from .agent import TriggerUncertain
                raise TriggerUncertain(str(error)) from error
            if str(error).startswith('SUMMARY_OPERATION_UNCERTAIN:'):
                from .agent import SummaryUncertain
                raise SummaryUncertain(str(error)) from error
            raise

    async def _dispatch_async(self, command, on_result=None):
        try:
            if self._store is not None:
                return await self._store._dispatch_async({'op': 'objective', 'id': self.id, 'command': command}, on_result=on_result)
            raw = self._native.dispatch(json.dumps(command, allow_nan=False))
            result = json.loads(raw)
            if on_result is not None: on_result(result)
            return result
        except RuntimeError as error:
            if str(error).startswith('OBJECTIVE_LIMIT:'): raise ObjectiveLimit(str(error)) from error
            if str(error).startswith('VERIFICATION_OPERATION_UNCERTAIN:'):
                from .agent import VerificationUncertain
                raise VerificationUncertain(str(error)) from error
            if str(error).startswith('SUMMARY_OPERATION_UNCERTAIN:'):
                from .agent import SummaryUncertain
                raise SummaryUncertain(str(error)) from error
            if str(error).startswith('TRIGGER_OPERATION_UNCERTAIN:'):
                from .agent import TriggerUncertain
                raise TriggerUncertain(str(error)) from error
            raise

    async def inspect_async(self) -> dict:
        with self._lock:
            if self._exclusive: raise RuntimeError('BUSY: objective reconciliation is pending')
        return await self._dispatch_async({'op': 'inspect'})

    async def _reconcile_async(self, command):
        with self._lock:
            if self._exclusive or self._callbacks: raise RuntimeError('BUSY: stop objective executors before reconciliation')
            self._exclusive = True
        try: return await self._dispatch_async(command)
        finally:
            with self._lock: self._exclusive = False

    async def resolve_summary_async(self, *, key: str, execution_id: str, content: str):
        return await self._reconcile_async({'op': 'summary_finish', 'key': key, 'execution_id': execution_id, 'content': content})

    async def resolve_verification_async(self, *, key: str, execution_id: str, verdict: dict):
        return await self._reconcile_async({'op': 'verify_finish', 'key': key, 'execution_id': execution_id, 'verdict': verdict})

    async def cancel_async(self) -> None:
        with self._lock:
            if self._exclusive: raise RuntimeError('BUSY: objective reconciliation is pending')
        def notify(_):
            with self._lock:
                for loop, callback in self._callbacks: loop.call_soon_threadsafe(callback)
        await self._dispatch_async({'op': 'cancel'}, on_result=notify)

    def inspect(self) -> dict:
        return self._dispatch({'op': 'inspect'})

    def trigger(self, trigger_id: str, schedule: dict, callback: Callable):
        from .triggers import Trigger
        return Trigger(self, trigger_id, schedule, callback)

    async def trigger_async(self, trigger_id: str, schedule: dict, callback: Callable):
        from .triggers import Trigger
        return await Trigger.open(self, trigger_id, schedule, callback)

    async def _scope(self, external_cancel, work, track_cancel=None):
        if external_cancel is not None and external_cancel.is_set(): raise asyncio.CancelledError
        cancelled = asyncio.Event()
        def request_cancel(cause='cancelled'): cancelled.set()
        detach = lambda: None; untrack = lambda: None; watcher = None; cleanup_cancelled = None
        try:
            detach = self._attach(request_cancel, 0, True)
            if track_cancel is not None: untrack = track_cancel(request_cancel)
            if external_cancel is not None:
                async def watch(): await external_cancel.wait(); cancelled.set()
                watcher = asyncio.create_task(watch())
                if external_cancel.is_set(): cancelled.set()
            if cancelled.is_set(): raise asyncio.CancelledError
            result = await work(cancelled)
            if cancelled.is_set(): raise asyncio.CancelledError
            return result
        except Exception:
            if cancelled.is_set(): raise asyncio.CancelledError
            raise
        finally:
            cancelled.set()
            try:
                if watcher is not None:
                    watcher.cancel()
                    _, cleanup_cancelled = await _join(asyncio.gather(watcher, return_exceptions=True))
            finally: untrack(); detach()
            if cleanup_cancelled is not None: raise cleanup_cancelled

    async def summarize(self, key: str, value: dict, callback: Callable, *, cancel_event=None, _track_cancel=None) -> dict:
        from .agent import SummaryUncertain
        if not callable(callback): raise TypeError('Summary needs a callback')
        if cancel_event is not None and cancel_event.is_set(): raise asyncio.CancelledError
        value = json.loads(json.dumps(value, allow_nan=False))
        async def work(scoped_cancel):
            claim = await self._dispatch_async({'op': 'summary_begin', 'key': key, 'input': value})
            if scoped_cancel.is_set(): raise asyncio.CancelledError
            if claim['result'] is not None: return claim['result']
            return await self._execute(value, callback, cancel_event=scoped_cancel,
                ids={'summary_key': key, 'execution_id': claim['execution_id']}, allow_delegate=True,
                finish=lambda content: self._dispatch_async({'op': 'summary_finish', 'key': key, 'execution_id': claim['execution_id'], 'content': content}),
                uncertain_class=SummaryUncertain, context_type=SummaryContext, summary_key=key)
        return await self._scope(cancel_event, work, _track_cancel)

    def resolve_summary(self, *, key: str, execution_id: str, content: str):
        with self._lock:
            if self._callbacks: raise RuntimeError('BUSY: stop objective executors before resolving a summary')
            return self._dispatch({'op': 'summary_finish', 'key': key, 'execution_id': execution_id, 'content': content})

    async def verify(self, key: str, value: Any, callback: Callable, *, cancel_event=None, model_review: dict | None = None) -> dict:
        from .agent import VerificationUncertain
        if not callable(callback): raise TypeError('Verification needs a callback')
        if cancel_event is not None and cancel_event.is_set(): raise asyncio.CancelledError
        value = json.loads(json.dumps(value, allow_nan=False))
        async def work(scoped_cancel):
            claim = await self._dispatch_async({'op': 'verify_begin', 'key': key, 'input': value, 'model_review': model_review})
            if scoped_cancel.is_set(): raise asyncio.CancelledError
            if claim['result'] is not None: return claim['result']
            return await self._execute(value, callback, cancel_event=scoped_cancel,
                ids={'verification_key': key, 'execution_id': claim['execution_id']}, allow_delegate=model_review is not None,
                finish=lambda verdict: self._dispatch_async({'op': 'verify_finish', 'key': key, 'execution_id': claim['execution_id'], 'verdict': verdict}),
                uncertain_class=VerificationUncertain)
        return await self._scope(cancel_event, work)

    async def _execute(self, value, callback, *, cancel_event, ids, finish, allow_delegate, uncertain_class,
                       context_type=VerificationContext, track_cancel=None, summary_key=None):
        from .agent import Agent, DeadlineExceeded, _invoke
        import inspect
        cancelled = asyncio.Event()
        operation = None
        reason = None
        children = []
        watchers = []
        open_scope = True
        detach = lambda: None
        untrack = lambda: None
        callback_cancellable = inspect.iscoroutinefunction(callback)
        cleanup_cancelled = False

        def request_cancel(cause='cancelled'):
            nonlocal reason
            if reason is None: reason = cause
            cancelled.set()
            if callback_cancellable and operation is not None: operation.cancel()

        def check_cancel():
            if cancelled.is_set():
                if reason == 'deadline': raise DeadlineExceeded('DEADLINE_EXCEEDED: objective deadline elapsed')
                raise asyncio.CancelledError

        def cancel_children():
            for child, task in children:
                if not task.done() and not child.cancel(): task.cancel()

        async def settle_cleanup(future):
            nonlocal cleanup_cancelled
            while True:
                try:
                    return await asyncio.shield(future)
                except asyncio.CancelledError:
                    cleanup_cancelled = True
                    request_cancel()
                    if future.done(): return future.result()

        try:
            detach = self._attach(request_cancel, 0)
            await self._dispatch_async({'op': 'check', 'depth': 0})
            if track_cancel is not None: untrack = track_cancel(request_cancel)
            deadline = (await self.inspect_async())['deadline_unix_ms']
            if cancel_event is not None:
                async def watch_cancel():
                    await cancel_event.wait()
                    request_cancel()
                watchers.append(asyncio.create_task(watch_cancel()))
                if cancel_event.is_set(): request_cancel()
            if deadline is not None:
                async def watch_deadline():
                    while (remaining := deadline - time.time_ns() // 1_000_000) > 0:
                        await asyncio.sleep(remaining / 1000)
                    request_cancel('deadline')
                watchers.append(asyncio.create_task(watch_deadline()))
                if deadline <= time.time_ns() // 1_000_000: request_cancel('deadline')
            check_cancel()
            context = context_type(objective_id=self.id, **ids)
            context.cancel_event = cancelled
            def delegate(child, prompt):
                if not open_scope: raise RuntimeError('CLOSED: callback has ended')
                if not allow_delegate: raise RuntimeError('VERIFIER_MODEL_DISABLED: provide model_review evaluation evidence before delegating')
                if not isinstance(child, Agent): raise TypeError('delegate requires an Agent')
                if child._busy: raise RuntimeError('BUSY: child agent already has an active run')
                check_cancel()
                asyncio.get_running_loop()
                task = asyncio.create_task(child._collect(child._drive(prompt, resume=False, streaming=False,
                    external_cancel=cancelled, inherited_objective=self, depth=1, summary_key=summary_key)))
                children.append((child, task))
                return task
            context.delegate = delegate
            operation = asyncio.create_task(_invoke(callback, value, context))
            if callback_cancellable:
                verdict = await operation
            else:
                # Repeated Task.cancel cannot release a live synchronous callback's store.
                while True:
                    try:
                        verdict = await asyncio.shield(operation)
                        break
                    except asyncio.CancelledError:
                        request_cancel()
                        if operation.done():
                            verdict = operation.result()
                            break
            open_scope = False
            if children: await asyncio.gather(*(task for _, task in children))
            result = await finish(verdict)
            check_cancel()
            return result
        except asyncio.CancelledError:
            request_cancel()
            cancel_children()
            if reason == 'deadline': raise DeadlineExceeded('DEADLINE_EXCEEDED: objective deadline elapsed') from None
            raise
        except Exception as error:
            cancel_children()
            check_cancel()
            if isinstance(error, (ObjectiveLimit, uncertain_class)): raise
            raise uncertain_class() from error
        finally:
            open_scope = False
            try:
                cancel_children()
                if children: await settle_cleanup(asyncio.gather(*(task for _, task in children), return_exceptions=True))
                cancelled.set()
                for watcher in watchers: watcher.cancel()
                if watchers: await settle_cleanup(asyncio.gather(*watchers, return_exceptions=True))
            finally:
                untrack()
                detach()
            if cleanup_cancelled: check_cancel()

    def resolve_verification(self, *, key: str, execution_id: str, verdict: dict) -> dict:
        with self._lock:
            if self._callbacks: raise RuntimeError('BUSY: stop objective executors before resolving a verification')
            return self._dispatch({'op': 'verify_finish', 'key': key, 'execution_id': execution_id, 'verdict': verdict})

    def cancel(self) -> None:
        with self._lock:
            self._dispatch({'op': 'cancel'})
            for loop, callback in self._callbacks:
                loop.call_soon_threadsafe(callback)

    def _attach(self, callback, depth, terminal=False):
        # Registration only; executors await their kernel check before callbacks.
        item = (asyncio.get_running_loop(), callback)
        with self._lock:
            if self._exclusive: raise RuntimeError('BUSY: objective reconciliation is pending')
            if self._store is not None: self._store._retain()
            self._callbacks.add(item)
        def detach():
            with self._lock:
                self._callbacks.discard(item)
                if self._store is not None: self._store._release()
        return detach
