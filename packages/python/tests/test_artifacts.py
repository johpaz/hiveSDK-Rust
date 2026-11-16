import base64
import struct
import tempfile
import unittest
from pathlib import Path
from hive_sdk import SessionStore

PNG = (bytes.fromhex('89504e470d0a1a0a0000000d49484452') + struct.pack('>II', 640, 480) + bytes(9))


class ArtifactTests(unittest.IsolatedAsyncioTestCase):
    async def test_save_read_inspect_and_expire_through_agent_ready_tools(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(str(Path(directory) / 'db'), tenant_id='t')
            try:
                root = Path(directory) / 'files'
                artifacts = store.artifacts(str(root))
                tools = {tool.name: tool for tool in await artifacts.create_tools()}
                self.assertEqual(list(tools), ['artifact_save', 'artifact_inspect', 'artifact_read', 'artifact_list', 'artifact_delete', 'image_info'])

                note = await tools['artifact_save'].execute({'kind': 'note', 'mime_type': 'text/plain', 'text': 'hola 🦀', 'expires_in_days': 1})
                self.assertEqual((await tools['artifact_read'].execute({'id': note['id']}))['text'], 'hola 🦀')
                image = await tools['artifact_save'].execute({'kind': 'shot', 'mime_type': 'application/octet-stream',
                                                              'data': base64.b64encode(PNG).decode()})
                self.assertEqual((image['mime_type'], image['width'], image['height']), ('image/png', 640, 480))
                self.assertEqual(await tools['image_info'].execute({'id': image['id']}), {'mime_type': 'image/png', 'width': 640, 'height': 480})
                data = (await tools['artifact_read'].execute({'id': image['id'], 'encoding': 'base64'}))['data']
                self.assertEqual(base64.b64decode(data), PNG)
                self.assertEqual(len((await tools['artifact_list'].execute({}))['items']), 2)
                self.assertEqual(len(list((root / 'artifacts').iterdir())), 1)

                self.assertEqual(await artifacts.expire(now=note['expires_at'] - 1), [])
                self.assertEqual(await artifacts.expire(now=note['expires_at']), [note['id']])
                with self.assertRaisesRegex(Exception, 'ARTIFACT_EXPIRED'):
                    await tools['artifact_read'].execute({'id': note['id']})
                self.assertEqual((await tools['artifact_inspect'].execute({'id': note['id']}))['status'], 'expired')
                with self.assertRaises(TypeError):
                    store.artifacts('')
            finally:
                store.close()


if __name__ == '__main__':
    unittest.main()
