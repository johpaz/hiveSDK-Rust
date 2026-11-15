import asyncio
import json
import tempfile
import threading
import time
import unittest
from contextlib import asynccontextmanager
from hive_sdk import Agent, Objective, SessionStore, Tool, SummaryUncertain, VerificationUncertain, TriggerUncertain
from test_agent import Scripted

REPLY = {'content': 'Known 🦀', 'usage': {'input_tokens': 3, 'output_tokens': 2}}
POLICY = {'max_model_calls': 8, 'max_output_tokens': 8, 'max_total_output_tokens': 64, 'max_depth': 2}


class Gate:
    def __init__(self, handle, operation, *, after=False, nth=1):
        self.handle = handle; self.operation = operation; self.after = after; self.nth = nth; self.seen = 0
        self.entered = threading.Event(); self.released = threading.Event()
    def __getattr__(self, name):
        original = getattr(self.handle, name)
        if name != 'dispatch': return original
        def execute(command):
            if json.loads(command).get('command', {}).get('op') == self.operation:
                self.seen += 1
                if self.seen == self.nth:
                    value = original(command) if self.after else None
                    self.entered.set(); self.released.wait()
                    return value if self.after else original(command)
            return original(command)
        return execute
    async def ready(self):
        if not await asyncio.to_thread(self.entered.wait, 5): raise TimeoutError('worker did not reach gate')


@asynccontextmanager
async def fixture():
    with tempfile.TemporaryDirectory() as directory:
        store = await SessionStore.open(directory)
        try: yield store
        finally: await store.close_async()


async def source_input():
    source = await Agent.open(model='source', provider=Scripted(lambda request: REPLY)); await source.run('History')
    span = source.capture_context(0, 2); return {'source': span, 'messages': source.read_context(span)}


async def action(objective, kind, callback, value):
    if kind == 'verify':
        async def run(cancel_event=None): return await objective.verify('k', {}, callback, cancel_event=cancel_event)
        async def resolve(): return await objective.resolve_verification_async(key='k', execution_id='stale', verdict={'passed': True})
        return run, resolve, VerificationUncertain, None
    if kind == 'summary':
        async def run(cancel_event=None): return await objective.summarize('k', value, callback, cancel_event=cancel_event)
        async def resolve(): return await objective.resolve_summary_async(key='k', execution_id='stale', content='Known')
        return run, resolve, SummaryUncertain, None
    trigger = await objective.trigger_async('event', {'kind': 'event'}, callback); await trigger.enable_async()
    async def run(cancel_event=None): return await trigger.emit('k', {}, cancel_event=cancel_event)
    async def resolve(): return await trigger.resolve_async(occurrence_key='k', execution_id='stale', result='Known')
    return run, resolve, TriggerUncertain, trigger


