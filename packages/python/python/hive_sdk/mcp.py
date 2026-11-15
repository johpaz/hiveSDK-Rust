from __future__ import annotations

import asyncio
import copy
import json
from . import _native
from .agent import Tool, ToolOperationUncertain
from ._storage import _join


class McpOperationUncertain(ToolOperationUncertain):
    """An MCP tool response was not confirmed; inspect the durable claim before resuming."""


class McpClient:
    def __init__(self, handle):
        self._native = handle
        self._catalog = json.loads(handle.catalog())

    @classmethod
    async def connect(cls, *, command: str | None = None, args: list[str] | None = None, env: dict[str, str] | None = None,
                      cwd: str | None = None, protocol_version: str = '2025-11-25', timeout_ms: int = 30_000,
                      max_message_bytes: int = 8 * 1024 * 1024, max_tools: int = 256,
                      max_pages: int = 16, max_messages: int = 128,
                      transport: str = 'stdio', url: str | None = None, headers: dict[str, str] | None = None) -> McpClient:
        if transport == 'stdio':
            if url is not None or headers is not None: raise ValueError('HTTP options require transport=http')
            connection = {'command': command, 'args': args or [], 'env': env or {}, 'cwd': cwd}
        elif transport == 'http':
            if command is not None or args is not None or env is not None or cwd is not None: raise ValueError('Process options require transport=stdio')
            connection = {'url': url, 'headers': headers or {}}
        else: raise ValueError('Unsupported MCP transport')
        config = json.dumps({'transport': transport, **connection,
            'protocol_version': protocol_version, 'timeout_ms': timeout_ms, 'max_message_bytes': max_message_bytes,
            'max_tools': max_tools, 'max_pages': max_pages, 'max_messages': max_messages}, allow_nan=False)
        return cls(await asyncio.to_thread(_native.connect_mcp, config))

    @property
    def info(self):
        return json.loads(self._native.info())

    @property
    def catalog(self):
        return copy.deepcopy(self._catalog)

    async def list_resources(self, *, cursor=None, cancel_event=None):
        return await self._catalog_request('resources/list', {} if cursor is None else {'cursor': cursor}, cancel_event)

    async def read_resource(self, uri, *, cancel_event=None):
        return await self._catalog_request('resources/read', {'uri': uri}, cancel_event)

    async def list_prompts(self, *, cursor=None, cancel_event=None):
        return await self._catalog_request('prompts/list', {} if cursor is None else {'cursor': cursor}, cancel_event)

    async def get_prompt(self, name, arguments=None, *, cancel_event=None):
        return await self._catalog_request('prompts/get', {'name': name, 'arguments': arguments or {}}, cancel_event)

    async def _catalog_request(self, method, params, cancel_event):
        if cancel_event is not None and cancel_event.is_set(): raise asyncio.CancelledError
        handle = self._native.start_catalog_request(method, json.dumps(params, allow_nan=False))
        watcher = None
        if cancel_event is not None:
            async def watch():
                await cancel_event.wait()
                handle.cancel()
            watcher = asyncio.create_task(watch())
        task = asyncio.create_task(asyncio.to_thread(handle.result))
        try:
            try: return json.loads(await asyncio.shield(task))
            except asyncio.CancelledError:
                handle.cancel()
                try: await _join(task)
                except Exception: pass
                raise
            except RuntimeError as error:
                if str(error).startswith('MCP_OPERATION_UNCERTAIN:'): raise McpOperationUncertain(str(error)) from error
                raise
        finally:
            handle.cancel()
            if watcher is not None:
                watcher.cancel()
                await asyncio.gather(watcher, return_exceptions=True)

    def tools(self, *, prefix: str = 'mcp_', names: list[str] | None = None) -> list[Tool]:
        if not isinstance(prefix, str): raise TypeError('MCP tool prefix must be a string')
        selected = names if names is not None else [tool['name'] for tool in self._catalog]
        if len(set(selected)) != len(selected): raise ValueError('MCP tool names must be unique')
        tools = []
        for name in selected:
            definition = next((tool for tool in self._catalog if tool['name'] == name), None)
            if definition is None: raise ValueError('MCP_ERROR: unknown selected tool')
            async def execute(arguments, context, remote_name=name):
                return await self.call(remote_name, arguments, context=dict(context), cancel_event=context.cancel_event)
            tools.append(Tool(prefix + name, definition['description'], copy.deepcopy(definition['inputSchema']), execute, with_context=True))
        return tools

    async def call(self, name: str, arguments: dict, *, context: dict | None = None, cancel_event: asyncio.Event | None = None):
        if cancel_event is not None and cancel_event.is_set(): raise asyncio.CancelledError
        handle = self._native.start_call(name, json.dumps(arguments, allow_nan=False), json.dumps(context or {}, allow_nan=False))
        watcher = None
        if cancel_event is not None:
            async def watch():
                await cancel_event.wait()
                handle.cancel()
            watcher = asyncio.create_task(watch())
        try:
            try: return json.loads(await asyncio.to_thread(handle.result))
            except RuntimeError as error:
                if str(error).startswith('MCP_OPERATION_UNCERTAIN:'): raise McpOperationUncertain(str(error)) from error
                raise
        finally:
            handle.cancel()
            if watcher is not None:
                watcher.cancel()
                await asyncio.gather(watcher, return_exceptions=True)

    async def close(self):
        await asyncio.to_thread(self._native.close)

    async def __aenter__(self): return self
    async def __aexit__(self, *args): await self.close()
