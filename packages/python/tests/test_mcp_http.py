import asyncio
from contextlib import asynccontextmanager
import json
from pathlib import Path
import tempfile
import unittest
from hive_sdk import Agent, McpClient, McpOperationUncertain, SessionStore
from test_streaming import data, headers

DECLARATION = {'name': 'add', 'description': 'Add', 'inputSchema': {'type': 'object', 'properties': {'a': {'type': 'integer'}, 'b': {'type': 'integer'}}, 'required': ['a', 'b']}}


def call(): return {'content': None, 'tool_calls': [{'id': 'c', 'name': 'remote_add', 'arguments': {'a': 2, 'b': 3}}]}


async def send(writer, value=None, status=200, extra=''):
    payload = b'' if value is None else json.dumps(value).encode()
    writer.write(f'HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {len(payload)}\r\nConnection: close\r\n{extra}\r\n'.encode() + payload)
    await writer.drain()


@asynccontextmanager
async def serve(handler):
    tasks, writers, errors = set(), set(), []
    async def connected(reader, writer):
        task = asyncio.current_task(); tasks.add(task); writers.add(writer)
        try:
            header = await reader.readuntil(b'\r\n\r\n'); lower = header.lower()
            assert b'authorization: bearer test-only' in lower
            if lower.startswith(b'delete '): await send(writer, status=204); return
            length = next(int(line.split(b':', 1)[1]) for line in lower.split(b'\r\n') if line.startswith(b'content-length:'))
            message = json.loads(await reader.readexactly(length))
            if message['method'] == 'initialize':
                assert message['params']['capabilities'] == {}
                await send(writer, {'jsonrpc': '2.0', 'id': message['id'], 'result': {'protocolVersion': '2025-11-25', 'serverInfo': {'name': 'local-http', 'version': '1'}, 'capabilities': {'tools': {}}}}, extra='Mcp-Session-Id: session-1\r\n')
                return
            assert b'mcp-session-id: session-1' in lower and b'mcp-protocol-version: 2025-11-25' in lower
            if message['method'] == 'notifications/initialized': await send(writer, status=202); return
            if message['method'] == 'tools/list': await send(writer, {'jsonrpc': '2.0', 'id': message['id'], 'result': {'tools': [DECLARATION]}}); return
            assert message['method'] == 'tools/call'
            await handler(message, reader, writer)
        except Exception as error: errors.append(error)
        finally:
            writer.close()
            try: await writer.wait_closed()
            except ConnectionError: pass
            writers.discard(writer); tasks.discard(task)
    server = await asyncio.start_server(connected, '127.0.0.1', 0)
    port = server.sockets[0].getsockname()[1]
    async def connect(): return await McpClient.connect(transport='http', url=f'http://127.0.0.1:{port}/mcp', headers={'Authorization': 'Bearer test-only'}, timeout_ms=5000)
    try: yield connect
    finally:
        server.close(); await server.wait_closed()
        for writer in list(writers): writer.close()
        pending = list(tasks)
        for task in pending: task.cancel()
        if pending: await asyncio.gather(*pending, return_exceptions=True)
        if errors: raise errors[0]


class McpHttpTests(unittest.IsolatedAsyncioTestCase):
    async def test_json_and_sse_use_the_same_agent_tools_and_result_accounting(self):
        for streaming in (False, True):
            calls, models = 0, 0
            async def handler(message, reader, writer):
                nonlocal calls
                calls += 1
                value = {'jsonrpc': '2.0', 'id': message['id'], 'result': {'content': [{'type': 'text', 'text': '5'}], 'structuredContent': {'answer': 5}}}
                if streaming: await headers(writer); await data(writer, value)
                else: await send(writer, value)
            class Provider:
                async def complete(inner, request):
                    nonlocal models
                    models += 1
                    if models == 1: return call()
                    self.assertEqual(json.loads(request['messages'][-1]['content'])['structuredContent']['answer'], 5)
                    return {'content': 'done'}
            async with serve(handler) as connect:
                client = await connect()
                try:
                    result = await Agent(model='test', tools=client.tools(prefix='remote_'), provider=Provider()).run('Add')
                    self.assertEqual(result['content'], 'done'); self.assertEqual(result['stats']['tool_calls'], 1); self.assertEqual(calls, 1)
                finally: await client.close()

    async def test_response_loss_preserves_an_uncertain_claim_without_repeating_post(self):
        calls, models = 0, 0
        async def handler(message, reader, writer):
            nonlocal calls
            calls += 1
        class Provider:
            async def complete(inner, request):
                nonlocal models
                models += 1
                return call()
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(str(Path(directory) / 'db'))
            async with serve(handler) as connect:
                client = await connect()
                config = dict(model='test', store=store, agent_id='a', session_id='s', tools=client.tools(prefix='remote_'), provider=Provider())
                try:
                    with self.assertRaises(McpOperationUncertain): await Agent(**config).run('Add')
                    self.assertEqual(calls, 1); self.assertEqual(models, 1)
                    self.assertEqual(store.inspect(agent_id='a', session_id='s')['run']['in_flight']['kind'], 'tool')
                    with self.assertRaisesRegex(RuntimeError, 'SESSION_OPERATION_UNCERTAIN'): Agent(**config, resume=True)
                    with self.assertRaisesRegex(RuntimeError, 'closed'): await client.call('add', {'a': 2, 'b': 3})
                    self.assertEqual(calls, 1)
                finally: store.close(); await client.close()

    async def test_http_cancellation_preserves_the_tool_claim(self):
        ready, closed = asyncio.Event(), asyncio.Event()
        calls = 0
        async def handler(message, reader, writer):
            nonlocal calls
            calls += 1; ready.set(); await reader.read(); closed.set()
        class Provider:
            async def complete(inner, request): return call()
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(str(Path(directory) / 'db'))
            async with serve(handler) as connect:
                client = await connect()
                try:
                    agent = Agent(model='test', store=store, agent_id='a', session_id='s', tools=client.tools(prefix='remote_'), provider=Provider())
                    task = asyncio.create_task(agent.run('Add')); await asyncio.wait_for(ready.wait(), 3); agent.cancel()
                    with self.assertRaises(asyncio.CancelledError): await task
                    await asyncio.wait_for(closed.wait(), 3)
                    self.assertEqual(store.inspect(agent_id='a', session_id='s')['run']['in_flight']['kind'], 'tool'); self.assertEqual(calls, 1)
                finally: store.close(); await client.close()
