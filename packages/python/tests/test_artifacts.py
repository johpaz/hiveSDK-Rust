import base64
import struct
import tempfile
import unittest
from pathlib import Path
from hive_sdk import SessionStore

# A real, decodable 4x2 PNG: red on the left half, blue on the right.
REAL_PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAQAAAACCAIAAADwyuo0AAAAFUlEQVR4nGP4z8Dwn4EBTPxnQOYAAGeyB/nadYY6AAAAAElFTkSuQmCC'
PNG = (bytes.fromhex('89504e470d0a1a0a0000000d49484452') + struct.pack('>II', 640, 480) + bytes(9))


class ArtifactTests(unittest.IsolatedAsyncioTestCase):
    async def test_save_read_inspect_and_expire_through_agent_ready_tools(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(str(Path(directory) / 'db'), tenant_id='t')
            try:
                root = Path(directory) / 'files'
                artifacts = store.artifacts(str(root))
                tools = {tool.name: tool for tool in await artifacts.create_tools()}
                self.assertEqual(list(tools), ['artifact_save', 'artifact_inspect', 'artifact_read', 'artifact_list', 'artifact_delete', 'image_info', 'artifact_transform_image'])

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

    async def test_transform_image_makes_a_new_artifact_and_keeps_the_source(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(str(Path(directory) / 'db'), tenant_id='t')
            try:
                tools = {tool.name: tool for tool in await store.artifacts(str(Path(directory) / 'files')).create_tools()}
                source = await tools['artifact_save'].execute({'kind': 'photo', 'mime_type': 'image/png', 'data': REAL_PNG})
                self.assertEqual((source['width'], source['height']), (4, 2))
                turned = await tools['artifact_transform_image'].execute({'id': source['id'], 'rotate': 90, 'format': 'webp'})
                self.assertEqual((turned['width'], turned['height'], turned['artifact']['mime_type'], turned['source']), (2, 4, 'image/webp', source['id']))
                data = base64.b64decode((await tools['artifact_read'].execute({'id': turned['artifact']['id'], 'encoding': 'base64'}))['data'])
                self.assertEqual(data[8:12], b'WEBP')
                self.assertEqual(await tools['image_info'].execute({'id': turned['artifact']['id']}), {'mime_type': 'image/webp', 'width': 2, 'height': 4})
                self.assertEqual((await tools['artifact_inspect'].execute({'id': source['id']}))['status'], 'active')
                with self.assertRaisesRegex(Exception, 'no transformation'):
                    await tools['artifact_transform_image'].execute({'id': source['id']})
                with self.assertRaisesRegex(Exception, 'do not match schema'):
                    await tools['artifact_transform_image'].execute({'id': source['id'], 'rotate': 45})
                with self.assertRaisesRegex(Exception, 'outside the image'):
                    await tools['artifact_transform_image'].execute({'id': source['id'], 'crop': {'x': 3, 'y': 0, 'width': 2, 'height': 2}})
            finally:
                store.close()


if __name__ == '__main__':
    unittest.main()
