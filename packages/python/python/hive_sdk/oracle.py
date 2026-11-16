"""Opt-in decision oracle (Jev/Kev protocol); see docs/oracle.md.

Advisory only: a failure returns a ``fallback`` reply and never raises. With an
``objective``, each call is reserved before it is sent and its usage settled
afterwards, like any model call.
"""
from __future__ import annotations
import asyncio
import json
from typing import Any
from . import _native
from ._storage import storage_call, _join


class Oracle:
    def __init__(self, config: dict[str, Any]):
        self._cap = config.get('max_output_tokens', 64)
        self._native = _native.NativeOracle(json.dumps(config, allow_nan=False))

    async def ask(self, state: Any, questions: dict[str, dict[str, Any]], *, objective=None, depth: int = 0) -> dict[str, Any]:
        permit = None
        if objective is not None and questions and not self.status()['cooling_down']:
            permit = await objective._dispatch_async({'op': 'model', 'depth': depth, 'cap': self._cap})
        payload = json.dumps({'state': state, 'questions': questions}, allow_nan=False)
        # Cancelling the awaiting task aborts the request before joining it; the
        # reservation then stays on the objective because the outcome is unknown.
        task = asyncio.create_task(storage_call(self._native.ask, payload))
        try:
            reply = json.loads(await asyncio.shield(task))
        except asyncio.CancelledError:
            self._native.cancel()
            try: await _join(task)
            except Exception: pass
            raise
        if permit is not None and reply['outcome'] == 'decision':
            await objective._dispatch_async({'op': 'settle', 'id': permit['id'], 'usage': reply['charge']})
        return reply

    def status(self) -> dict[str, Any]:
        return json.loads(self._native.status())

    def record_overruled(self, reason: str) -> bool:
        return self._native.record_overruled(reason)

    def record_agreement(self) -> None:
        self._native.record_agreement()

    def reset(self) -> None:
        self._native.reset()

    def cancel(self) -> None:
        self._native.cancel()
