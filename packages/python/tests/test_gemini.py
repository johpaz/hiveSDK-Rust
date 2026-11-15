import asyncio
from contextlib import asynccontextmanager
import json
import tempfile
import unittest
from hive_sdk import Agent, Gemini, SessionStore, Tool
from test_streaming import data, headers

POLICY = {'max_output_tokens': 4, 'max_total_output_tokens': 5, 'max_context_bytes': 10000}


def tool(execute):
    return Tool('add', 'Add', {'type': 'object', 'properties': {'a': {'type': 'number'}, 'b': {'type': 'number'}}, 'required': ['a', 'b']}, execute)


def parts(first):
    return ([{'text': '', 'thoughtSignature': 'text-sig'}, {'functionCall': {'id': 'c', 'name': 'add', 'args': {'a': 2, 'b': 3}}, 'thoughtSignature': 'tool-sig'}]
            if first else [{'text': 'Hola '}, {'text': '🐝'}, {'text': '', 'thoughtSignature': 'tail'}])


def usage(first):
    return {'promptTokenCount': 10 if first else 5, 'cachedContentTokenCount': 4 if first else 0,
            'candidatesTokenCount': 1, 'thoughtsTokenCount': 1 if first else 0}


def reply(first, reason='STOP'):
    return {'candidates': [{'index': 0, 'content': {'role': 'model', 'parts': parts(first)}, 'finishReason': reason}], 'usageMetadata': usage(first)}


async def send_json(writer, value, status=200):
    payload = json.dumps(value, ensure_ascii=False).encode()
    writer.write(f'HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {len(payload)}\r\nConnection: close\r\n\r\n'.encode() + payload)
    await writer.drain()


async def block(writer, part):
    await data(writer, {'candidates': [{'index': 0, 'content': {'role': 'model', 'parts': [part]}}]})


async def finish(writer, first, terminal=True):
    if terminal: await data(writer, {'candidates': [{'index': 0, 'finishReason': 'STOP'}], 'usageMetadata': usage(first)})


@asynccontextmanager
async def serve(handler):
    tasks, writers, errors = set(), set(), []
    requests = 0
    async def connected(reader, writer):
        nonlocal requests
        task = asyncio.current_task(); tasks.add(task); writers.add(writer)
        try:
            header = await reader.readuntil(b'\r\n\r\n'); lower = header.lower()
            route = header.split(b' ')[1]
            assert route in (b'/v1beta/models/test:generateContent', b'/v1beta/models/test:streamGenerateContent?alt=sse')
            assert b'x-goog-api-key: test-only' in lower; assert b'authorization:' not in lower
            length = next(int(line.split(b':', 1)[1]) for line in lower.split(b'\r\n') if line.startswith(b'content-length:'))
            body = json.loads(await reader.readexactly(length)); requests += 1
            for key in ('model', 'stream', 'context', 'deadline_unix_ms', 'provider_state'): assert key not in body
            await handler(body, reader, writer, requests, b'streamGenerateContent' in route)
        except Exception as error: errors.append(error)
        finally:
            writer.close()
            try: await writer.wait_closed()
            except ConnectionError: pass
            writers.discard(writer); tasks.discard(task)
    server = await asyncio.start_server(connected, '127.0.0.1', 0)
    port = server.sockets[0].getsockname()[1]
    try: yield lambda: Gemini(base_url=f'http://127.0.0.1:{port}/v1beta', api_key='test-only', default_max_output_tokens=9)
    finally:
        server.close(); await server.wait_closed()
        for writer in list(writers): writer.close()
        pending = list(tasks)
        for task in pending: task.cancel()
        if pending: await asyncio.gather(*pending, return_exceptions=True)
        if errors: raise errors[0]


