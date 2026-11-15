from pathlib import Path
import unittest
from hive_sdk import Browser, McpClient, McpOperationUncertain

FIXTURE = str(Path(__file__).resolve().parents[3] / 'tests/fixtures/mcp_stdio.py')


class McpCatalogTests(unittest.IsolatedAsyncioTestCase):
    async def test_resources_prompts_and_pagination_without_tools(self):
        client = await McpClient.connect(command='python3', args=[FIXTURE, 'catalog'], timeout_ms=2000)
        try:
            self.assertEqual(client.catalog, [])
            page = await client.list_resources()
            self.assertEqual(page['resources'][0]['uri'], 'test://guide')
            self.assertEqual((await client.list_resources(cursor=page['nextCursor']))['resources'], [])
            self.assertEqual((await client.read_resource('test://guide'))['contents'][0]['text'], 'Use the SDK')
            self.assertEqual((await client.list_prompts())['prompts'][0]['name'], 'review')
            self.assertEqual((await client.get_prompt('review', {'topic': 'Rust'}))['messages'][0]['content']['text'], 'Review Rust')
        finally: await client.close()

    async def test_malformed_resource_closes_session_without_retry(self):
        client = await McpClient.connect(command='python3', args=[FIXTURE, 'bad_resource'], timeout_ms=2000)
        try:
            with self.assertRaises(McpOperationUncertain): await client.read_resource('test://guide')
            with self.assertRaisesRegex(RuntimeError, 'closed'): await client.read_resource('test://guide')
        finally: await client.close()

    async def test_browser_rejects_incompatible_endpoint(self):
        with self.assertRaisesRegex(RuntimeError, 'ws/wss'):
            await Browser.connect('http://127.0.0.1:1/page')
