"""Sandboxed filesystem, command and web tools backed by the Rust engine.

Command and web tools exist only when their policy is configured, programs run
without a shell, and every path stays inside the configured root.
"""
from __future__ import annotations
import asyncio
import json
from typing import Any
from . import _native
from ._storage import storage_call, _join


class NativeTools:
    def __init__(self, config: dict[str, Any]):
        self._native = _native.NativeTools(json.dumps(config, allow_nan=False))
        self._active = set()

    async def _call(self, command: dict[str, Any]):
        # Cancelling the awaiting task aborts the native call before joining it. A
        # command may already have run and must not be retried implicitly.
        request = self._native.fork()
        self._active.add(request)
        task = asyncio.create_task(storage_call(request.dispatch, json.dumps(command, allow_nan=False)))
        try:
            return json.loads(await asyncio.shield(task))
        except asyncio.CancelledError:
            request.cancel()
            try: await _join(task)
            except Exception: pass
            raise
        finally:
            self._active.discard(request)

    async def definitions(self) -> list[dict[str, Any]]:
        return await self._call({'op': 'tool_definitions'})

    async def execute(self, name: str, arguments: dict[str, Any]) -> Any:
        return await self._call({'op': 'execute_tool', 'name': name, 'arguments': arguments})

    async def create_tools(self):
        from .agent import Tool
        def bind(name):
            async def execute(arguments):
                return await self.execute(name, arguments)
            return execute
        return [Tool(name=d['name'], description=d['description'], parameters=d['parameters'],
                     execute=bind(d['name'])) for d in await self.definitions()]

    async def install_obscura(self, *, update: bool = False) -> dict[str, Any]:
        """Download and install the latest Obscura into ``obscura.install_dir`` (``~/.hive/bin`` by default)
        after checking its published SHA-256. Keeps an existing install unless ``update`` is true."""
        return await self._call({'op': 'install_obscura', 'update': update})

    def cancel(self) -> None:
        for request in tuple(self._active): request.cancel()

    async def close(self) -> None:
        """Stop the Obscura session the obscura_* tools started, if any; the next call starts a new one."""
        await storage_call(self._native.close)

    async def __aenter__(self): return self
    async def __aexit__(self, *_): await self.close()
