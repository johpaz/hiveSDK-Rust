import tempfile
import unittest
from hive_sdk import Agent, SessionStore, Tool


def budget(encoding='cl100k_base', max_tokens=160):
    return {'encoding': encoding, 'max_tokens': max_tokens, 'overhead_tokens': 7}


class TokenTests(unittest.IsolatedAsyncioTestCase):
    async def test_provider_framing_and_exact_combined_window(self):
        for framing in ['neutral_json', 'openai_chat_json', 'anthropic_messages_json', 'gemini_generate_content_json']:
            estimates, calls = [], []
            class Provider:
                async def complete(inner, request):
                    calls.append(True); estimates.append(request['context']['token_estimate']['total_tokens'])
                    self.assertEqual(request['context']['token_estimate']['framing'], framing)
                    return {'content': 'done', 'usage': {'input_tokens': 1, 'output_tokens': 1}}
            policy = {'input_token_budget': {**budget(), 'framing': framing}, 'max_output_tokens': 10}
            probe = Agent(model='test', provider=Provider(), policy=policy); await probe.run('Hello 🦀'); probe.close()
            for extra, expected in [(10, 'completed'), (9, 'stopped')]:
                before = len(calls)
                agent = Agent(model='test', provider=Provider(), policy={**policy,
                    'input_token_budget': {**policy['input_token_budget'], 'context_window_tokens': estimates[0] + extra}})
                result = await agent.run('Hello 🦀'); agent.close()
                self.assertEqual(result['status'], expected); self.assertEqual(len(calls) - before, int(extra == 10))
                if extra == 9: self.assertEqual(result['stop_reason'], 'context_limit')

    async def test_accumulated_input_and_pending_tool_preserve_budget_on_reopen(self):
        estimates, calls, effects = [], [], []
        def execute(args): effects.append(True); return 5
        tools = [Tool(name='read', description='Read', parameters={'type': 'object'}, execute=execute)]
        policy = {'input_token_budget': {**budget(), 'framing': 'openai_chat_json'}}
        class Probe:
            async def complete(inner, request): estimates.append(request['context']['token_estimate']['total_tokens']); return {'content': 'done'}
        probe = Agent(model='test', tools=tools, policy=policy, provider=Probe()); await probe.run('Read'); probe.close()
        policy['input_token_budget']['max_total_tokens'] = estimates[0]
        class Provider:
            async def complete(inner, request):
                calls.append(True)
                return {'content': None, 'tool_calls': [{'id': 'c', 'name': 'read', 'arguments': {}}], 'usage': {'input_tokens': 1, 'output_tokens': 1}}
        async def on_event(action):
            if action['type'] == 'tool': raise ValueError('pause')
        config = dict(model='test', tools=tools, policy=policy, provider=Provider(), agent_id='a', session_id='s')
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                agent = Agent(**config, store=store, on_event=on_event)
                with self.assertRaisesRegex(ValueError, 'pause'): await agent.run('Read')
                agent.close(); self.assertEqual(effects, []); store.close(); store = SessionStore(directory)
                resumed = Agent(**config, store=store, resume=True); result = await resumed.resume(); resumed.close()
                self.assertEqual(result['stop_reason'], 'input_limit'); self.assertEqual((len(calls), len(effects)), (1, 1))
                self.assertEqual(result['stats']['estimated_input_tokens'], estimates[0])
                self.assertEqual(result['stats']['charged_input_tokens'], estimates[0])
                self.assertEqual(result['stats']['input_tokens'], 1); self.assertEqual(result['stats']['reserved_input_tokens'], 0)
                self.assertEqual(store.inspect(agent_id='a', session_id='s')['run']['tools'][0]['status'], 'completed')
            finally: store.close()

    async def test_input_total_missing_usage_and_overrun_prevent_effects(self):
        for usage in [None, {'input_tokens': 1001, 'output_tokens': 1}]:
            class Provider:
                async def complete(inner, request):
                    return {'content': None, 'tool_calls': [{'id': 'c', 'name': 'read', 'arguments': {}}], 'usage': usage}
            def execute(args): self.fail('no effect')
            agent = Agent(model='test', provider=Provider(), policy={'input_token_budget': {**budget(), 'max_total_tokens': 1000}},
                tools=[Tool(name='read', description='Read', parameters={'type': 'object'}, execute=execute)])
            result = await agent.run('Read'); agent.close()
            self.assertEqual(result['stop_reason'], 'input_limit' if usage else 'usage_unavailable')
            self.assertEqual(result['stats']['tool_calls'], 0); self.assertEqual(result['stats']['reserved_input_tokens'], 0)

    async def test_both_encodings_report_estimates_without_inventing_actual_usage(self):
        calls = 0
        for encoding in ('cl100k_base', 'o200k_base'):
            for streaming in (False, True):
                class Provider:
                    async def stream(inner, request):
                        nonlocal calls
                        calls += 1
                        estimate = request['context']['token_estimate']
                        self.assertEqual(estimate['encoding'], encoding)
                        self.assertEqual(estimate['overhead_tokens'], 7)
                        self.assertEqual(estimate['total_tokens'], estimate['serialized_tokens'] + 7)
                        self.assertLessEqual(estimate['total_tokens'], 160)
                        self.assertGreater(estimate['serialized_tokens'], 0)
                        self.assertEqual(request['messages'][0]['content'], 'Hola 🦀\n"\\ <|endoftext|>')
                        yield {'type': 'text_delta', 'text': 'done'}
                        yield {'type': 'reply', 'reply': {'content': 'done', 'usage': {'input_tokens': 123, 'output_tokens': 1}}}
                agent = Agent(model='private-alias', provider=Provider(), policy={'input_token_budget': budget(encoding)})
                if streaming:
                    events = [event async for event in agent.stream('Hola 🦀\n"\\ <|endoftext|>')]
                    result = events[-1]['result']
                else:
                    result = await agent.run('Hola 🦀\n"\\ <|endoftext|>')
                self.assertEqual(result['status'], 'completed'); self.assertEqual(result['stats']['input_tokens'], 123)
                self.assertEqual(result['stats']['model_calls'], 1)
        self.assertEqual(calls, 4)

    async def test_low_limit_never_dispatches_or_reserves_and_invalid_profiles_fail(self):
        calls = 0
        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1
                return {'content': 'unexpected'}
        result = await Agent(model='test', provider=Provider(),
            policy={'input_token_budget': budget(max_tokens=8), 'max_total_output_tokens': 30}).run('Hello')
        self.assertEqual(result['stop_reason'], 'context_limit'); self.assertEqual(calls, 0)
        self.assertEqual(result['stats']['reserved_output_tokens'], 0)
        for profile in (budget('unknown'), budget(max_tokens=0), budget(max_tokens=7), {**budget(), 'typo': 1}):
            with self.assertRaises(RuntimeError):
                Agent(model='test', provider=Provider(), policy={'input_token_budget': profile})

    async def test_token_only_selection_survives_reopen_with_full_history(self):
        calls, interrupted = 0, False
        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1
                self.assertLessEqual(request['context']['token_estimate']['total_tokens'], 160)
                if calls == 2:
                    self.assertEqual(request['context']['dropped_messages'], 2)
                    self.assertEqual([m['content'] for m in request['messages']], ['System', 'current ' * 100])
                return {'content': 'finished'}
        async def on_event(action):
            if interrupted and action['type'] == 'model': raise ValueError('interrupted')
        config = dict(model='test', system='System', agent_id='a', session_id='s', provider=Provider(),
            policy={'input_token_budget': budget(), 'context_overflow': 'drop_oldest_turns'})
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                agent = Agent(**config, store=store, on_event=on_event)
                await agent.run('old ' * 100); interrupted = True
                with self.assertRaisesRegex(ValueError, 'interrupted'): await agent.run('current ' * 100)
                before = store.inspect(agent_id='a', session_id='s')['run']
                self.assertIsNone(before['in_flight']); self.assertEqual(len(before['state']['messages']), 4)
                self.assertEqual(before['state']['action']['request']['context']['dropped_messages'], 2)
                store.close(); store = SessionStore(directory)
                resumed = Agent(**config, store=store, resume=True)
                self.assertEqual((await resumed.resume())['status'], 'completed'); self.assertEqual(calls, 2)
                self.assertEqual(len(resumed.messages), 5); self.assertEqual(resumed.messages[1]['content'], 'old ' * 100)
            finally: store.close()

    async def test_large_known_tool_result_is_saved_without_another_model_call(self):
        calls = 0
        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1
                return {'content': None, 'tool_calls': [{'id': 'c', 'name': 'read', 'arguments': {}}],
                        'usage': {'input_tokens': 1, 'output_tokens': 1}}
        async def execute(args): return 'result ' * 1000
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                agent = Agent(model='test', provider=Provider(), store=store, agent_id='a', session_id='s',
                    tools=[Tool('read', 'Read', {'type': 'object'}, execute)],
                    policy={'input_token_budget': budget(), 'max_context_bytes': 100_000, 'max_total_output_tokens': 30})
                result = await agent.run('Read')
                self.assertEqual(result['stop_reason'], 'context_limit'); self.assertEqual(calls, 1)
                self.assertEqual(result['stats']['reserved_output_tokens'], 0); self.assertEqual(result['stats']['input_tokens'], 1)
                run = store.inspect(agent_id='a', session_id='s')['run']
                self.assertEqual(run['tools'][0]['status'], 'completed')
                self.assertEqual(run['tools'][0]['result'], 'result ' * 1000); self.assertIsNone(run['in_flight'])
            finally: store.close()
