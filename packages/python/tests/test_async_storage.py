import asyncio
from contextlib import asynccontextmanager
import json
import tempfile
import threading
import unittest
from unittest.mock import patch
from hive_sdk import Agent, Objective, SessionStore, Tool, ToolOperationUncertain, SummaryUncertain, VerificationUncertain
from hive_sdk import _native
from test_agent import Scripted

REPLY = {'content': 'Known 🦀', 'usage': {'input_tokens': 3, 'output_tokens': 2}}
TOOL_REPLY = {**REPLY, 'content': None, 'tool_calls': [{'id': 'c', 'name': 'read', 'arguments': {}}]}
def config(store): return dict(model='test', store=store, agent_id='a', session_id='s', provider=Scripted(lambda request: REPLY))


class Gate:
    def __init__(self, handle, method, matches=lambda *args: True):
        self.handle = handle; self.method = method; self.matches = matches
        self.entered = threading.Event(); self.released = threading.Event(); self.used = False
    def __getattr__(self, name):
        original = getattr(self.handle, name)
        if name != self.method: return original
        def execute(*args):
            if not self.used and self.matches(*args):
                self.used = True; self.entered.set(); self.released.wait()
            return original(*args)
        return execute
    async def ready(self):
        if not await asyncio.to_thread(self.entered.wait, 5): raise TimeoutError('worker did not reach gate')


@asynccontextmanager
async def fixture():
    with tempfile.TemporaryDirectory() as directory:
        store = await SessionStore.open(directory)
        try: yield store
        finally: await store.close_async()


