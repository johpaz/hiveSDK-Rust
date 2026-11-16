"""Bounded artifact store: bytes under a root directory, metadata in the tenant's HiveDB.

See docs/artifacts.md. Content is verified against its recorded SHA-256 on every
read; nothing is deleted until ``expire`` runs or an artifact is deleted.
"""
from __future__ import annotations
from typing import Any


class ArtifactStore:
    def __init__(self, store, root: str):
        if not isinstance(root, str) or not root:
            raise TypeError('ArtifactStore needs a root directory')
        self._store = store
        self._root = root

    async def _call(self, command: dict[str, Any]):
        return await self._store._dispatch_async({'op': 'artifacts', 'root': self._root, 'command': command})

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

    async def expire(self, *, now: int | None = None) -> list[str]:
        """Remove the content of every active artifact whose retention elapsed; returns their ids."""
        command: dict[str, Any] = {'op': 'expire'}
        if now is not None:
            command['now'] = now
        return (await self._call(command))['expired']
