import asyncio
import tempfile
import unittest
from pathlib import Path
from hive_sdk import McpManager, McpOperationUncertain

FIXTURE = str(Path(__file__).resolve().parents[3] / 'tests/fixtures/mcp_stdio.py')


def server(mode, receipt, **extra):
    return {'command': 'python3', 'args': [FIXTURE, mode, str(receipt)], 'timeout_ms': 3000, **extra}


class McpManagerTests(unittest.IsolatedAsyncioTestCase):
    async def test_set_is_reconciled_and_failures_are_per_server(self):
        with tempfile.TemporaryDirectory() as directory:
            d = Path(directory)
            async with McpManager() as manager:
                first = await manager.apply({'math': server('normal', d / 'a'), 'pages': server('pages', d / 'b'),
                                             'missing': {'command': '/nonexistent/mcp-server'}, 'off': server('normal', d / 'o', enabled=False)})
                self.assertEqual((sorted(first['added']), first['disabled'], list(first['failed'])), (['math', 'pages'], ['off'], ['missing']))
                self.assertEqual({r['name']: r['state'] for r in manager.status}, {'math': 'connected', 'pages': 'connected', 'missing': 'failed', 'off': 'disabled'})
                tools = manager.tools()
                self.assertEqual(sorted(t.name for t in tools), ['math__add', 'math__echo', 'pages__add', 'pages__echo'])
                add = next(t for t in tools if t.name == 'math__add')
                class Ctx(dict):
                    cancel_event = None
                self.assertEqual((await add.execute({'a': 2, 'b': 5}, Ctx()))['structuredContent']['answer'], 7)
                with self.assertRaisesRegex(Exception, 'MCP_SERVER_UNAVAILABLE'):
                    await manager.call('missing', 'add', {'a': 1, 'b': 1})
                second = await manager.apply({'math': server('normal', d / 'a'), 'pages': server('normal', d / 'b'), 'fresh': server('normal', d / 'f')})
                self.assertEqual((second['unchanged'], second['restarted'], second['added'], sorted(second['removed'])), (['math'], ['pages'], ['fresh'], ['missing', 'off']))
                before = manager.status
                with self.assertRaisesRegex(Exception, 'server name'):
                    await manager.apply({'bad name': server('normal', d / 'x')})
                with self.assertRaises(TypeError):
                    await manager.apply([])
                self.assertEqual(manager.status, before)

    async def test_uncertain_call_closes_the_server_and_reconnect_recovers(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / 'calls'
            async with McpManager() as manager:
                await manager.apply({'flaky': server('exit', receipt)})
                with self.assertRaises(McpOperationUncertain):
                    await manager.call('flaky', 'add', {'a': 1, 'b': 1})
                self.assertEqual(manager.status[0]['state'], 'closed')
                with self.assertRaisesRegex(Exception, 'is closed; reconnect it'):
                    await manager.call('flaky', 'add', {'a': 1, 'b': 1})
                await asyncio.sleep(0.2)
                self.assertEqual(len(receipt.read_text().strip().splitlines()), 1)
                self.assertTrue((await manager.reconnect('flaky'))['connected'])
                self.assertEqual(manager.status[0]['state'], 'connected')


    async def test_a_manager_reports_a_changed_tool_list_and_refresh_tools_updates_its_catalog(self):
        with tempfile.TemporaryDirectory() as directory:
            async with McpManager() as manager:
                await manager.apply({'live': server('mutating', Path(directory) / 'a')})
                self.assertEqual(sorted(t['name'] for t in manager.catalog), ['live__add', 'live__echo'])
                self.assertFalse(manager.status[0]['tools_changed'])
                await manager.call('live', 'echo', {'value': 'mutate'})
                self.assertTrue(manager.status[0]['tools_changed'])
                self.assertEqual(await manager.refresh_tools('live'), {'server': 'live', 'added': ['extra'], 'removed': [], 'changed': ['add']})
                self.assertEqual((manager.status[0]['tools'], manager.status[0]['tools_changed']), (3, False))
                self.assertEqual(sorted(t['name'] for t in manager.catalog), ['live__add', 'live__echo', 'live__extra'])
                with self.assertRaisesRegex(Exception, 'unknown MCP server'):
                    await manager.refresh_tools('nobody')


if __name__ == '__main__':
    unittest.main()
