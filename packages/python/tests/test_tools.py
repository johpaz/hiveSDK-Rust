import asyncio
import http.server
import tempfile
import threading
import unittest
from pathlib import Path
from hive_sdk import NativeTools


class _Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = (b'{"web":{"results":[{"title":"T","url":"https://x","description":"D"}]}}'
                if self.path.startswith('/search') else b'{"hello":"world"}')
        self.send_response(200)
        self.send_header('content-type', 'application/json')
        self.send_header('content-length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


class NativeToolsTests(unittest.IsolatedAsyncioTestCase):
    async def test_filesystem_tools_stay_inside_the_root(self):
        with tempfile.TemporaryDirectory() as outer:
            root = Path(outer) / 'root'
            root.mkdir()
            (Path(outer) / 'secret.txt').write_text('secret')
            tools = NativeTools({'root': str(root)})
            names = [tool['name'] for tool in await tools.definitions()]
            self.assertEqual(names, ['fs_read', 'fs_write', 'fs_edit', 'fs_list', 'fs_glob', 'fs_grep'])

            await tools.execute('fs_write', {'path': 'notes/a.txt', 'content': 'uno\ndos\n', 'create_dirs': True})
            self.assertEqual((root / 'notes/a.txt').read_text(), 'uno\ndos\n')
            self.assertEqual((await tools.execute('fs_read', {'path': 'notes/a.txt', 'offset': 1}))['content'], 'dos\n')
            await tools.execute('fs_edit', {'path': 'notes/a.txt', 'old': 'dos', 'new': '2'})
            hits = await tools.execute('fs_grep', {'pattern': '2'})
            self.assertEqual([hit['path'] for hit in hits['matches']], ['notes/a.txt'])
            with self.assertRaisesRegex(RuntimeError, 'relative|escape'):
                await tools.execute('fs_read', {'path': '../secret.txt'})
            with self.assertRaisesRegex(RuntimeError, 'unknown or disabled'):
                await tools.execute('command_run', {'program': 'echo'})
            with self.assertRaisesRegex(RuntimeError, 'unknown field'):
                NativeTools({'root': str(root), 'nonsense': True})

    async def test_commands_honour_allowlist_timeout_and_cancellation(self):
        with tempfile.TemporaryDirectory() as root:
            tools = NativeTools({'root': root, 'command': {'allowed_programs': ['echo', 'sleep'], 'max_timeout_ms': 5000}})
            self.assertEqual((await tools.execute('command_run', {'program': 'echo', 'args': ['a;b']}))['stdout'], 'a;b\n')
            with self.assertRaisesRegex(RuntimeError, 'allowlist'):
                await tools.execute('command_run', {'program': 'rm'})
            slow = await tools.execute('command_run', {'program': 'sleep', 'args': ['30'], 'timeout_ms': 200})
            self.assertTrue(slow['timed_out'])

            task = asyncio.create_task(tools.execute('command_run', {'program': 'sleep', 'args': ['30']}))
            await asyncio.sleep(0.2)
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
            self.assertEqual((await tools.execute('command_run', {'program': 'echo', 'args': ['again']}))['stdout'], 'again\n')

            # Cancellation captured before a worker starts must remain sticky.
            request = tools._native.fork()
            request.cancel()
            with self.assertRaisesRegex(RuntimeError, 'CANCELLED'):
                await asyncio.to_thread(request.dispatch, '{"op":"execute_tool","name":"command_run","arguments":{"program":"echo"}}')
            self.assertEqual((await tools.execute('command_run', {'program': 'echo', 'args': ['independent']}))['stdout'], 'independent\n')

    async def test_web_tools_fetch_search_and_bind_to_agent_tools(self):
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), _Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        base = f'http://127.0.0.1:{server.server_port}'
        try:
            with tempfile.TemporaryDirectory() as root:
                with self.assertRaisesRegex(RuntimeError, 'non-public'):
                    await NativeTools({'root': root, 'web': {}}).execute('web_fetch', {'url': f'{base}/doc'})
                tools = NativeTools({'root': root, 'web': {'allow_private_hosts': True,
                                     'search': {'kind': 'brave', 'api_key': 'k', 'endpoint': f'{base}/search'}}})
                fetched = await tools.execute('web_fetch', {'url': f'{base}/doc'})
                self.assertEqual((fetched['status'], fetched['body']), (200, '{"hello":"world"}'))
                found = await tools.execute('web_search', {'query': 'hive', 'limit': 1})
                self.assertEqual(found['results'], [{'title': 'T', 'url': 'https://x', 'snippet': 'D'}])
                agent_tools = await tools.create_tools()
                search = next(tool for tool in agent_tools if tool.name == 'web_search')
                self.assertEqual(len((await search.execute({'query': 'hive'}))['results']), 1)
        finally:
            server.shutdown()
            server.server_close()


class ComputerToolsTests(unittest.IsolatedAsyncioTestCase):
    async def test_computer_tools_are_off_unless_configured_and_the_host_controls_reach_the_engine(self):
        with tempfile.TemporaryDirectory() as root:
            def names(definitions): return [d['name'] for d in definitions if d['name'].startswith('computer_')]
            async with NativeTools({'root': root}) as none:
                self.assertEqual(names(await none.definitions()), [])
            async with NativeTools({'root': root, 'computer': {'backend': 'x11', 'display': ':99'}}) as screenshots:
                self.assertEqual(names(await screenshots.definitions()), ['computer_screenshot', 'computer_wait'])
            async with NativeTools({'root': root, 'computer': {'backend': 'x11', 'display': ':99', 'allow_input': True}}) as full:
                self.assertEqual(len(names(await full.definitions())), 8)
                self.assertEqual(await full.computer_status(), {'stopped': False, 'actions': 0})
                self.assertEqual((await full.computer_stop())['stopped'], True)
                with self.assertRaises(Exception): await full.execute('computer_screenshot', {})
                self.assertEqual((await full.computer_resume())['stopped'], False)
                self.assertEqual(await full.computer_reset(), {'stopped': False, 'actions': 0})
            with self.assertRaisesRegex(Exception, 'allow_screenshot or allow_input'):
                NativeTools({'root': root, 'computer': {'allow_screenshot': False}})


if __name__ == '__main__':
    unittest.main()
