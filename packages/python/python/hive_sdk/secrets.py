"""Explicit, tenant-scoped native secret vault; only reveal returns plaintext."""
import json
import threading
import weakref
from typing import TypedDict
from ._storage import storage_call


class SecretReference(TypedDict):
    vault_id: str
    name: str


class SecretMetadata(TypedDict):
    name: str
    revision: int
    key_id: str
    deleted: bool
    created_at: int
    updated_at: int


def _key(value):
    if not isinstance(value, bytes) or len(value) != 32:
        raise TypeError('Secret master key must be exactly 32 bytes')
    return value


def _cleanup(native, store):
    try: native.close()
    except Exception: pass
    finally: store._release()


class SecretVault:
    def __init__(self, store, native):
        self._store, self._native = store, native
        self._active, self._closed, self._closing = 0, False, False
        self._lock = threading.RLock()
        self._finalizer = weakref.finalize(self, _cleanup, native, store)

    def __repr__(self): return '<SecretVault [REDACTED]>'

    @classmethod
    async def open(cls, store, *, key: bytes, id='default', key_id='primary'):
        material = _key(key)
        store._retain()
        try:
            native = await storage_call(store._native.open_secret_vault, id, key_id, material, discard=lambda handle: handle.close())
            return cls(store, native)
        except BaseException:
            store._release()
            raise

    def _enter(self):
        with self._lock:
            if self._closed: raise RuntimeError('CLOSED: secret vault is closed')
            if self._closing: raise RuntimeError('BUSY: secret vault close is pending')
            self._store._retain(); self._active += 1

    def _leave(self):
        with self._lock: self._active -= 1
        self._store._release()

    async def _call(self, op, **fields):
        encoded = json.dumps({**fields, 'op': op}, allow_nan=False)
        self._enter()
        try: return json.loads(await storage_call(self._native.dispatch, encoded))
        finally: self._leave()

    async def info(self): return await self._call('info')
    async def inspect(self, name: str) -> SecretMetadata | None: return await self._call('inspect', name=name)
    async def reveal(self, name: str) -> str | None: return await self._call('reveal', name=name)
    async def put(self, name: str, value: str, *, revision=0) -> SecretMetadata: return await self._call('put', name=name, value=value, revision=revision)
    async def delete(self, name: str, revision: int) -> SecretMetadata: return await self._call('delete', name=name, revision=revision)
    async def list(self, *, limit=50, cursor=None, include_deleted=False):
        return await self._call('list', limit=limit, cursor=cursor, include_deleted=include_deleted)
    async def rekey(self, *, key_id: str, key: bytes):
        material = _key(key)
        self._enter()
        try: return json.loads(await storage_call(self._native.rekey, key_id, material))
        finally: self._leave()

    def _mark_closed(self, _result):
        with self._lock:
            if self._closed: return
            self._closed = True
            self._finalizer.detach()
        self._store._release()

    async def close(self):
        with self._lock:
            if self._closed: return
            if self._closing or self._active: raise RuntimeError('BUSY: wait for secret operations before closing')
            self._closing = True
        try: await storage_call(self._native.close, on_result=self._mark_closed)
        finally:
            with self._lock: self._closing = False
