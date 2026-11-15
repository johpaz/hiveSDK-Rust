import json
import tempfile
import unittest
from hive_sdk import Agent, SessionStore, Tool

POLICY = {'max_context_bytes': 500, 'context_overflow': 'drop_oldest_turns'}


def size(request):
    return len(json.dumps({k: request[k] for k in ('model', 'messages', 'tools')},
                          ensure_ascii=False, separators=(',', ':')).encode('utf-8'))


class ContextTests(unittest.IsolatedAsyncioTestCase):
    async def test_oversized_context_does_not_dispatch_or_reserve_output(self):
        calls = 0
        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1
                return {'content': 'unexpected'}
        agent = Agent(model='test', provider=Provider(),
                      policy={'max_context_bytes': 100, 'max_total_output_tokens': 10})
        result = await agent.run('x' * 1000)
        self.assertEqual(result['stop_reason'], 'context_limit')
        self.assertEqual(calls, 0); self.assertEqual(result['stats']['reserved_output_tokens'], 0)
        self.assertEqual(agent.messages[-1]['content'], 'x' * 1000)

    async def test_bounded_request_survives_resume_and_retains_full_history(self):
        calls, interrupted = 0, False
        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1
                self.assertEqual(request['context']['serialized_bytes'], size(request))
                if calls == 2:
                    self.assertEqual(request['context']['dropped_messages'], 2)
                    self.assertEqual([m['content'] for m in request['messages']], ['System', 'current' * 20])
                return {'content': 'finished'}
        async def on_event(action):
            if interrupted and action['type'] == 'model': raise ValueError('interrupted')
        limit = size({'model': 'test', 'tools': [], 'messages': [
            {'role': 'system', 'content': 'System'}, {'role': 'user', 'content': 'old' * 60}]})
        config = dict(model='test', system='System', policy={**POLICY, 'max_context_bytes': limit},
                      agent_id='a', session_id='s', provider=Provider())
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                agent = Agent(**config, store=store, on_event=on_event)
                await agent.run('old' * 60); interrupted = True
                with self.assertRaisesRegex(ValueError, 'interrupted'): await agent.run('current' * 20)
                before = store.inspect(agent_id='a', session_id='s')['run']
                self.assertEqual(len(before['state']['messages']), 4)
                self.assertEqual(before['state']['action']['request']['context']['dropped_messages'], 2)
                self.assertIsNone(before['in_flight']); self.assertEqual(calls, 1)
                store.close(); store = SessionStore(directory)
                resumed = Agent(**config, store=store, resume=True)
                self.assertEqual((await resumed.resume())['status'], 'completed'); self.assertEqual(calls, 2)
                self.assertEqual(len(resumed.messages), 5)
                self.assertEqual(resumed.messages[1]['content'], 'old' * 60)
            finally: store.close()

    async def test_large_active_tool_result_is_durable_before_context_stop(self):
        calls = 0
        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1
                return {'content': None, 'tool_calls': [{'id': 'c', 'name': 'read', 'arguments': {}}]}
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                agent = Agent(model='test', provider=Provider(), store=store, agent_id='a', session_id='s', policy=POLICY,
                              tools=[Tool('read', 'Read', {'type': 'object'}, lambda args: 'x' * 1000)])
                result = await agent.run('Read')
                self.assertEqual(result['stop_reason'], 'context_limit'); self.assertEqual(calls, 1)
                self.assertEqual(result['stats']['tool_calls'], 1)
                run = store.inspect(agent_id='a', session_id='s')['run']
                self.assertEqual(run['tools'][0]['status'], 'completed')
                self.assertEqual(run['tools'][0]['result'], 'x' * 1000)
                self.assertIsNone(run['in_flight']); self.assertEqual(agent.messages[-1]['role'], 'tool')
            finally: store.close()

    async def test_streaming_provider_gets_exact_utf8_json_bytes_and_unchanged_text(self):
        text, calls = 'é🦀\n"\\', 0
        class Provider:
            async def stream(inner, request):
                nonlocal calls
                calls += 1
                self.assertEqual(request['context']['serialized_bytes'], size(request))
                self.assertEqual(request['messages'][0]['content'], text)
                self.assertEqual(request['context']['dropped_messages'], 0)
                yield {'type': 'text_delta', 'text': 'ok'}
                yield {'type': 'reply', 'reply': {'content': 'ok'}}
        agent = Agent(model='test', provider=Provider(), policy=POLICY)
        events = [event async for event in agent.stream(text)]
        self.assertEqual(calls, 1); self.assertEqual(events[-1]['result']['content'], 'ok')
        self.assertEqual(next(e for e in events if e['type'] == 'text_delta')['text'], 'ok')
