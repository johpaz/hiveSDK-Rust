import asyncio
import unittest

from hive_sdk import Agent, Objective, ObjectiveLimit, Tool, ToolOperationUncertain


def reply(content='done', output=1):
    return {'content': content, 'usage': {'input_tokens': 1, 'output_tokens': output}}


def call(arguments=None):
    return {**reply(None), 'tool_calls': [{'id': 'delegate-1', 'name': 'delegate', 'arguments': arguments or {}}]}


def tool(execute):
    return Tool(name='delegate', description='Delegate explicitly', parameters={'type': 'object'}, execute=execute, with_context=True)


class Provider:
    def __init__(self, callback):
        self.complete = callback


class ObjectiveTests(unittest.IsolatedAsyncioTestCase):
    async def test_shared_calls_never_reset_on_new_run(self):
        objective = Objective({'max_model_calls': 2})
        calls = []
        async def child_reply(request):
            calls.append('child')
            return reply()
        async def parent_reply(request):
            calls.append('parent')
            return call()
        async def delegate(args, context):
            return (await context.delegate(child, 'Work'))['content']
        child = Agent(model='child', provider=Provider(child_reply))
        parent = Agent(model='parent', objective=objective, provider=Provider(parent_reply), tools=[tool(delegate)])
        with self.assertRaises(ObjectiveLimit): await parent.run('Work')
        self.assertEqual(calls, ['parent', 'child'])
        self.assertEqual(objective.inspect()['model_calls'], 2)
        self.assertEqual(objective.inspect()['tool_calls'], 1)
        with self.assertRaises(ObjectiveLimit): await parent.run('Again')
        self.assertEqual(len(calls), 2)
        parent.close(); child.close()

    async def test_output_reservation_reconciles_once_and_shrinks_cap(self):
        objective = Objective({'max_output_tokens': 10, 'max_total_output_tokens': 10})
        caps = []
        parent_calls = 0
        async def child_reply(request):
            caps.append(request['max_output_tokens'])
            return reply('child', 2)
        async def parent_reply(request):
            nonlocal parent_calls
            caps.append(request['max_output_tokens'])
            parent_calls += 1
            return call() if parent_calls == 1 else reply('finished', 3)
        async def delegate(args, context):
            return (await context.delegate(child, 'Work'))['content']
        child = Agent(model='child', provider=Provider(child_reply))
        parent = Agent(model='parent', objective=objective, provider=Provider(parent_reply), tools=[tool(delegate)])
        self.assertEqual((await parent.run('Work'))['content'], 'finished')
        self.assertEqual(caps, [10, 9, 7])
        view = objective.inspect()
        self.assertEqual((view['model_calls'], view['input_tokens'], view['output_tokens']), (3, 3, 6))
        self.assertEqual(view['reservations'], {})
        parent.close(); child.close()

    async def test_depth_and_changing_objective_fail_before_child_dispatch(self):
        for different in [False, True]:
            objective = Objective({'max_depth': 2 if different else 0})
            child_calls = []
            async def child_reply(request):
                child_calls.append(True)
                return reply()
            async def parent_reply(request): return call()
            async def delegate(args, context): return await context.delegate(child, 'Work')
            child = Agent(model='child', objective=Objective() if different else None, provider=Provider(child_reply))
            parent = Agent(model='parent', objective=objective, provider=Provider(parent_reply), tools=[tool(delegate)])
            with self.assertRaises(ObjectiveLimit): await parent.run('Work')
            self.assertEqual(child_calls, [])
            self.assertEqual(objective.inspect()['model_calls'], 1)
            parent.close(); child.close()

    async def test_rejected_schema_consumes_shared_attempt_without_effects(self):
        objective = Objective({'max_tool_calls': 1})
        calls = 0
        effects = []
        async def parent_reply(request):
            nonlocal calls
            calls += 1
            return call({} if calls == 1 else {'n': 2})
        async def execute(args, context): effects.append(True); return True
        definition = Tool(name='delegate', description='Integer', parameters={'type': 'object', 'required': ['n'], 'properties': {'n': {'type': 'integer'}}}, execute=execute, with_context=True)
        parent = Agent(model='parent', objective=objective, provider=Provider(parent_reply), tools=[definition])
        with self.assertRaises(ObjectiveLimit): await parent.run('Work')
        self.assertEqual(calls, 2); self.assertEqual(effects, [])
        self.assertEqual(objective.inspect()['tool_calls'], 1)
        parent.close()

    async def test_unmetered_and_lost_replies_retain_reservation(self):
        for fail in [False, True]:
            objective = Objective()
            calls = []
            async def complete(request):
                calls.append(True)
                if fail: raise RuntimeError('response lost')
                return {'content': 'unmetered'}
            agent = Agent(model='test', objective=objective, provider=Provider(complete))
            with self.assertRaisesRegex(RuntimeError, 'response lost' if fail else 'OBJECTIVE_LIMIT'):
                await agent.run('Work')
            self.assertEqual(len(calls), 1)
            self.assertEqual(objective.inspect()['reservations'], {'1': 2048})
            self.assertEqual(objective.inspect()['stopped'], None if fail else 'usage_unavailable')
            agent.close()

    async def test_objective_cancel_propagates_and_preserves_child_reservation(self):
        objective = Objective()
        entered = asyncio.Event()
        stopped = []
        async def child_reply(request):
            entered.set()
            try: await asyncio.Future()
            finally: stopped.append(True)
        async def parent_reply(request): return call()
        async def delegate(args, context): return await context.delegate(child, 'Work')
        child = Agent(model='child', provider=Provider(child_reply))
        parent = Agent(model='parent', objective=objective, provider=Provider(parent_reply), tools=[tool(delegate)])
        pending = asyncio.create_task(parent.run('Work'))
        await entered.wait(); objective.cancel()
        with self.assertRaises(asyncio.CancelledError): await pending
        self.assertEqual(stopped, [True])
        self.assertEqual(objective.inspect()['model_calls'], 2)
        self.assertEqual(len(objective.inspect()['reservations']), 1)
        with self.assertRaises(ObjectiveLimit): await parent.run('Again')
        parent.close(); child.close()

    async def test_unawaited_child_is_joined_and_scope_cannot_be_reused(self):
        objective = Objective()
        entered, gate = asyncio.Event(), asyncio.Event()
        parent_calls = 0
        captured = []
        async def child_reply(request):
            entered.set(); await gate.wait(); return reply()
        async def parent_reply(request):
            nonlocal parent_calls
            parent_calls += 1
            return call() if parent_calls == 1 else reply()
        async def delegate(args, context):
            captured.append(context)
            context.delegate(child, 'Work')
            return 'started'
        child = Agent(model='child', provider=Provider(child_reply))
        parent = Agent(model='parent', objective=objective, provider=Provider(parent_reply), tools=[tool(delegate)])
        pending = asyncio.create_task(parent.run('Work'))
        await entered.wait(); self.assertEqual(parent_calls, 1); gate.set()
        self.assertEqual((await pending)['status'], 'completed')
        with self.assertRaisesRegex(RuntimeError, 'CLOSED'): captured[0].delegate(child, 'Too late')
        parent.close(); child.close()

    async def test_parent_failure_cancels_and_joins_child_without_continuation(self):
        objective = Objective()
        entered = asyncio.Event()
        stopped = []
        async def child_reply(request):
            entered.set()
            try: await asyncio.Future()
            finally: stopped.append(True)
        async def parent_reply(request): return call()
        async def delegate(args, context):
            context.delegate(child, 'Work')
            await entered.wait()
            raise RuntimeError('parent failed')
        child = Agent(model='child', provider=Provider(child_reply))
        parent = Agent(model='parent', objective=objective, provider=Provider(parent_reply), tools=[tool(delegate)])
        with self.assertRaises(ToolOperationUncertain): await parent.run('Work')
        self.assertEqual(stopped, [True]); self.assertEqual(objective.inspect()['model_calls'], 2)
        parent.close(); child.close()

    async def test_persistent_objective_is_rejected_before_loading_store(self):
        with self.assertRaisesRegex(TypeError, 'cannot use SessionStore'):
            Agent(model='test', objective=Objective(), store=object())

    async def test_parent_failure_before_child_provider_starts_cancels_queued_work(self):
        objective = Objective()
        child_calls = []
        async def child_reply(request): child_calls.append(True); return reply()
        async def parent_reply(request): return call()
        async def delegate(args, context):
            context.delegate(child, 'Work')
            raise RuntimeError('parent failed immediately')
        child = Agent(model='child', provider=Provider(child_reply))
        parent = Agent(model='parent', objective=objective, provider=Provider(parent_reply), tools=[tool(delegate)])
        with self.assertRaises(ToolOperationUncertain): await parent.run('Work')
        self.assertEqual(child_calls, [])
        # A child may reserve before the callback exception reaches its awaiting driver.
        # Reservation accounting is intentionally conservative, even without host dispatch.
        view = objective.inspect()
        self.assertIn(view['model_calls'], (1, 2))
        self.assertEqual(len(view['reservations']), view['model_calls'] - 1)
        parent.close(); child.close()

    async def test_streaming_uses_shared_reservation_and_no_auxiliary_call(self):
        objective = Objective({'max_output_tokens': 7})
        class Streaming:
            async def stream(self, request):
                self_cap.append(request['max_output_tokens'])
                yield {'type': 'text_delta', 'text': 'done'}
                yield {'type': 'reply', 'reply': reply()}
        self_cap = []
        agent = Agent(model='test', objective=objective, provider=Streaming())
        events = [event async for event in agent.stream('Work')]
        self.assertEqual(self_cap, [7]); self.assertEqual(events[-1]['result']['content'], 'done')
        self.assertEqual(objective.inspect()['model_calls'], 1)
        self.assertEqual(objective.inspect()['reservations'], {})
        agent.close()

    async def test_objective_deadline_cancels_active_provider_and_never_resets(self):
        objective = Objective({'max_run_ms': 300})
        started = []
        async def complete(request):
            started.append(request['deadline_unix_ms'])
            await asyncio.Future()
        agent = Agent(model='test', objective=objective, provider=Provider(complete))
        with self.assertRaisesRegex((RuntimeError, TimeoutError), 'DEADLINE|OBJECTIVE_LIMIT'):
            await agent.run('Work')
        self.assertEqual(len(started), 1)
        with self.assertRaises(ObjectiveLimit): await agent.run('Again')
        self.assertEqual(objective.inspect()['model_calls'], 1)
        agent.close()