class AsyncStorageTests(unittest.IsolatedAsyncioTestCase):
    async def test_factories_tenants_inspection_and_context_plans_preserve_cas(self):
        async with fixture() as store:
            agent = await Agent.open(**config(store)); await agent.run('History'); stale = await Agent.open(**config(store))
            plan = {'summary': {'id': 's', 'source': agent.capture_context(0, 2), 'content': 'Known'}, 'retrieved': []}
            await agent.set_context_plan_async(plan); self.assertEqual(agent.context_plan, plan)
            with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFLICT'): await stale.set_context_plan_async(None)
            self.assertEqual((await Agent.open(**config(store))).context_plan, plan)
            self.assertEqual(len((await store.inspect_async(agent_id='a', session_id='s'))['run']['state']['messages']), 2)
            tenant = await store.for_tenant_async('other')
            try: self.assertIsNone(await tenant.inspect_async(agent_id='a', session_id='s'))
            finally: await tenant.close_async()
            objective = await Objective.open({}, store=store, objective_id='root'); self.assertEqual((await objective.inspect_async())['model_calls'], 0)
            memory = await Agent.open(model='test', provider=Scripted(lambda request: REPLY)); await memory.run('Memory'); await memory.set_context_plan_async(None)

    async def test_async_model_resolution_fences_ids_and_resumes_without_model(self):
        async with fixture() as store:
            calls = []
            def fail(request): calls.append(True); raise ValueError('lost')
            options = dict(config(store), provider=Scripted(fail)); agent = await Agent.open(**options)
            with self.assertRaisesRegex(ValueError, 'lost'): await agent.run('Work')
            snapshot = await store.inspect_async(agent_id='a', session_id='s')
            resolution = dict(agent_id='a', session_id='s', revision=snapshot['revision'], execution_id=snapshot['run']['in_flight']['execution_id'], reply=REPLY)
            with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFLICT'): await store.resolve_model_async(**dict(resolution, execution_id='stale'))
            await store.resolve_model_async(**resolution)
            self.assertEqual((await (await Agent.open(**options, resume=True)).resume())['content'], REPLY['content']); self.assertEqual(len(calls), 1)

    async def test_async_tool_resolution_and_recovery_do_not_replay(self):
        async with fixture() as store:
            models, effects = [], []
            def complete(request): models.append(True); return TOOL_REPLY if len(models) == 1 else REPLY
            def uncertain(args): effects.append(True); raise ToolOperationUncertain()
            options = dict(config(store), provider=Scripted(complete), tools=[Tool('read', 'Read', {'type': 'object'}, uncertain)])
            agent = await Agent.open(**options)
            with self.assertRaises(ToolOperationUncertain): await agent.run('Work')
            snapshot = await store.inspect_async(agent_id='a', session_id='s')
            await store.resolve_tool_async(agent_id='a', session_id='s', revision=snapshot['revision'], execution_id=snapshot['run']['in_flight']['execution_id'], result=5)
            await (await Agent.open(**options, resume=True)).resume(); self.assertEqual(len(effects), 1); self.assertEqual(len(models), 2)
            def fail(request): raise ValueError('lost')
            failed = dict(config(store), session_id='abandoned', provider=Scripted(fail))
            with self.assertRaisesRegex(ValueError, 'lost'): await (await Agent.open(**failed)).run('Work')
            await store.recover_async(agent_id='a', session_id='abandoned'); self.assertEqual((await Agent.open(**failed)).messages, [])

    async def test_pending_maintenance_blocks_loads_runs_and_sync_calls(self):
        async with fixture() as store:
            def fail(request): raise ValueError('lost')
            options = dict(config(store), provider=Scripted(fail)); agent = await Agent.open(**options)
            with self.assertRaisesRegex(ValueError, 'lost'): await agent.run('Work')
            gate = Gate(store._native, 'dispatch', lambda command: json.loads(command)['op'] == 'recover'); store._native = gate
            pending = asyncio.create_task(store.recover_async(agent_id='a', session_id='s'))
            try:
                await gate.ready()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): Agent(**options)
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): agent.set_context_plan(None)
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): await Agent.open(**options)
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): await agent.run('Blocked')
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.inspect(agent_id='a', session_id='s')
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): await store.close_async()
                gate.released.set(); await pending
                self.assertEqual((await store.inspect_async(agent_id='a', session_id='s'))['status'], 'ready')
            finally: gate.released.set(); await asyncio.gather(pending, return_exceptions=True)

    async def test_repeated_inspection_cancellation_joins_worker_before_releasing_store(self):
        async with fixture() as store:
            gate = Gate(store._native, 'dispatch'); store._native = gate
            pending = asyncio.create_task(store.inspect_async(agent_id='a', session_id='s'))
            try:
                await gate.ready(); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                self.assertFalse(pending.done())
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): await store.recover_async(agent_id='a', session_id='s')
                gate.released.set()
                with self.assertRaises(asyncio.CancelledError): await pending
            finally: gate.released.set(); await asyncio.gather(pending, return_exceptions=True)

    async def test_late_open_after_cancellation_closes_resource_in_worker(self):
        original = _native.NativeSessionStore; entered = threading.Event(); released = threading.Event(); closed = threading.Event(); close_threads = []
        class Opened:
            def __init__(self, handle): self.handle = handle
            def close(self):
                self.handle.close(); close_threads.append(threading.get_ident()); closed.set()
        def create(path, tenant):
            handle = original(path, tenant); entered.set(); released.wait(); return Opened(handle)
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(_native, 'NativeSessionStore', create):
                pending = asyncio.create_task(SessionStore.open(directory))
                try:
                    self.assertTrue(await asyncio.to_thread(entered.wait, 5)); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                    self.assertFalse(pending.done()); released.set()
                    with self.assertRaises(asyncio.CancelledError): await pending
                    self.assertTrue(closed.is_set()); self.assertNotEqual(close_threads[0], threading.get_ident())
                finally: released.set(); await asyncio.gather(pending, return_exceptions=True)
            reopened = await SessionStore.open(directory); await reopened.close_async()

    async def test_cancelled_context_update_installs_known_result_before_releasing_guards(self):
        async with fixture() as store:
            agent = await Agent.open(**config(store)); await agent.run('History')
            plan = {'summary': {'id': 's', 'source': agent.capture_context(0, 2), 'content': 'Known'}, 'retrieved': []}
            gate = Gate(agent._durable, 'dispatch', lambda command: json.loads(command)['op'] == 'set_context_plan'); agent._durable = gate
            pending = asyncio.create_task(agent.set_context_plan_async(plan))
            try:
                await gate.ready(); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                self.assertFalse(pending.done()); self.assertFalse(agent.cancel())
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): agent.close()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): await agent.run('Blocked')
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                gate.released.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                self.assertEqual(agent.context_plan, plan); self.assertEqual((await Agent.open(**config(store))).context_plan, plan)
            finally: gate.released.set(); await asyncio.gather(pending, return_exceptions=True)

    async def test_objective_async_reconciliation_preserves_errors_ids_and_sources(self):
        async with fixture() as store:
            objective = await Objective.open({}, store=store, objective_id='root')
            def fail(value, context): raise ValueError('unknown')
            with self.assertRaises(VerificationUncertain): await objective.verify('v', {}, fail)
            execution_id = (await objective.inspect_async())['verifications']['v']['execution_id']
            with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFLICT'): await objective.resolve_verification_async(key='v', execution_id='stale', verdict={'passed': True})
            result = await objective.resolve_verification_async(key='v', execution_id=execution_id, verdict={'passed': True}); self.assertTrue(result['verdict']['passed'])
            agent = await Agent.open(**config(store)); await agent.run('History'); source = agent.capture_context(0, 2)
            with self.assertRaises(SummaryUncertain): await objective.summarize('s', {'source': source, 'messages': agent.read_context(source)}, fail)
            summary_id = (await objective.inspect_async())['summaries']['s']['execution_id']
            self.assertEqual((await objective.resolve_summary_async(key='s', execution_id=summary_id, content='Known'))['summary']['content'], 'Known')
            local = await Objective.open({})
            with self.assertRaises(VerificationUncertain): await local.verify('v', {}, fail)
            local_id = (await local.inspect_async())['verifications']['v']['execution_id']
            self.assertTrue((await local.resolve_verification_async(key='v', execution_id=local_id, verdict={'passed': True}))['verdict']['passed'])
            await local.cancel_async(); self.assertEqual((await local.inspect_async())['stopped'], 'cancelled')

    async def test_cancelled_objective_cancel_still_signals_callbacks_after_known_write(self):
        async with fixture() as store:
            objective = await Objective.open({}, store=store, objective_id='root'); entered = asyncio.Event()
            class Provider:
                async def complete(self, request): entered.set(); await asyncio.Future()
            agent = await Agent.open(**dict(config(store), objective=objective, provider=Provider()))
            run = asyncio.create_task(agent.run('Work')); await entered.wait()
            gate = Gate(store._native, 'dispatch', lambda command: json.loads(command).get('command', {}).get('op') == 'cancel'); store._native = gate
            pending = asyncio.create_task(objective.cancel_async())
            try:
                await gate.ready(); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                self.assertFalse(pending.done()); gate.released.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                with self.assertRaises(asyncio.CancelledError): await run
                self.assertEqual((await objective.inspect_async())['stopped'], 'cancelled')
            finally:
                gate.released.set(); agent.cancel(); await asyncio.gather(pending, run, return_exceptions=True)

    async def test_public_storage_apis_execute_native_calls_off_the_event_loop_thread(self):
        main_thread = threading.get_ident(); original = _native.NativeSessionStore; calls = []
        class OffThread:
            def __init__(self, handle): self.handle = handle
            def __getattr__(self, name):
                callback = getattr(self.handle, name)
                def execute(*args):
                    if threading.get_ident() == main_thread: raise AssertionError(f'synchronous native entry point: {name}')
                    calls.append(name); result = callback(*args)
                    return OffThread(result) if name in ('open_agent', 'for_tenant') else result
                return execute
        def create(path, tenant):
            self.assertNotEqual(threading.get_ident(), main_thread)
            return OffThread(original(path, tenant))
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(_native, 'NativeSessionStore', create):
                store = await SessionStore.open(directory)
                try:
                    agent = await Agent.open(**config(store)); await agent.set_context_plan_async(None)
                    objective = await Objective.open({}, store=store, objective_id='root'); await objective.inspect_async()
                    await store.inspect_async(agent_id='a', session_id='s')
                    tenant = await store.for_tenant_async('other'); await tenant.close_async()
                finally: await store.close_async()
        self.assertIn('open_agent', calls); self.assertIn('dispatch', calls); self.assertIn('for_tenant', calls); self.assertIn('close', calls)

    async def test_cancelled_agent_load_joins_worker_and_retains_store(self):
        async with fixture() as store:
            gate = Gate(store._native, 'open_agent'); store._native = gate
            pending = asyncio.create_task(Agent.open(**config(store)))
            try:
                await gate.ready(); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                self.assertFalse(pending.done())
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                gate.released.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                self.assertIsNone(await store.inspect_async(agent_id='a', session_id='s'))
                self.assertEqual((await Agent.open(**config(store))).messages, [])
            finally: gate.released.set(); await asyncio.gather(pending, return_exceptions=True)

    async def test_pending_close_excludes_operations_until_worker_finishes(self):
        async with fixture() as store:
            agent = await Agent.open(**config(store)); gate = Gate(store._native, 'close'); store._native = gate
            pending = asyncio.create_task(store.close_async())
            try:
                await gate.ready(); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                self.assertFalse(pending.done())
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): await Agent.open(**config(store))
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.inspect(agent_id='a', session_id='s')
                gate.released.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                with self.assertRaisesRegex(RuntimeError, 'CLOSED'): await store.inspect_async(agent_id='a', session_id='s')
                self.assertEqual(agent.messages, [])
            finally: gate.released.set(); await asyncio.gather(pending, return_exceptions=True)
