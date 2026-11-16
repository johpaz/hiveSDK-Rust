import asyncio
import json
import tempfile
import threading
import unittest
from hive_sdk import SecretVault, SessionStore


class SecretTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='hive-secrets-')
        self.store = await SessionStore.open(self.directory.name, tenant_id='one')
    async def asyncTearDown(self):
        await self.store.close_async(); self.directory.cleanup()

    async def test_metadata_cas_tombstones_and_store_lifetime(self):
        vault = await SecretVault.open(self.store, key=bytes([7])*32)
        try:
            value = 'private fixture token 🦀'
            meta = await vault.put('api', value)
            self.assertEqual(await vault.reveal('api'), value)
            self.assertNotIn(value, repr(vault)); self.assertNotIn(value, json.dumps(await vault.list()))
            with self.assertRaisesRegex(RuntimeError, 'SECRET_CONFLICT'): await vault.put('api', 'stale')
            with self.assertRaisesRegex(RuntimeError, 'BUSY'): await self.store.close_async()
            deleted = await vault.delete('api', meta['revision'])
            self.assertIsNone(await vault.reveal('api')); self.assertTrue(deleted['deleted'])
            self.assertEqual((await vault.inspect('api'))['revision'], 2)
            await vault.put('api', 'new', revision=2)
            self.assertEqual(await vault.reveal('api'), 'new')
        finally: await vault.close()
        await vault.close()
        with self.assertRaisesRegex(RuntimeError, 'CLOSED'): await vault.info()

    async def test_rekey_and_tenant_isolation(self):
        first = await self.store.open_secrets(key=bytes([7])*32)
        stale = await self.store.open_secrets(key=bytes([7])*32)
        other_store = await self.store.for_tenant_async('two')
        other = await other_store.open_secrets(key=bytes([7])*32)
        try:
            await first.put('api', 'one'); await other.put('api', 'two')
            with self.assertRaisesRegex(RuntimeError, 'AUTHENTICATION'): await self.store.open_secrets(key=bytes([8])*32)
            receipt = await first.rekey(key_id='second', key=bytes([8])*32)
            self.assertEqual(receipt['encryptions'], 2)
            with self.assertRaisesRegex(RuntimeError, 'AUTHENTICATION'): await stale.reveal('api')
            self.assertEqual(await first.reveal('api'), 'one'); self.assertEqual(await other.reveal('api'), 'two')
        finally:
            await first.close(); await stale.close(); await other.close(); await other_store.close_async()

    async def test_close_cancellation_joins_the_native_cleanup_and_releases_store(self):
        vault = await self.store.open_secrets(key=bytes([7])*32)
        original = vault._native
        entered = asyncio.Event(); release = threading.Event(); loop = asyncio.get_running_loop()
        class Gate:
            def close(self):
                loop.call_soon_threadsafe(entered.set); release.wait(2); original.close()
        vault._native = Gate()
        closing = asyncio.create_task(vault.close())
        try:
            await entered.wait(); closing.cancel(); release.set()
            with self.assertRaises(asyncio.CancelledError): await closing
            await vault.close()
            with self.assertRaisesRegex(RuntimeError, 'CLOSED'): await vault.info()
        finally:
            release.set()
            if not closing.done(): await asyncio.gather(closing, return_exceptions=True)
            await vault.close()

    async def test_catalog_references_are_metadata_and_keys_are_explicit_bytes(self):
        with self.assertRaises(TypeError): await self.store.open_secrets(key='not bytes')
        vault = await self.store.open_secrets(key=bytes([7])*32)
        try:
            await vault.put('provider-key', 'private fixture')
            record = await self.store.services.collection('providers').create('fixture', {
                'name':'Fixture', 'protocol':'openai_compatible', 'base_url':'http://127.0.0.1:9',
                'credential_ref':{'vault_id':'default','name':'provider-key'}})
            self.assertNotIn('private fixture', json.dumps(record))
            with self.assertRaisesRegex(RuntimeError, 'inline credentials'):
                await self.store.services.collection('providers').update('fixture', record['revision'], {'api_key':'private fixture'})
        finally: await vault.close()
