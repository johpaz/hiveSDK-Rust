"""Native CDP connection. The host owns the browser process and profile."""
import asyncio
import json
from . import _native
from ._storage import storage_call, _join


class Browser:
    def __init__(self, handle):
        self._native = handle

    @classmethod
    async def connect(cls, page_websocket_url: str, *, timeout_ms: int = 10000):
        handle = await storage_call(_native.connect_browser, page_websocket_url, timeout_ms,
                                    discard=lambda handle: handle.close())
        return cls(handle)

    async def _call(self, op, **fields):
        # Cancellation drops the native socket before joining the worker. A page
        # action may already have happened and must not be retried implicitly.
        task = asyncio.create_task(storage_call(self._native.dispatch, json.dumps({**fields, 'op': op}, allow_nan=False)))
        try:
            return json.loads(await asyncio.shield(task))
        except asyncio.CancelledError:
            self._native.cancel()
            try: await _join(task)
            except Exception: pass
            raise

    async def navigate(self, url: str): return await self._call('navigate', url=url)
    async def evaluate(self, expression: str): return await self._call('evaluate', expression=expression)
    async def click(self, selector: str): return await self._call('click', selector=selector)
    async def type_text(self, selector: str, text: str): return await self._call('type_text', selector=selector, text=text)
    async def extract(self, selector: str = 'body'): return await self._call('extract', selector=selector)
    async def screenshot(self): return await self._call('screenshot')
    def cancel(self): self._native.cancel()
    async def close(self): await storage_call(self._native.close)
    async def __aenter__(self): return self
    async def __aexit__(self, *_): await self.close()
