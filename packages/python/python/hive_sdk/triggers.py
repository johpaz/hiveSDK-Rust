"""Explicit trigger dispatch and opt-in calendar watcher outside the Agent loop."""
from __future__ import annotations

import asyncio
import json
import threading
import time
from typing import Callable
from .objective import Objective, VerificationContext


class TriggerContext(VerificationContext):
    """Invocation IDs are dictionary values; cancellation/delegation are attributes."""


class Trigger:
    def __init__(self, objective: Objective, trigger_id: str, schedule: dict, callback: Callable):
        self._initialize(objective, trigger_id, schedule, callback)
        objective._dispatch({'op': 'trigger_register', 'id': trigger_id, 'schedule': schedule})

    @classmethod
    async def open(cls, objective: Objective, trigger_id: str, schedule: dict, callback: Callable):
        trigger = object.__new__(cls)
        trigger._initialize(objective, trigger_id, schedule, callback)
        async def register(cancelled):
            return await objective._dispatch_async({'op': 'trigger_register', 'id': trigger_id, 'schedule': schedule})
        await objective._scope(None, register)
        return trigger

    def _initialize(self, objective, trigger_id, schedule, callback):
        if not isinstance(objective, Objective): raise TypeError('Trigger requires an Objective')
        if not callable(callback): raise TypeError('Trigger requires a callback')
        self._objective = objective
        self._id = trigger_id
        self._callback = callback
        self._lock = threading.RLock()
        self._cancellers = set()
        self._wakers = set()
        self._watching = False
        self._exclusive = False

    @property
    def id(self) -> str:
        return self._id

    def inspect(self) -> dict:
        return self._objective._dispatch({'op': 'trigger_inspect', 'id': self.id})

    async def inspect_async(self) -> dict:
        return await self._objective._dispatch_async({'op': 'trigger_inspect', 'id': self.id})

    def enable(self) -> None:
        self._objective._dispatch({'op': 'trigger_enable', 'id': self.id, 'enabled': True})

    async def enable_async(self) -> None:
        async def enable(cancelled): await self._objective._dispatch_async({'op': 'trigger_enable', 'id': self.id, 'enabled': True})
        await self._objective._scope(None, enable)

    async def disable_async(self) -> None:
        def wake(_):
            with self._lock:
                for loop, callback in self._wakers: loop.call_soon_threadsafe(callback)
        async def disable(cancelled):
            await self._objective._dispatch_async({'op': 'trigger_enable', 'id': self.id, 'enabled': False}, on_result=wake)
        await self._objective._scope(None, disable)

    def disable(self) -> None:
        self._objective._dispatch({'op': 'trigger_enable', 'id': self.id, 'enabled': False})
        with self._lock:
            for loop, wake in self._wakers: loop.call_soon_threadsafe(wake)

    def cancel(self) -> bool:
        with self._lock:
            if not self._cancellers: return False
            for loop, cancel in self._cancellers: loop.call_soon_threadsafe(cancel)
            return True

    def _track(self, cancel):
        item = (asyncio.get_running_loop(), cancel)
        with self._lock: self._cancellers.add(item)
        def detach():
            with self._lock: self._cancellers.discard(item)
        return detach

    async def _execute(self, claim, cancel_event):
        from .agent import TriggerUncertain
        if claim is None: return None
        if claim['result'] is not None: return claim['result']
        return await self._objective._execute(claim['input'], self._callback, cancel_event=cancel_event,
            ids={'trigger_id': self.id, 'occurrence_key': claim['occurrence_key'], 'execution_id': claim['execution_id']},
            finish=lambda result: self._objective._dispatch_async({'op': 'trigger_finish', 'id': self.id,
                'key': claim['occurrence_key'], 'execution_id': claim['execution_id'], 'result': result}),
            allow_delegate=True, uncertain_class=TriggerUncertain, context_type=TriggerContext, track_cancel=self._track)

    async def emit(self, key: str, value, *, cancel_event=None):
        with self._lock:
            if self._exclusive: raise RuntimeError('BUSY: trigger reconciliation is pending')
        if cancel_event is not None and cancel_event.is_set(): raise asyncio.CancelledError
        value = json.loads(json.dumps(value, allow_nan=False))
        async def work(cancelled):
            if (await self.inspect_async())['schedule']['kind'] != 'event': raise ValueError('emit requires an event trigger')
            if cancelled.is_set(): raise asyncio.CancelledError
            claim = await self._objective._dispatch_async({'op': 'trigger_emit', 'id': self.id, 'key': key, 'input': value})
            if cancelled.is_set(): raise asyncio.CancelledError
            return await self._execute(claim, cancelled)
        return await self._objective._scope(cancel_event, work, self._track)

    async def tick(self, *, cancel_event=None):
        with self._lock:
            if self._exclusive: raise RuntimeError('BUSY: trigger reconciliation is pending')
        if cancel_event is not None and cancel_event.is_set(): raise asyncio.CancelledError
        async def work(cancelled):
            if (await self.inspect_async())['schedule']['kind'] == 'event': raise ValueError('tick requires a calendar trigger')
            if cancelled.is_set(): raise asyncio.CancelledError
            claim = await self._objective._dispatch_async({'op': 'trigger_tick', 'id': self.id})
            if cancelled.is_set(): raise asyncio.CancelledError
            return await self._execute(claim, cancelled)
        return await self._objective._scope(cancel_event, work, self._track)

    def resolve(self, *, occurrence_key: str, execution_id: str, result):
        with self._lock:
            if self._exclusive or self._cancellers: raise RuntimeError('BUSY: stop trigger executors before resolving an occurrence')
            return self._objective._dispatch({'op': 'trigger_finish', 'id': self.id, 'key': occurrence_key,
                'execution_id': execution_id, 'result': result})

    async def resolve_async(self, *, occurrence_key: str, execution_id: str, result):
        with self._lock:
            if self._exclusive or self._cancellers: raise RuntimeError('BUSY: stop trigger executors before resolving an occurrence')
            self._exclusive = True
        async def resolve(cancelled):
            return await self._objective._dispatch_async({'op': 'trigger_finish', 'id': self.id, 'key': occurrence_key, 'execution_id': execution_id, 'result': result})
        try: return await self._objective._scope(None, resolve)
        finally:
            with self._lock: self._exclusive = False

    async def watch(self, *, cancel_event=None):
        from .agent import DeadlineExceeded
        with self._lock:
            if self._watching or self._exclusive: raise RuntimeError('BUSY: trigger already has an active watcher or reconciliation')
            self._watching = True
        cancelled, wake = asyncio.Event(), asyncio.Event()
        reason = None
        cleanup_cancelled = False
        detach = lambda: None
        untrack = lambda: None
        watchers = []
        wake_item = (asyncio.get_running_loop(), wake.set)

        def request_cancel(cause='cancelled'):
            nonlocal reason
            if reason is None: reason = cause
            cancelled.set(); wake.set()

        def check_cancel():
            if cancelled.is_set():
                if reason == 'deadline': raise DeadlineExceeded('DEADLINE_EXCEEDED: objective deadline elapsed')
                raise asyncio.CancelledError

        async def settle_cleanup(future):
            nonlocal cleanup_cancelled
            while True:
                try: return await asyncio.shield(future)
                except asyncio.CancelledError:
                    cleanup_cancelled = True; request_cancel()
                    if future.done(): return future.result()

        try:
            if cancel_event is not None and cancel_event.is_set(): raise asyncio.CancelledError
            detach = self._objective._attach(request_cancel, 0, True)
            untrack = self._track(request_cancel)
            initial = await self.inspect_async()
            check_cancel()
            if initial['schedule']['kind'] == 'event': raise ValueError('Event triggers use emit instead of watch')
            if not initial['enabled'] or (initial['next_due_unix_ms'] is None and initial['active_occurrence'] is None): return
            await self._objective._dispatch_async({'op': 'check', 'depth': 0})
            if cancel_event is not None:
                async def watch_cancel(): await cancel_event.wait(); request_cancel()
                watchers.append(asyncio.create_task(watch_cancel()))
                if cancel_event.is_set(): request_cancel()
            deadline = (await self._objective.inspect_async())['deadline_unix_ms']
            if deadline is not None:
                async def watch_deadline():
                    while (remaining := deadline - time.time_ns() // 1_000_000) > 0:
                        await asyncio.sleep(remaining / 1000)
                    request_cancel('deadline')
                watchers.append(asyncio.create_task(watch_deadline()))
                if deadline <= time.time_ns() // 1_000_000: request_cancel('deadline')
            while True:
                check_cancel()
                receipt = await self.tick(cancel_event=cancelled)
                if receipt is not None:
                    state = await self.inspect_async()
                    yield receipt
                    if not state['enabled'] or state['next_due_unix_ms'] is None: return
                    continue
                state = await self.inspect_async()
                if not state['enabled'] or state['next_due_unix_ms'] is None: return
                wake.clear()
                with self._lock: self._wakers.add(wake_item)
                try:
                    # A thread may disable the trigger between inspection and registration.
                    if not (await self.inspect_async())['enabled']: return
                    while (remaining := state['next_due_unix_ms'] - time.time_ns() // 1_000_000) > 0:
                        check_cancel()
                        try: await asyncio.wait_for(wake.wait(), timeout=min(remaining / 1000, 2_147_483))
                        except asyncio.TimeoutError: continue
                        break
                finally:
                    with self._lock: self._wakers.discard(wake_item)
        except asyncio.CancelledError:
            request_cancel()
            if reason == 'deadline': raise DeadlineExceeded('DEADLINE_EXCEEDED: objective deadline elapsed') from None
            raise
        except Exception:
            check_cancel()
            raise
        finally:
            request_cancel()
            try:
                for watcher in watchers: watcher.cancel()
                if watchers: await settle_cleanup(asyncio.gather(*watchers, return_exceptions=True))
            finally:
                untrack(); detach()
                with self._lock:
                    self._wakers.discard(wake_item); self._watching = False
            if cleanup_cancelled: check_cancel()