class GeminiTests(unittest.IsolatedAsyncioTestCase):
    def check_request(self, body, first):
        self.assertEqual(body['generationConfig'], {'candidateCount': 1, 'maxOutputTokens': 4 if first else 3})
        self.assertEqual(body['systemInstruction'], {'parts': [{'text': 'System'}]})
        self.assertEqual(body['tools'][0]['functionDeclarations'][0]['parametersJsonSchema']['type'], 'object')
        if not first:
            self.assertEqual(body['contents'][-2], {'role': 'model', 'parts': parts(True)})
            self.assertEqual(body['contents'][-1], {'role': 'user', 'parts': [{'functionResponse': {'id': 'c', 'name': 'add', 'response': {'result': 5}}}]})

    async def _roundtrip(self, streaming):
        executions, requests = 0, 0
        gate = asyncio.Event()
        async def handler(body, reader, writer, count, stream):
            nonlocal requests
            requests = count; first = count == 1; self.check_request(body, first); self.assertEqual(stream, streaming)
            if stream:
                await headers(writer)
                for index, part in enumerate(parts(first)):
                    await block(writer, part)
                    if not first and index == 0: await gate.wait()
                await finish(writer, first)
            else: await send_json(writer, reply(first))
        async def execute(args):
            nonlocal executions
            executions += 1
            return args['a'] + args['b']
        async with serve(handler) as provider:
            agent = Agent(model='models/test', system='System', policy=POLICY, provider=provider(), tools=[tool(execute)])
            text = ''
            if streaming:
                async for event in agent.stream('Add'):
                    if event['type'] == 'tool_call_delta': self.assertEqual(executions, 0)
                    if event['type'] == 'text_delta':
                        text += event['text']
                        if event['text']: gate.set()
                    if event['type'] == 'result': result = event['result']
            else: result = await agent.run('Add')
            self.assertEqual(result['content'], 'Hola 🐝'); self.assertEqual(executions, 1); self.assertEqual(requests, 2)
            self.assertEqual(result['stats']['input_tokens'], 15); self.assertEqual(result['stats']['output_tokens'], 3)
            self.assertEqual(result['stats']['model_calls'], 2); self.assertEqual(result['stats']['reserved_output_tokens'], 0)
            self.assertEqual(agent.messages[-1]['provider_state']['parts'], parts(False))
            if streaming: self.assertEqual(text, result['content'])

    async def test_json_tools_signatures_caps_cache_and_thought_usage(self):
        await self._roundtrip(False)

    async def test_sse_tools_signatures_caps_cache_and_thought_usage(self):
        await self._roundtrip(True)

    async def test_missing_stop_never_executes_function_and_keeps_uncertain_reservation(self):
        executions, requests = 0, 0
        async def handler(body, reader, writer, count, stream):
            nonlocal requests
            requests = count; self.assertTrue(stream); await headers(writer)
            for part in parts(True): await block(writer, part)
            await finish(writer, True, False)
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

    async def test_cancellation_closes_pending_native_http(self):
        received, closed = asyncio.Event(), asyncio.Event()
        requests = 0
        async def handler(body, reader, writer, count, stream):
            nonlocal requests
            requests = count; received.set(); await reader.read(); closed.set()
        async with serve(handler) as provider:
            agent = Agent(model='test', policy=POLICY, provider=provider()); task = asyncio.create_task(agent.run('Wait'))
            await asyncio.wait_for(received.wait(), 3); self.assertTrue(agent.cancel())
            with self.assertRaises(asyncio.CancelledError): await task
            await asyncio.wait_for(closed.wait(), 3); self.assertEqual(requests, 1)

    async def test_json_to_sse_resume_keeps_signed_parts_and_never_replays_tool(self):
        requests, executions, models = 0, 0, 0
        async def handler(body, reader, writer, count, stream):
            nonlocal requests
            requests = count; first = count == 1; self.check_request(body, first)
            if stream:
                await headers(writer)
                for part in parts(first): await block(writer, part)
                await finish(writer, first)
            else: await send_json(writer, reply(first))
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
                    config = dict(model='test', system='System', policy=POLICY, agent_id='a', session_id='s', tools=[tool(execute)])
                    with self.assertRaisesRegex(ValueError, 'interrupted'): await Agent(**config, store=store, provider=provider(), on_event=on_event).run('Add')
                    self.assertEqual(requests, 1); self.assertEqual(executions, 1); store.close(); store = SessionStore(directory)
                    agent = Agent(**config, store=store, provider=provider(), resume=True)
                    events = [event async for event in agent.resume_stream()]; result = events[-1]['result']
                    self.assertEqual(result['content'], 'Hola 🐝'); self.assertEqual(result['stats']['input_tokens'], 15)
                    self.assertEqual(result['stats']['output_tokens'], 3); self.assertEqual(requests, 2); self.assertEqual(executions, 1)
                    self.assertEqual(agent.messages[2]['provider_state']['parts'], parts(True))
            finally: store.close()

    async def test_truncation_safety_malformed_calls_and_http_errors_never_retry(self):
        for reason in ('MAX_TOKENS', 'SAFETY', 'MALFORMED_FUNCTION_CALL', 'http_error'):
            requests, executions = 0, 0
            async def handler(body, reader, writer, count, stream):
                nonlocal requests
                requests = count
                await send_json(writer, {'private': 'vendor message'} if reason == 'http_error' else reply(True, reason), 429 if reason == 'http_error' else 200)
            async def execute(args):
                nonlocal executions
                executions += 1
                return 0
            async with serve(handler) as provider:
                with self.assertRaisesRegex(RuntimeError, 'PROVIDER_ERROR'): await Agent(model='test', policy=POLICY, provider=provider(), tools=[tool(execute)]).run('Add')
                self.assertEqual(requests, 1); self.assertEqual(executions, 0)
