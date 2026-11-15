import asyncio
import unittest
from contextlib import aclosing
from hive_sdk import Agent, Tool
from test_agent import Scripted

TOOL_REPLY = {'content': 'Reading', 'tool_calls': [{'id': 'c', 'name': 'read', 'arguments': {}}], 'usage': {'input_tokens': 3, 'output_tokens': 2}}


class HandleTests(unittest.IsolatedAsyncioTestCase):
    async def test_failed_work_after_known_tool_preserves_committed_getters(self):
        models, effects = 0, 0
        def execute(args):
            nonlocal effects
            effects += 1; self.assertEqual(len(agent.messages), 2); return 5
        def complete(request):
            nonlocal models
            models += 1
            if models == 2: return TOOL_REPLY
            if models == 3: raise ValueError('provider failed after known tool')
            if models == 4: self.assertEqual([message['role'] for message in request['messages']], ['user', 'assistant', 'user'])
            return {'content': 'Known 🦀'}
        agent = Agent(model='test', provider=Scripted(complete), tools=[Tool('read', 'Read', {'type': 'object'}, execute)])
        await agent.run('History'); before = agent.messages
        with self.assertRaisesRegex(ValueError, 'provider failed'): await agent.run('Failed task')
        self.assertEqual(agent.messages, before); self.assertEqual(effects, 1)
        self.assertEqual((await agent.run('Explicit new task'))['content'], 'Known 🦀'); self.assertEqual(effects, 1)
        copy = agent.messages; copy[0]['content'] = 'changed'; self.assertEqual(agent.messages[0]['content'], 'History')
        agent.close(); self.assertEqual(len(agent.messages), 4)

    async def test_terminal_observer_failure_discards_work(self):
        fail = True
        def observe(action):
            nonlocal fail
            if fail and action['type'] == 'complete':
                fail = False; raise ValueError('observer failed')
        agent = Agent(model='test', provider=Scripted(lambda request: {'content': 'Known'}), on_event=observe)
        with self.assertRaisesRegex(ValueError, 'observer failed'): await agent.run('Failed')
        self.assertEqual(agent.messages, [])
        await agent.run('Explicit'); self.assertEqual(agent.messages[0]['content'], 'Explicit'); self.assertEqual(len(agent.messages), 2)

    async def test_closing_iterator_after_tool_discards_active_turn(self):
        models, effects, model_actions = 0, 0, 0
        def execute(args):
            nonlocal effects
            effects += 1; return 5
        def complete(request):
            nonlocal models
            models += 1
            return TOOL_REPLY if models == 1 else {'content': 'Explicit result'}
        agent = Agent(model='test', provider=Scripted(complete), tools=[Tool('read', 'Read', {'type': 'object'}, execute)])
        async with aclosing(agent.stream('Interrupted')) as events:
            async for event in events:
                if event['type'] == 'model':
                    model_actions += 1
                    if model_actions == 2: break
        self.assertEqual(models, 1); self.assertEqual(effects, 1); self.assertEqual(agent.messages, [])
        await agent.run('Explicit'); self.assertEqual(models, 2); self.assertEqual(effects, 1); self.assertEqual(len(agent.messages), 2)

    async def test_cancellation_after_tool_discards_work_and_agent_remains_reusable(self):
        ready = asyncio.Event(); models = 0
        class Provider:
            with_context = True
            async def complete(self, request, context):
                nonlocal models
                models += 1
                if models == 1: return TOOL_REPLY
                if models == 2:
                    ready.set(); await context['cancel_event'].wait(); raise asyncio.CancelledError
                return {'content': 'New'}
        agent = Agent(model='test', provider=Provider(), tools=[Tool('read', 'Read', {'type': 'object'}, lambda args: 5)])
        pending = asyncio.create_task(agent.run('Cancel')); await ready.wait(); agent.cancel()
        with self.assertRaises(asyncio.CancelledError): await pending
        self.assertEqual(agent.messages, []); await agent.run('Explicit'); self.assertEqual(len(agent.messages), 2)
