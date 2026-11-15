import asyncio
from contextlib import asynccontextmanager
import json
import tempfile
import unittest
from hive_sdk import Agent, Anthropic, SessionStore, Tool
from test_streaming import data, headers

POLICY = {'max_output_tokens': 4, 'max_total_output_tokens': 5, 'max_context_bytes': 10000}
USE = {'type': 'tool_use', 'id': 'c', 'name': 'add', 'input': {'a': 2, 'b': 3}}


def tool(execute):
    return Tool('add', 'Add', {'type': 'object', 'properties': {'a': {'type': 'number'}, 'b': {'type': 'number'}}, 'required': ['a', 'b']}, execute)


def usage(first):
    return {'input_tokens': 10 if first else 5, 'cache_creation_input_tokens': 4 if first else 0,
            'cache_read_input_tokens': 6 if first else 0, 'output_tokens': 2 if first else 1}


def reply(first, reason=None):
    return {'type': 'message', 'role': 'assistant', 'content': [USE] if first else [{'type': 'text', 'text': 'Hola 🐝'}],
            'stop_reason': reason or ('tool_use' if first else 'end_turn'), 'usage': usage(first)}


async def send_json(writer, value, status=200):
    payload = json.dumps(value, ensure_ascii=False).encode()
    writer.write(f'HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {len(payload)}\r\nConnection: close\r\n\r\n'.encode() + payload)
    await writer.drain()


async def start(writer, first):
    await headers(writer)
    await data(writer, {'type': 'message_start', 'message': {'type': 'message', 'role': 'assistant', 'content': [],
        'stop_reason': None, 'usage': {**usage(first), 'output_tokens': 0}}})


async def block(writer, first):
    await data(writer, {'type': 'content_block_start', 'index': 0,
        'content_block': {**USE, 'input': {}} if first else {'type': 'text', 'text': ''}})
    for part in (['{"a":2,', '"b":3}'] if first else ['Hola ', '🐝']):
        await data(writer, {'type': 'content_block_delta', 'index': 0,
            'delta': {'type': 'input_json_delta', 'partial_json': part} if first else {'type': 'text_delta', 'text': part}})
    await data(writer, {'type': 'content_block_stop', 'index': 0})


async def finish(writer, first, terminal=True):
    await data(writer, {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use' if first else 'end_turn'},
        'usage': {'output_tokens': 2 if first else 1}})
    if terminal: await data(writer, {'type': 'message_stop'})


@asynccontextmanager
async def serve(handler):
    tasks, writers, errors = set(), set(), []
    requests = 0
    async def connected(reader, writer):
        nonlocal requests
        task = asyncio.current_task(); tasks.add(task); writers.add(writer)
        try:
            header = await reader.readuntil(b'\r\n\r\n')
            lower = header.lower()
            assert lower.startswith(b'post /v1/messages http/1.1')
            assert b'authorization: bearer test-only' in lower
            assert b'anthropic-version: 2026-10-21' in lower
            length = next(int(line.split(b':', 1)[1]) for line in lower.split(b'\r\n') if line.startswith(b'content-length:'))
            body = json.loads(await reader.readexactly(length)); requests += 1
            for key in ('context', 'deadline_unix_ms', 'max_output_tokens', 'stream_options'): assert key not in body
            await handler(body, reader, writer, requests)
        except Exception as error:
            errors.append(error)
        finally:
            writer.close()
            try: await writer.wait_closed()
            except ConnectionError: pass
            writers.discard(writer); tasks.discard(task)
    server = await asyncio.start_server(connected, '127.0.0.1', 0)
    port = server.sockets[0].getsockname()[1]
    try:
        yield lambda: Anthropic(base_url=f'http://127.0.0.1:{port}/v1', api_key='test-only', default_max_output_tokens=9)
    finally:
        server.close(); await server.wait_closed()
        for writer in list(writers): writer.close()
        pending = list(tasks)
        for task in pending: task.cancel()
        if pending: await asyncio.gather(*pending, return_exceptions=True)
        if errors: raise errors[0]


