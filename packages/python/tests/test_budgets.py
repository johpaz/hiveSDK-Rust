import asyncio
import json
import tempfile
import time
import unittest
from hive_sdk import Agent, DeadlineExceeded, SessionStore, Tool
from test_streaming import serve, headers, data, finish


def tool(execute, **kwargs):
    return Tool('read', 'Read', {'type': 'object'}, execute, **kwargs)


def reply(output, use_tool=False):
    return {'content': None if use_tool else 'done', 'tool_calls': [{'id': 'c', 'name': 'read', 'arguments': {}}] if use_tool else [],
            'usage': {'input_tokens': 1, 'output_tokens': output}}


class BudgetTests(unittest.IsolatedAsyncioTestCase):
    async def test_reservation_is_durable_before_dispatch_and_cap_shrinks(self):
        calls = 0
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            class Provider:
                async def complete(inner, request):
                    nonlocal calls
                    calls += 1; expected = 4 if calls == 1 else 2
                    self.assertEqual(request['max_output_tokens'], expected)
                    self.assertEqual(store.inspect(agent_id='a', session_id='s')['run']['state']['stats']['reserved_output_tokens'], expected)
                    return reply(3 if calls == 1 else 2, calls == 1)
            try:
                agent = Agent(model='test', store=store, agent_id='a', session_id='s', tools=[tool(lambda args: 'ok')],
                    provider=Provider(), policy={'max_output_tokens': 4, 'max_total_output_tokens': 5})
                result = await agent.run('Read')
                self.assertEqual(result['status'], 'completed'); self.assertEqual(result['stats']['output_tokens'], 5)
                self.assertEqual(result['stats']['reserved_output_tokens'], 0); self.assertEqual(calls, 2)
            finally: store.close()

    async def test_modern_and_legacy_caps_are_sent_for_json_and_sse(self):
        parameter, requests = '', 0
        async def handler(body, reader, writer):
            nonlocal requests
            requests += 1; self.assertEqual(body[parameter], 2)
            self.assertNotIn('max_tokens' if parameter == 'max_completion_tokens' else 'max_completion_tokens', body)
            self.assertNotIn('deadline_unix_ms', body); self.assertNotIn('max_output_tokens', body)
            if body['stream']:
                await headers(writer); await data(writer, {'choices': [{'delta': {'content': 'done'}}]})
                await finish(writer, 'stop', {'prompt_tokens': 1, 'completion_tokens': 2})
            else:
                payload = json.dumps({'choices': [{'message': {'content': 'done'}}], 'usage': {'prompt_tokens': 1, 'completion_tokens': 2}}).encode()
                writer.write(f'HTTP/1.1 200 OK\r\nContent-Length: {len(payload)}\r\n\r\n'.encode() + payload); await writer.drain()
        async with serve(handler) as provider:
            for parameter in ('max_completion_tokens', 'max_tokens'):
                for streaming in (False, True):
                    agent = Agent(model='test', provider=provider(output_token_parameter=parameter),
                        policy={'max_output_tokens': 2, 'max_run_ms': 3000})
                    if streaming:
                        async for event in agent.stream('Read'): pass
                    else: self.assertEqual((await agent.run('Read'))['content'], 'done')
        self.assertEqual(requests, 4)

    async def test_deadline_after_hook_stops_before_provider(self):
        calls = 0
        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1; return reply(1)
        async def on_event(action):
            if action['type'] == 'model': await asyncio.sleep(0.05)
        result = await Agent(model='test', provider=Provider(), on_event=on_event, policy={'max_run_ms': 20}).run('Read')
        self.assertEqual(result['stop_reason'], 'deadline_exceeded'); self.assertEqual(calls, 0)

    async def test_global_deadline_closes_http_and_retains_unknown_reservation(self):
        received, closed = asyncio.Event(), asyncio.Event()
        async def handler(body, reader, writer):
            received.set(); await reader.read(); closed.set()
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                async with serve(handler) as provider:
                    config = dict(model='test', store=store, agent_id='a', session_id='s', provider=provider(timeout_ms=5000),
                        policy={'max_run_ms': 1500, 'max_total_output_tokens': 7})
                    task = asyncio.create_task(Agent(**config).run('Wait')); await asyncio.wait_for(received.wait(), 3)
                    with self.assertRaises(DeadlineExceeded): await task
                    await asyncio.wait_for(closed.wait(), 3)
                    run = store.inspect(agent_id='a', session_id='s')['run']
                    self.assertEqual(run['in_flight']['kind'], 'model'); self.assertEqual(run['state']['stats']['reserved_output_tokens'], 7)
                    with self.assertRaisesRegex(RuntimeError, 'SESSION_OPERATION_UNCERTAIN'): Agent(**config, resume=True)
            finally: store.close()

    async def test_expired_clean_resume_preserves_deadline(self):
        calls = 0
        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1; return reply(1)
        async def on_event(action):
            if action['type'] == 'model': raise ValueError('interrupted')
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                config = dict(model='test', store=store, agent_id='a', session_id='s', provider=Provider(),
                    policy={'max_run_ms': 500, 'max_total_output_tokens': 7})
                with self.assertRaisesRegex(ValueError, 'interrupted'): await Agent(**config, on_event=on_event).run('Read')
                deadline = store.inspect(agent_id='a', session_id='s')['run']['state']['deadline_unix_ms']
                await asyncio.sleep(max(0, (deadline-time.time()*1000+30)/1000))
                result = await Agent(**config, resume=True).resume()
                self.assertEqual(result['stop_reason'], 'deadline_exceeded'); self.assertEqual(calls, 0)
                state = store.inspect(agent_id='a', session_id='s')['run']['state']
                self.assertEqual(state['deadline_unix_ms'], deadline); self.assertEqual(state['stats']['reserved_output_tokens'], 0)
            finally: store.close()

    async def test_known_late_tool_result_is_saved_without_paid_continuation(self):
        calls = 0
        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1; return reply(1, True)
        async def execute(args):
            try: await asyncio.sleep(3600)
            except asyncio.CancelledError: return {'known': True}
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                agent = Agent(model='test', store=store, agent_id='a', session_id='s', provider=Provider(),
                    tools=[tool(execute)], policy={'max_run_ms': 1500})
                result = await agent.run('Read')
                self.assertEqual(result['stop_reason'], 'deadline_exceeded'); self.assertEqual(calls, 1)
                run = store.inspect(agent_id='a', session_id='s')['run']
                self.assertIsNone(run['in_flight']); self.assertEqual(run['tools'][0]['result'], {'known': True})
                self.assertEqual(run['tools'][0]['status'], 'completed')
            finally: store.close()
