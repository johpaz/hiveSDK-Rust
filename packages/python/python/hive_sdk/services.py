"""Embedded Rust catalog services; no HTTP server or hidden model calls.

Records describe capabilities, not their executors. All methods are async and
join the storage worker before cancellation returns.
"""
from __future__ import annotations
from typing import Any, Literal, TypedDict

EntityKind = Literal['agents', 'skills', 'tools', 'memory', 'providers', 'models',
                     'ethics', 'mcp_servers', 'endpoints', 'cron']


class CatalogRecord(TypedDict):
    id: str
    kind: EntityKind
    revision: int
    data: dict[str, Any]


class CatalogPage(TypedDict):
    items: list[CatalogRecord]
    next_cursor: str | None


class CatalogHit(TypedDict):
    record: CatalogRecord
    score: float


class CatalogServices:
    def __init__(self, store):
        self._store = store

    def collection(self, kind: EntityKind) -> CatalogCollection:
        return CatalogCollection(self._store, kind)

    async def bundled_skills(self) -> list[dict]:
        """Skills that ship with the SDK. Listing enables nothing."""
        return await self._store._dispatch_async({'op': 'services', 'command': {'op': 'bundled_skills'}})

    async def import_bundled_skill(self, name: str) -> CatalogRecord:
        """Import one bundled skill (obscura_browser, office_document_manager, pdf_report_writer) into the catalog."""
        return await self._store._dispatch_async({'op': 'services', 'command': {'op': 'import_bundled_skill', 'name': name}})

    async def import_skill(self, path: str) -> CatalogRecord:
        return await self._store._dispatch_async({'op': 'services', 'command': {'op': 'import_skill', 'path': path}})

    async def create_tools(self):
        from .agent import Tool
        definitions = await self._store._dispatch_async({'op': 'services', 'command': {'op': 'tool_definitions'}})
        def bind(name):
            async def execute(arguments):
                return await self._store._dispatch_async({'op': 'services', 'command': {'op': 'execute_tool', 'name': name, 'arguments': arguments}})
            return execute
        return [Tool(name=d['name'], description=d['description'], parameters=d['parameters'],
                     execute=bind(d['name'])) for d in definitions]


class CatalogCollection:
    def __init__(self, store, kind: EntityKind):
        self._store = store
        self._kind = kind

    async def _call(self, op, **fields):
        return await self._store._dispatch_async({
            'op': 'services', 'command': {**fields, 'op': op, 'kind': self._kind}})

    async def create(self, id: str, data: dict[str, Any]) -> CatalogRecord:
        return await self._call('create', id=id, data=data)

    async def get(self, id: str) -> CatalogRecord | None:
        return await self._call('get', id=id)

    async def list(self, *, limit: int = 50, cursor: str | None = None) -> CatalogPage:
        return await self._call('list', limit=limit, cursor=cursor)

    async def update(self, id: str, revision: int, patch: dict[str, Any]) -> CatalogRecord:
        return await self._call('update', id=id, revision=revision, patch=patch)

    async def delete(self, id: str, revision: int) -> None:
        await self._call('delete', id=id, revision=revision)

    async def set_enabled(self, id: str, revision: int, enabled: bool) -> CatalogRecord:
        return await self._call('set_enabled', id=id, revision=revision, enabled=enabled)

    async def search(self, query: str, *, limit: int = 50) -> list[CatalogHit]:
        return await self._call('search', query=query, limit=limit)