class ObjectiveRuntimeTests(unittest.IsolatedAsyncioTestCase):
    async def test_durable_flows_use_workers_for_all_objective_control(self):
        async with fixture() as store:
            objective = await Objective.open(POLICY, store=store, objective_id='root')
            source = await Agent.open(model='source', store=store, agent_id='source', session_id='s', provider=Scripted(lambda request: REPLY)); await source.run('History')
            worker = await Agent.open(model='worker', store=store, objective=objective, agent_id='worker', session_id='s', provider=Scripted(lambda request: REPLY)); calls = []
            async def delegate(args, ctx): return (await ctx.delegate(worker, 'Work'))['content']
            def parent_reply(request):
                calls.append(True)
                return {**REPLY, 'content': None, 'tool_calls': [{'id': 'c', 'name': 'delegate', 'arguments': {}}]} if len(calls) == 1 else REPLY
            parent = await Agent.open(model='parent', store=store, objective=objective, agent_id='parent', session_id='s', provider=Scripted(parent_reply), tools=[Tool('delegate', 'Delegate', {'type': 'object'}, delegate, with_context=True)])
            main_thread = threading.get_ident(); native = store._native
            class OffThread:
                def __getattr__(self, name):
                    original = getattr(native, name)
                    if name != 'dispatch': return original
                    def execute(command):
                        if threading.get_ident() == main_thread: raise AssertionError('synchronous store entry point')
                        return original(command)
                    return execute
            store._native = OffThread()
            def forbidden(command): raise AssertionError('synchronous objective control')
            objective._dispatch = forbidden
            async def verify(value, ctx): await ctx.delegate(worker, 'Review'); return {'passed': True}
            self.assertTrue((await objective.verify('v', {}, verify, model_review={'evaluation_id': 'fixture', 'justification': 'explicit check'}))['verdict']['passed'])
            async def summarize(value, ctx): return (await ctx.delegate(worker, 'Summary'))['content']
            self.assertEqual((await source.summarize('s', source.capture_context(0, 2), summarize, objective=objective))['summary']['content'], REPLY['content'])
            self.assertEqual((await parent.run('Task'))['content'], REPLY['content'])
            event = await objective.trigger_async('event', {'kind': 'event'}, delegate); await event.enable_async()
            self.assertEqual((await event.emit('one', {}))['result'], REPLY['content']); await event.emit('one', {})
            once = await objective.trigger_async('once', {'kind': 'at', 'unix_ms': time.time_ns() // 1_000_000 - 1}, lambda value, ctx: 'done'); await once.enable_async()
            self.assertEqual(len([receipt async for receipt in once.watch()]), 1); await once.disable_async()
            self.assertEqual((await objective.inspect_async())['model_calls'], 6)

    async def test_cancelled_pending_claims_join_workers_and_never_execute_callbacks(self):
        async with fixture() as store:
            value = await source_input()
            for kind in ('verify', 'summary', 'trigger'):
                objective = await Objective.open(POLICY, store=store, objective_id=kind); calls = []
                run, resolve, uncertain, trigger = await action(objective, kind, lambda value, ctx: calls.append(True) or ({'passed': True} if kind == 'verify' else 'Known'), value)
                original = store._native; held = Gate(original, 'trigger_emit' if kind == 'trigger' else kind + '_begin'); store._native = held
                pending = asyncio.create_task(run())
                try:
                    await held.ready()
                    with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                    with self.assertRaisesRegex(RuntimeError, 'BUSY'): await resolve()
                    if trigger is not None: self.assertTrue(trigger.cancel())
                    else: pending.cancel(); await asyncio.sleep(0); pending.cancel()
                    await asyncio.sleep(0); self.assertFalse(pending.done()); held.released.set()
                    with self.assertRaises(asyncio.CancelledError): await pending
                    self.assertEqual(calls, []); self.assertEqual((await objective.inspect_async())['tool_calls'], 1)
                    with self.assertRaises(uncertain): await run()
                    self.assertEqual(calls, [])
                    state = await objective.inspect_async()
                    with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFLICT'): await resolve()
                    if trigger is not None:
                        await trigger.resolve_async(occurrence_key='k', execution_id=state['triggers']['event']['executions']['k']['execution_id'], result='Known')
                    elif kind == 'summary':
                        await objective.resolve_summary_async(key='k', execution_id=state['summaries']['k']['execution_id'], content='Known')
                    else:
                        await objective.resolve_verification_async(key='k', execution_id=state['verifications']['k']['execution_id'], verdict={'passed': True})
                    self.assertIsNotNone(await run()); self.assertEqual(calls, [])
                finally:
                    held.released.set(); await asyncio.gather(pending, return_exceptions=True); store._native = original

    async def test_cancelled_finalizations_persist_known_results_and_cached_reads_ignore_new_work_limits(self):
        async with fixture() as store:
            value = await source_input()
            for kind in ('verify', 'summary', 'trigger'):
                objective = await Objective.open(POLICY, store=store, objective_id=kind); calls = []
                run, _, _, _ = await action(objective, kind, lambda value, ctx: calls.append(True) or ({'passed': True} if kind == 'verify' else 'Known'), value)
                original = store._native; held = Gate(original, ('trigger' if kind == 'trigger' else kind) + '_finish'); store._native = held
                pending = asyncio.create_task(run())
                try:
                    await held.ready(); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                    self.assertFalse(pending.done())
                    with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                    held.released.set()
                    with self.assertRaises(asyncio.CancelledError): await pending
                    await objective.cancel_async(); self.assertIsNotNone(await run()); self.assertEqual(len(calls), 1); self.assertEqual((await objective.inspect_async())['tool_calls'], 1)
                finally:
                    held.released.set(); await asyncio.gather(pending, return_exceptions=True); store._native = original

    async def test_cancelling_agent_during_root_check_dispatches_nothing_and_releases_scope(self):
        async with fixture() as store:
            objective = await Objective.open(POLICY, store=store, objective_id='root'); models = []
            agent = await Agent.open(model='test', store=store, objective=objective, agent_id='a', session_id='s', provider=Scripted(lambda request: models.append(True) or REPLY))
            held = Gate(store._native, 'check'); store._native = held; pending = asyncio.create_task(agent.run('Work'))
            try:
                await held.ready(); await objective.cancel_async()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                held.released.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                self.assertEqual(models, []); self.assertIsNone(await store.inspect_async(agent_id='a', session_id='s'))
            finally: held.released.set(); await asyncio.gather(pending, return_exceptions=True)

    async def test_disable_known_after_cancellation_cannot_leave_watcher_on_stale_inspection(self):
        async with fixture() as store:
            objective = await Objective.open(POLICY, store=store, objective_id='root')
            trigger = await objective.trigger_async('future', {'kind': 'at', 'unix_ms': time.time_ns() // 1_000_000 + 60000}, lambda value, ctx: self.fail('not due')); await trigger.enable_async()
            read = Gate(store._native, 'trigger_inspect', after=True, nth=3); store._native = read
            iterator = trigger.watch(); pending = asyncio.create_task(iterator.__anext__()); disable = None; write = None
            try:
                await read.ready()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): await trigger.watch().__anext__()
                write = Gate(read, 'trigger_enable'); store._native = write
                disable = asyncio.create_task(trigger.disable_async()); await write.ready(); disable.cancel(); await asyncio.sleep(0); disable.cancel(); await asyncio.sleep(0)
                self.assertFalse(disable.done()); write.released.set()
                with self.assertRaises(asyncio.CancelledError): await disable
                read.released.set()
                with self.assertRaises(StopAsyncIteration): await asyncio.wait_for(asyncio.shield(pending), 2)
                self.assertEqual((await objective.inspect_async())['tool_calls'], 0)
            finally:
                read.released.set()
                if write is not None: write.released.set()
                trigger.cancel(); await asyncio.gather(pending, *([disable] if disable is not None else []), return_exceptions=True); await iterator.aclose()
