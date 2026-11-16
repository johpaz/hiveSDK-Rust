import tempfile
import unittest
from pathlib import Path
from hive_sdk import Agent, SessionStore


class CatalogTests(unittest.IsolatedAsyncioTestCase):
    async def test_tenant_revisions_reopen_and_import(self):
        with tempfile.TemporaryDirectory() as directory:
            store = await SessionStore.open(str(Path(directory) / 'db'), tenant_id='first')
            other = await store.for_tenant_async('second')
            try:
                memory = store.services.collection('memory')
                row = await memory.create('same', {'title': 'Preferencias', 'content': 'Español'})
                self.assertEqual(row['revision'], 1)
                self.assertIsNone(await other.services.collection('memory').get('same'))
                await memory.update('same', 1, {'content': 'Hola'})
                with self.assertRaisesRegex(RuntimeError, 'CATALOG_CONFLICT'):
                    await memory.delete('same', 1)
                with self.assertRaisesRegex(RuntimeError, 'schema'):
                    await memory.update('same', 2, {'user_id': 'second'})
                skill = Path(directory) / 'SKILL.md'
                skill.write_text('---\nname: correo\ndescription: Redactar correo\n---\nEscribe claramente.\n')
                imported = await store.services.import_skill(str(skill))
                self.assertEqual(imported['id'], 'correo')
                self.assertEqual((await store.services.collection('skills').search('CORREO'))[0]['record']['id'], 'correo')
            finally:
                await other.close_async()
                await store.close_async()
            store = await SessionStore.open(str(Path(directory) / 'db'), tenant_id='first')
            try:
                self.assertEqual((await store.services.collection('memory').get('same'))['revision'], 2)
            finally:
                await store.close_async()

    async def test_native_tools_run_in_agent(self):
        with tempfile.TemporaryDirectory() as directory:
            store = await SessionStore.open(directory)
            agent = None
            calls = []
            class Provider:
                async def complete(self, request):
                    calls.append(request)
                    if len(calls) == 1:
                        return {'content': None, 'tool_calls': [{'id': 'remember', 'name': 'memory_write', 'arguments': {'title': 'Entrega', 'content': 'Viernes'}}]}
                    return {'content': 'Guardado'}
            try:
                tools = await store.services.create_tools()
                self.assertEqual(len(tools), 8)
                agent = await Agent.open(store=store, model='fixture', agent_id='a', session_id='s', tools=tools, provider=Provider())
                self.assertEqual((await agent.run('Recuerda la entrega'))['content'], 'Guardado')
                self.assertEqual((await store.services.collection('memory').get('Entrega'))['data']['content'], 'Viernes')
                self.assertEqual(len(calls), 2)
            finally:
                if agent: agent.close()
                await store.close_async()


class BundledSkillTests(unittest.IsolatedAsyncioTestCase):
    async def test_bundled_skills_list_and_import_once_their_sdk_tools_are_in_the_catalog(self):
        from hive_sdk import NativeTools
        with tempfile.TemporaryDirectory() as directory:
            store = await SessionStore.open(str(Path(directory) / 'db'), tenant_id='first')
            try:
                listed = await store.services.bundled_skills()
                self.assertEqual(sorted(s['name'] for s in listed), ['obscura_browser', 'office_document_manager', 'pdf_report_writer'])
                self.assertEqual((await store.services.collection('skills').list())['items'], [])
                with self.assertRaisesRegex(Exception, 'unknown bundled skill'):
                    await store.services.import_bundled_skill('nope')
                with self.assertRaisesRegex(Exception, 'reference not found'):
                    await store.services.import_bundled_skill('pdf_report_writer')
                tools = NativeTools({'root': directory, 'office': {}, 'obscura': {'allow_evaluate': True, 'allow_credentials': True}})
                for d in await tools.definitions():
                    await store.services.collection('tools').create(d['name'], {'name': d['name'], 'description': d['description'], 'parameters': d['parameters']})
                for skill in listed:
                    self.assertEqual((await store.services.import_bundled_skill(skill['name']))['id'], skill['name'])
                browser = (await store.services.collection('skills').get('obscura_browser'))['data']
                self.assertTrue(any('Chromium' in rule for rule in browser['rules']))
            finally:
                await store.close_async()
