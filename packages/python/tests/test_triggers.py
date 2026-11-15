import asyncio
import tempfile
import time
import unittest
from hive_sdk import Agent, Objective, ObjectiveLimit, SessionStore, TriggerUncertain


def now(): return time.time_ns() // 1_000_000


class TriggerTests(unittest.IsolatedAsyncioTestCase):
    async def test_future_watcher_waits_and_deadline_stops_idle_work(self):
        objective = Objective(); due = now() + 40; calls = []
        def callback(value, context):
            self.assertGreaterEqual(now(), due); calls.append(True); return 'due'
        trigger = objective.trigger('future', {'kind': 'at', 'unix_ms': due}, callback); trigger.enable()
        receipts = [receipt async for receipt in trigger.watch()]
        self.assertEqual(receipts[0]['result'], 'due'); self.assertEqual(len(calls), 1)
        limited = Objective({'max_run_ms': 40})
        idle = limited.trigger('idle', {'kind': 'at', 'unix_ms': now() + 60_000}, lambda value, context: self.fail('no effect'))
        idle.enable()
        with self.assertRaisesRegex((RuntimeError, TimeoutError), 'DEADLINE|OBJECTIVE_LIMIT'):
            async for _ in idle.watch(): self.fail('no result')
        self.assertEqual(limited.inspect()['tool_calls'], 0); self.assertFalse(idle.cancel())

    async def test_disabled_events_and_cached_null_isolate_inputs(self):
        objective = Objective(); calls = []; contexts = []
        def callback(value, context):
            calls.append(True); contexts.append(context); value['n'] = 99
            return None
        trigger = objective.trigger('receipts', {'kind': 'event'}, callback)
        value = {'n': 1}
        self.assertIsNone(await trigger.emit('r1', value)); self.assertEqual(calls, [])
        trigger.enable(); receipt = await trigger.emit('r1', value)
        self.assertIsNone(receipt['result']); self.assertEqual(value, {'n': 1})
        self.assertEqual(sorted(contexts[0]), ['execution_id', 'objective_id', 'occurrence_key', 'trigger_id'])
        self.assertEqual(await trigger.emit('r1', value), receipt); self.assertEqual(len(calls), 1)
        with self.assertRaisesRegex(RuntimeError, 'already bound'): await trigger.emit('r1', {'n': 2})
        with self.assertRaisesRegex(RuntimeError, 'CLOSED'): contexts[0].delegate(None, 'late')
        with self.assertRaisesRegex(ValueError, 'calendar'): await trigger.tick()
        with self.assertRaisesRegex(ValueError, 'Event triggers'): await anext(trigger.watch())
        self.assertEqual(objective.inspect()['tool_calls'], 1)

    async def test_delayed_interval_coalesces_without_burst(self):
        objective = Objective(); inputs = []
        def callback(value, context): inputs.append(value); return 'done'
        trigger = objective.trigger('interval', {'kind': 'interval', 'every_ms': 60_000, 'anchor_unix_ms': now() - 600_100}, callback)
        trigger.enable(); self.assertEqual((await trigger.tick())['result'], 'done')
        self.assertGreaterEqual(inputs[0]['skipped_occurrences'], 9)
        self.assertGreater(trigger.inspect()['next_due_unix_ms'], now())
        self.assertIsNone(await trigger.tick()); self.assertEqual(objective.inspect()['tool_calls'], 1)

    async def test_one_shot_watcher_is_explicit_and_finishes_at_single_allowance(self):
        objective = Objective({'max_tool_calls': 1}); calls = []
        def callback(value, context): calls.append(True); return 'done'
        trigger = objective.trigger('once', {'kind': 'at', 'unix_ms': now() - 1}, callback)
        self.assertEqual([receipt async for receipt in trigger.watch()], [])
        trigger.enable(); watcher = trigger.watch(); self.assertEqual(calls, [])
        receipts = [receipt async for receipt in watcher]
        self.assertEqual(len(receipts), 1); self.assertEqual(receipts[0]['result'], 'done')
        self.assertEqual([receipt async for receipt in trigger.watch()], [])
        self.assertEqual(len(calls), 1); self.assertEqual(objective.inspect()['model_calls'], 0)

    async def test_idle_watcher_disable_and_cancel_release_store(self):
        for cancel in [False, True]:
            with tempfile.TemporaryDirectory() as directory:
                store = SessionStore(directory)
                try:
                    objective = Objective(store=store, objective_id='root')
                    trigger = objective.trigger('future', {'kind': 'at', 'unix_ms': now() + 60_000}, lambda value, context: self.fail('no effect'))
                    trigger.enable(); pending = asyncio.create_task(anext(trigger.watch()))
                    await asyncio.sleep(0)
                    with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                    with self.assertRaisesRegex(RuntimeError, 'BUSY'): await anext(trigger.watch())
                    if cancel:
                        self.assertTrue(trigger.cancel())
                        with self.assertRaises(asyncio.CancelledError): await asyncio.wait_for(pending, 2)
                    else:
                        trigger.disable()
                        with self.assertRaises(StopAsyncIteration): await asyncio.wait_for(pending, 2)
                    self.assertFalse(trigger.cancel()); self.assertEqual(objective.inspect()['tool_calls'], 0)
                finally: store.close()

    async def test_unknown_result_requires_fenced_resolution_before_more_events(self):
        objective = Objective(); calls = []
        def callback(value, context): calls.append(True); raise RuntimeError('unknown effect')
        trigger = objective.trigger('event', {'kind': 'event'}, callback); trigger.enable()
        with self.assertRaises(TriggerUncertain): await trigger.emit('one', {})
        with self.assertRaises(TriggerUncertain): await trigger.emit('two', {})
        execution_id = trigger.inspect()['executions']['one']['execution_id']
        with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFLICT'):
            trigger.resolve(occurrence_key='one', execution_id='stale', result=None)
        receipt = trigger.resolve(occurrence_key='one', execution_id=execution_id, result=None)
        self.assertEqual(await trigger.emit('one', {}), receipt)
        self.assertEqual(len(calls), 1); self.assertEqual(objective.inspect()['tool_calls'], 1)

    async def test_delegation_shares_root_budget_between_occurrences(self):
        objective = Objective({'max_model_calls': 1, 'max_output_tokens': 10, 'max_total_output_tokens': 10}); calls = []
        class Provider:
            async def complete(inner, request):
                calls.append(True); self.assertEqual(request['max_output_tokens'], 10)
                return {'content': 'done', 'usage': {'input_tokens': 2, 'output_tokens': 3}}
        agent = Agent(model='fixture', provider=Provider())
        async def callback(value, context): return await context.delegate(agent, 'Work')
        try:
            trigger = objective.trigger('work', {'kind': 'event'}, callback); trigger.enable()
            self.assertEqual((await trigger.emit('one', {}))['result']['content'], 'done')
            with self.assertRaises(ObjectiveLimit): await trigger.emit('two', {})
            self.assertEqual(len(calls), 1); self.assertEqual(objective.inspect()['output_tokens'], 3)
        finally: agent.close()

    async def test_late_known_result_commits_before_cancel_error(self):
        objective = Objective(); entered = asyncio.Event()
        async def callback(value, context):
            entered.set()
            try: await asyncio.Future()
            except asyncio.CancelledError: return 'confirmed'
        trigger = objective.trigger('event', {'kind': 'event'}, callback); trigger.enable()
        pending = asyncio.create_task(trigger.emit('one', {})); await entered.wait()
        with self.assertRaisesRegex(RuntimeError, 'BUSY'): trigger.resolve(occurrence_key='one', execution_id='x', result=None)
        self.assertTrue(trigger.cancel())
        with self.assertRaises(asyncio.CancelledError): await pending
        self.assertEqual((await trigger.emit('one', {}))['result'], 'confirmed')
        self.assertEqual(objective.inspect()['tool_calls'], 1)

    async def test_reopen_preserves_enabled_calendar(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                objective = Objective(store=store, objective_id='root')
                trigger = objective.trigger('interval', {'kind': 'interval', 'every_ms': 60_000}, lambda value, context: 'work')
                trigger.enable(); before = trigger.inspect(); store.close()
                store = SessionStore(directory); objective = Objective(store=store, objective_id='root')
                trigger = objective.trigger('interval', {'kind': 'interval', 'every_ms': 60_000}, lambda value, context: self.fail('not due'))
                self.assertEqual(trigger.inspect(), before); self.assertIsNone(await trigger.tick())
                with self.assertRaisesRegex(RuntimeError, 'schedule'):
                    objective.trigger('interval', {'kind': 'interval', 'every_ms': 1}, lambda value, context: None)
            finally: store.close()