class AnthropicTests(unittest.IsolatedAsyncioTestCase):
    def check_request(self, body, first):
        self.assertEqual(body['max_tokens'], 4 if first else 3)
        self.assertEqual(body['system'], [{'type': 'text', 'text': 'System'}])
        self.assertEqual(body['tools'][0]['input_schema']['type'], 'object')
        if not first:
            self.assertEqual(body['messages'][-2]['content'][0]['type'], 'tool_use')
            self.assertEqual(body['messages'][-1], {'role': 'user', 'content': [{'type': 'tool_result', 'tool_use_id': 'c', 'content': '5'}]})

    async def _roundtrip(self, streaming):
        executions, requests = 0, 0
        gate = asyncio.Event()
        async def handler(body, reader, writer, count):
            nonlocal requests
            requests = count; first = count == 1
            self.check_request(body, first); self.assertEqual(body['stream'], streaming)
            if streaming:
                await start(writer, first)
                if first: await block(writer, first)
                else:
                    await data(writer, {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}})
                    await data(writer, {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': 'Hola '}})
                    await gate.wait()
                    await data(writer, {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': '🐝'}})
                    await data(writer, {'type': 'content_block_stop', 'index': 0})
                await finish(writer, first)
            else: await send_json(writer, reply(first))
        async def execute(args):
            nonlocal executions
            executions += 1
            return args['a'] + args['b']
        async with serve(handler) as provider:
            agent = Agent(model='test', system='System', policy=POLICY, provider=provider(), tools=[tool(execute)])
            text = ''
            if streaming:
                async for event in agent.stream('Add'):
                    if event['type'] == 'tool_call_delta': self.assertEqual(executions, 0)
                    if event['type'] == 'text_delta': text += event['text']; gate.set()
                    if event['type'] == 'result': result = event['result']
            else: result = await agent.run('Add')
            self.assertEqual(result['content'], 'Hola 🐝'); self.assertEqual(result['stats']['model_calls'], 2)
            self.assertEqual(executions, 1); self.assertEqual(requests, 2)
            self.assertEqual(result['stats']['input_tokens'], 25); self.assertEqual(result['stats']['output_tokens'], 3)
            self.assertEqual(result['stats']['reserved_output_tokens'], 0)
            if streaming: self.assertEqual(text, result['content'])

    async def test_json_tools_cache_usage_and_output_reservations(self):
        await self._roundtrip(False)

    async def test_sse_tools_cache_usage_and_output_reservations(self):
        await self._roundtrip(True)

    async def test_incomplete_sse_never_executes_tool_and_preserves_uncertainty(self):
        executions, requests = 0, 0
        async def handler(body, reader, writer, count):
            nonlocal requests
            requests = count; self.assertTrue(body['stream'])
            await start(writer, True); await block(writer, True); await finish(writer, True, False)
        async def execute(args):
            nonlocal executions
            executions += 1
            return 0
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                async with serve(handler) as provider:
                    config = dict(model='test', store=store, agent_id='a', session_id='s', policy=POLICY, provider=provider(), tools=[tool(execute)])
                    with self.assertRaisesRegex(RuntimeError, 'PROVIDER_ERROR'):
                        async for event in Agent(**config).stream('Add'): pass
                    self.assertEqual(executions, 0); self.assertEqual(requests, 1)
                    run = store.inspect(agent_id='a', session_id='s')['run']
                    self.assertEqual(run['in_flight']['kind'], 'model'); self.assertEqual(run['state']['stats']['reserved_output_tokens'], 4)
                    with self.assertRaisesRegex(RuntimeError, 'SESSION_OPERATION_UNCERTAIN'): Agent(**config, resume=True)
            finally: store.close()

    async def test_cancel_before_headers_closes_http(self):
        received, closed = asyncio.Event(), asyncio.Event()
        requests = 0
        async def handler(body, reader, writer, count):
            nonlocal requests
            requests = count; received.set(); await reader.read(); closed.set()
        async with serve(handler) as provider:
            agent = Agent(model='test', policy=POLICY, provider=provider())
            task = asyncio.create_task(agent.run('Wait')); await asyncio.wait_for(received.wait(), 3)
            self.assertTrue(agent.cancel())
            with self.assertRaises(asyncio.CancelledError): await task
            await asyncio.wait_for(closed.wait(), 3); self.assertEqual(requests, 1)

    async def test_resume_keeps_tool_results_cache_usage_and_remaining_cap(self):
        requests, executions, models = 0, 0, 0
        async def handler(body, reader, writer, count):
            nonlocal requests
            requests = count; self.check_request(body, count == 1); await send_json(writer, reply(count == 1))
        async def execute(args):
            nonlocal executions
            executions += 1
            return args['a'] + args['b']
        async def on_event(action):
            nonlocal models
            if action['type'] == 'model':
                models += 1
                if models == 2: raise ValueError('interrupted')
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                async with serve(handler) as provider:
                    config = dict(model='test', system='System', agent_id='a', session_id='s', policy=POLICY, tools=[tool(execute)])
                    with self.assertRaisesRegex(ValueError, 'interrupted'):
                        await Agent(**config, store=store, provider=provider(), on_event=on_event).run('Add')
                    self.assertEqual(requests, 1); self.assertEqual(executions, 1)
                    store.close(); store = SessionStore(directory)
                    result = await Agent(**config, store=store, provider=provider(), resume=True).resume()
                    self.assertEqual(result['content'], 'Hola 🐝'); self.assertEqual(result['stats']['input_tokens'], 25)
                    self.assertEqual(result['stats']['output_tokens'], 3); self.assertEqual(result['stats']['model_calls'], 2)
                    self.assertEqual(executions, 1); self.assertEqual(requests, 2)
            finally: store.close()

    async def test_truncation_pauses_refusals_and_http_errors_never_retry(self):
        for reason in ('max_tokens', 'pause_turn', 'refusal', 'http_error'):
            requests, executions = 0, 0
            async def handler(body, reader, writer, count):
                nonlocal requests
                requests = count
                await send_json(writer, {'private': 'vendor message'} if reason == 'http_error' else reply(True, reason), 429 if reason == 'http_error' else 200)
            async def execute(args):
                nonlocal executions
                executions += 1
                return 0
            async with serve(handler) as provider:
                agent = Agent(model='test', policy=POLICY, provider=provider(), tools=[tool(execute)])
                with self.assertRaisesRegex(RuntimeError, 'PROVIDER_ERROR'): await agent.run('Add')
                self.assertEqual(requests, 1); self.assertEqual(executions, 0)
