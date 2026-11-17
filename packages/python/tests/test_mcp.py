import asyncio
import json
from pathlib import Path
import tempfile
import unittest
from hive_sdk import Agent, McpClient, McpOperationUncertain, ToolOperationUncertain, SessionStore, Tool

FIXTURE = Path(__file__).resolve().parents[3] / 'tests/fixtures/mcp_stdio.py'


async def connect(mode, receipt, **options):
    return await McpClient.connect(command='python3', args=[str(FIXTURE), mode, str(receipt)], timeout_ms=5000, **options)


def rows(file):
    return [json.loads(line) for line in Path(file).read_text().splitlines()] if Path(file).exists() else []


def call(args):
    return {'content': None, 'tool_calls': [{'id': 'c', 'name': 'fs_add', 'arguments': args}], 'usage': {'input_tokens': 1, 'output_tokens': 1}}


async def wait_receipt(file):
    async def wait():
        while not rows(file): await asyncio.sleep(.01)
    await asyncio.wait_for(wait(), 3)


class McpTests(unittest.IsolatedAsyncioTestCase):
    async def test_catalog_schema_rejection_and_durable_context(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / 'calls'; client = await connect('pages', receipt)
            store = SessionStore(str(Path(directory) / 'db')); requests = 0
            class Provider:
                async def complete(inner, request):
                    nonlocal requests
                    requests += 1; self.assertEqual(request['messages'][0]['content'], 'System')
                    if requests == 1: return call({'a': '2', 'b': 3})
                    if requests == 2:
                        self.assertEqual(len(rows(receipt)), 0)
                        return call({'a': 2, 'b': 3})
                    self.assertEqual(json.loads(request['messages'][-1]['content'])['structuredContent']['answer'], 5)
                    return {'content': 'done'}
            try:
                self.assertEqual(client.info['protocolVersion'], '2025-11-25'); self.assertEqual(len(client.catalog), 2)
                with self.assertRaises(ValueError): client.tools(names=['missing'])
                with self.assertRaises(ValueError): client.tools(names=['add', 'add'])
                catalog = client.catalog; catalog[0]['name'] = 'changed'; self.assertEqual(client.catalog[0]['name'], 'add')
                agent = Agent(model='test', system='System', store=store, agent_id='a', session_id='s', tools=client.tools(prefix='fs_', names=['add']), provider=Provider())
                result = await agent.run('Add'); self.assertEqual(result['content'], 'done')
                self.assertEqual(result['stats']['tool_rejections'], 1); self.assertEqual(result['stats']['tool_calls'], 1)
                run = store.inspect(agent_id='a', session_id='s')['run']
                self.assertEqual(len(rows(receipt)), 1)
                self.assertEqual(rows(receipt)[0]['meta']['hive/context']['execution_id'], run['tools'][1]['execution_id'])
            finally: store.close(); await client.close()

    async def test_lost_response_requires_resolution_and_resume_never_replays(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / 'calls'; client = await connect('exit', receipt)
            store = SessionStore(str(Path(directory) / 'db')); requests = 0; replacement = None
            class Provider:
                async def complete(inner, request):
                    nonlocal requests
                    requests += 1
                    return call({'a': 2, 'b': 3}) if requests == 1 else {'content': 'resolved', 'usage': {'input_tokens': 1, 'output_tokens': 1}}
            config = dict(model='test', store=store, agent_id='a', session_id='s', policy={'max_total_output_tokens': 5}, provider=Provider())
            try:
                with self.assertRaises(McpOperationUncertain):
                    await Agent(**config, tools=client.tools(prefix='fs_', names=['add'])).run('Add')
                self.assertEqual(requests, 1); self.assertEqual(len(rows(receipt)), 1)
                snapshot = store.inspect(agent_id='a', session_id='s')
                self.assertEqual(snapshot['run']['in_flight']['kind'], 'tool'); self.assertEqual(snapshot['run']['tools'][0]['status'], 'started')
                with self.assertRaisesRegex(RuntimeError, 'SESSION_OPERATION_UNCERTAIN'):
                    Agent(**config, tools=client.tools(prefix='fs_', names=['add']), resume=True)
                store.resolve_tool(agent_id='a', session_id='s', revision=snapshot['revision'], execution_id=snapshot['run']['in_flight']['execution_id'],
                    result={'content': [{'type': 'text', 'text': '5'}], 'structuredContent': {'answer': 5}})
                second_receipt = Path(directory) / 'second'; replacement = await connect('normal', second_receipt)
                result = await Agent(**config, tools=replacement.tools(prefix='fs_', names=['add']), resume=True).resume()
                self.assertEqual(result['content'], 'resolved'); self.assertEqual(requests, 2)
                self.assertEqual(len(rows(second_receipt)), 0); self.assertEqual(len(rows(receipt)), 1)
            finally:
                store.close(); await client.close()
                if replacement is not None: await replacement.close()

    async def test_confirmed_tool_errors_are_feedback_with_the_original_result(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / 'calls'; client = await connect('tool_error', receipt); requests = 0
            class Provider:
                async def complete(inner, request):
                    nonlocal requests
                    requests += 1
                    if requests == 1: return call({'a': 2, 'b': 3})
                    feedback = json.loads(request['messages'][-1]['content'])
                    self.assertEqual(feedback['error']['code'], 'MCP_TOOL_ERROR'); self.assertTrue(feedback['error']['result']['isError'])
                    return {'content': 'handled'}
            try:
                result = await Agent(model='test', tools=client.tools(prefix='fs_', names=['add']), provider=Provider()).run('Add')
                self.assertEqual(result['content'], 'handled'); self.assertEqual(len(rows(receipt)), 1)
            finally: await client.close()

    async def test_agent_cancel_closes_mcp_and_leaves_a_dispatched_tool_uncertain(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / 'calls'; client = await connect('wait', receipt); store = SessionStore(str(Path(directory) / 'db'))
            class Provider:
                async def complete(inner, request): return call({'a': 2, 'b': 3})
            try:
                agent = Agent(model='test', store=store, agent_id='a', session_id='s', tools=client.tools(prefix='fs_', names=['add']), provider=Provider())
                pending = asyncio.create_task(agent.run('Add')); await wait_receipt(receipt); self.assertTrue(agent.cancel())
                with self.assertRaises(asyncio.CancelledError): await pending
                self.assertEqual(store.inspect(agent_id='a', session_id='s')['run']['in_flight']['kind'], 'tool')
                self.assertEqual(len(rows(receipt)), 1)
            finally: store.close(); await client.close()

    async def test_discovery_rejects_invalid_schemas(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, 'INVALID_TOOL_SCHEMA'): await connect('bad_schema', Path(directory) / 'calls')

    async def test_custom_uncertain_tool_error_never_triggers_model_continuation(self):
        requests = 0
        class Provider:
            async def complete(inner, request):
                nonlocal requests
                requests += 1
                return call({'a': 2, 'b': 3})
        async def execute(args): raise ToolOperationUncertain('uncertain external outcome')
        with self.assertRaises(ToolOperationUncertain):
            await Agent(model='test', tools=[Tool('fs_add', 'Add', {'type': 'object'}, execute)], provider=Provider()).run('Add')
        self.assertEqual(requests, 1)

    async def test_a_server_that_changes_its_tools_is_noticed_and_refresh_tools_adopts_the_new_list(self):
        with tempfile.TemporaryDirectory() as directory:
            client = await connect('mutating', Path(directory) / 'calls')
            try:
                self.assertEqual([t['name'] for t in client.catalog], ['add', 'echo'])
                self.assertFalse(client.tools_changed)
                self.assertEqual((await client.call('echo', {'value': 'mutate'}))['structuredContent']['value'], 'mutate')
                self.assertTrue(client.tools_changed)
                self.assertEqual([t['name'] for t in client.catalog], ['add', 'echo'], 'unchanged until refreshed')
                self.assertEqual(await client.refresh_tools(), {'added': ['extra'], 'removed': [], 'changed': ['add']})
                self.assertFalse(client.tools_changed)
                self.assertEqual([t['name'] for t in client.catalog], ['add', 'echo', 'extra'])
                self.assertIn('mcp_extra', [t.name for t in client.tools()])
            finally:
                await client.close()



def _sse_server():
    """A minimal MCP server over the classic HTTP+SSE transport, as n8n's MCP Server Trigger speaks it."""
    import http.server, json as _json, threading
    state = {'stream': None, 'seen': [], 'ready': threading.Event()}

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'
        def log_message(self, *args): pass
        def do_GET(self):
            self.send_response(200); self.send_header('Content-Type', 'text/event-stream'); self.end_headers()
            state['stream'] = self.wfile
            self.wfile.write(b': hi\n\nevent: endpoint\ndata: /messages?sessionId=s1\n\n'); self.wfile.flush()
            state['ready'].wait(30)
        def do_POST(self):
            body = _json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            self.send_response(202); self.send_header('Content-Length', '8'); self.end_headers(); self.wfile.write(b'Accepted')
            state['seen'].append(body.get('method'))
            if 'id' not in body: return
            if body['method'] == 'initialize': result = {'protocolVersion': '2025-03-26', 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'n8n-like', 'version': '1'}}
            elif body['method'] == 'tools/list': result = {'tools': [{'name': 'echo', 'description': 'Echo', 'inputSchema': {'type': 'object', 'properties': {'value': {}}, 'required': ['value']}}]}
            else: result = {'content': [{'type': 'text', 'text': _json.dumps(body['params']['arguments']['value'])}], 'structuredContent': {'value': body['params']['arguments']['value']}}
            frame = f"event: message\ndata: {_json.dumps({'jsonrpc': '2.0', 'id': body['id'], 'result': result})}\n\n".encode()
            state['stream'].write(frame); state['stream'].flush()

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler); server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, state


class SseTests(unittest.IsolatedAsyncioTestCase):
    async def test_the_classic_http_sse_transport_connects_lists_and_calls_from_a_client_and_a_manager(self):
        server, state = _sse_server(); url = f'http://127.0.0.1:{server.server_address[1]}/mcp/x'
        client = None
        try:
            client = await McpClient.connect(transport='sse', url=url)
            self.assertEqual(client.info['protocolVersion'], '2025-03-26')
            self.assertEqual([t['name'] for t in client.catalog], ['echo'])
            self.assertEqual((await client.call('echo', {'value': {'n': 2}}))['structuredContent'], {'value': {'n': 2}})
            self.assertEqual(state['seen'], ['initialize', 'notifications/initialized', 'tools/list', 'tools/call'])
            with self.assertRaisesRegex(ValueError, 'Process options'):
                await McpClient.connect(transport='sse', url=url, command='x')
            from hive_sdk import McpManager
            async with McpManager() as manager:
                report = await manager.apply({'calendar': {'transport': 'sse', 'url': url}})
                self.assertEqual(report['added'], ['calendar']); self.assertEqual(manager.status[0]['state'], 'connected')
                self.assertEqual([t['name'] for t in manager.catalog], ['calendar__echo'])
        finally:
            if client: await client.close()
            state['ready'].set(); server.shutdown()


if __name__ == '__main__':
    unittest.main()
