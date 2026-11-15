import asyncio
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from hive_sdk import Agent, Objective, ObjectiveLimit, SessionStore, SummaryUncertain, Tool

POLICY = {'max_model_calls': 3, 'max_output_tokens': 10, 'max_total_output_tokens': 30}


class Provider:
    def __init__(self, complete): self.complete = complete


async def subject(store=None):
    async def complete(request): return {'content': 'R7 viernes'}
    config = dict(store=store, agent_id='subject', session_id='s') if store else {}
    agent = Agent(model='source', provider=Provider(complete), **config)
    if not agent.messages: await agent.run('Pedido R7')
    return agent


class SummaryTests(unittest.IsolatedAsyncioTestCase):
    async def test_finishing_summary_does_not_cancel_completed_child_reused_elsewhere(self):
        agent = await subject(); objective = Objective(POLICY); calls = []; aborted = []
        summary_ready, summary_gate, other_started, other_gate = (asyncio.Event() for _ in range(4))
        async def complete(request):
            calls.append(True)
            if len(calls) == 1: return {'content': 'fact', 'usage': {'input_tokens': 2, 'output_tokens': 1}}
            other_started.set()
            try: await other_gate.wait()
            except asyncio.CancelledError: aborted.append(True); raise
            return {'content': 'other result'}
        worker = Agent(model='worker', provider=Provider(complete))
        async def summarize(value, context):
            result = await context.delegate(worker, 'Fact'); summary_ready.set(); await summary_gate.wait(); return result['content']
        pending = asyncio.create_task(agent.summarize('s', agent.capture_context(0, 2), summarize, objective=objective)); other = None
        try:
            await summary_ready.wait(); other = asyncio.create_task(worker.run('Other task')); await other_started.wait()
            summary_gate.set(); await pending; self.assertEqual(aborted, [])
            other_gate.set(); self.assertEqual((await other)['content'], 'other result')
        finally:
            summary_gate.set(); other_gate.set(); await asyncio.gather(pending, *([other] if other else []), return_exceptions=True); worker.close(); agent.close()

    async def test_busy_delegate_fails_without_cancelling_existing_work(self):
        agent = await subject(); objective = Objective(POLICY); entered, gate = asyncio.Event(), asyncio.Event(); aborted = []
        async def complete(request):
            entered.set()
            try: await gate.wait()
            except asyncio.CancelledError: aborted.append(True); raise
            return {'content': 'other result'}
        worker = Agent(model='worker', provider=Provider(complete)); other = asyncio.create_task(worker.run('Other work'))
        async def summarize(value, context):
            with self.assertRaisesRegex(RuntimeError, 'BUSY'): context.delegate(worker, 'Busy')
            return 'R7 viernes'
        try:
            await entered.wait(); result = await agent.summarize('s', agent.capture_context(0, 2), summarize, objective=objective)
            self.assertEqual(result['cost']['model_calls'], 0); self.assertEqual(aborted, []); gate.set(); await other
        finally: gate.set(); await asyncio.gather(other, return_exceptions=True); worker.close(); agent.close()

    async def test_explicit_generation_shares_budget_caches_and_installs_only_on_request(self):
        for durable in [False, True]:
            with tempfile.TemporaryDirectory() as directory:
                store = SessionStore(directory) if durable else None
                objective = Objective(POLICY, store=store, objective_id='root' if store else None)
                agent = await subject(store); calls = []; contexts = []
                async def complete(request):
                    calls.append(True); self.assertEqual(request['max_output_tokens'], 10)
                    return {'content': 'R7 viernes', 'usage': {'input_tokens': 2, 'output_tokens': 3}}
                worker = Agent(model='summary', provider=Provider(complete), **(dict(store=store, objective=objective, agent_id='worker', session_id='s') if store else {}))
                async def summarize(value, context):
                    contexts.append(context); return (await context.delegate(worker, json.dumps(value['messages'])))['content']
                try:
                    source = agent.capture_context(0, 2); before = agent.messages
                    result = await agent.summarize('s', source, summarize, objective=objective)
                    self.assertEqual((result['cost']['model_calls'], result['cost']['tool_calls'], result['cost']['input_tokens'], result['cost']['output_tokens']), (1, 1, 2, 3))
                    self.assertEqual(result['cost']['pending_model_calls'], 0); self.assertEqual(result['cost']['reserved_output_tokens'], 0)
                    self.assertEqual(agent.messages, before); self.assertIsNone(agent.context_plan)
                    self.assertEqual(sorted(contexts[0]), ['execution_id', 'objective_id', 'summary_key'])
                    with self.assertRaisesRegex(RuntimeError, 'CLOSED'): contexts[0].delegate(worker, 'Late')
                    self.assertEqual(await agent.summarize('s', source, lambda value, context: self.fail('no replay'), objective=objective), result)
                    self.assertEqual(len(calls), 1); agent.set_context_plan({'summary': result['summary']})
                    self.assertEqual(agent.context_plan['summary']['id'], 's')
                finally:
                    worker.close(); agent.close()
                    if store: store.close()

    async def test_programmatic_callback_and_fenced_resolution_of_uncertain_text(self):
        agent = await subject(); objective = Objective(POLICY); source = agent.capture_context(0, 2); calls = []
        def pure(value, context): value['messages'][0]['content'] = 'changed'; return 'R7 viernes'
        def bad(value, context): calls.append(True); return {}
        try:
            result = await agent.summarize('pure', source, pure, objective=objective)
            self.assertEqual(result['cost']['model_calls'], 0); self.assertEqual(agent.messages[0]['content'], 'Pedido R7')
            with self.assertRaises(SummaryUncertain): await agent.summarize('bad', source, bad, objective=objective)
            with self.assertRaises(SummaryUncertain): await agent.summarize('bad', source, lambda value, context: self.fail('no replay'), objective=objective)
            execution_id = objective.inspect()['summaries']['bad']['execution_id']
            with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFLICT'): objective.resolve_summary(key='bad', execution_id='stale', content='fact')
            known = objective.resolve_summary(key='bad', execution_id=execution_id, content='R7 viernes')
            self.assertEqual(await agent.summarize('bad', source, lambda value, context: self.fail('no replay'), objective=objective), known)
            self.assertEqual(len(calls), 1); self.assertEqual(objective.inspect()['tool_calls'], 2)
        finally: agent.close()

    async def test_concurrent_summaries_exclude_unrelated_worker_usage(self):
        agent = await subject(); objective = Objective(POLICY); source = agent.capture_context(0, 2)
        value = {'source': source, 'messages': agent.read_context(source)}; arrivals = []; gate = asyncio.Event()
        def make_worker(input_tokens, output_tokens, root=None):
            async def complete(request):
                arrivals.append(True)
                if len(arrivals) == 3: gate.set()
                await gate.wait(); return {'content': 'fact', 'usage': {'input_tokens': input_tokens, 'output_tokens': output_tokens}}
            return Agent(model='fixture', provider=Provider(complete), objective=root)
        workers = [make_worker(3, 2), make_worker(7, 4), make_worker(1, 1, objective)]
        async def a(value, context): return (await context.delegate(workers[0], 'A'))['content']
        async def b(value, context): return (await context.delegate(workers[1], 'B'))['content']
        try:
            ra, rb, _ = await asyncio.gather(objective.summarize('a', value, a), objective.summarize('b', value, b), workers[2].run('Other work'))
            self.assertEqual((ra['cost']['input_tokens'], rb['cost']['input_tokens'], objective.inspect()['input_tokens']), (3, 7, 11))
            self.assertEqual((ra['cost']['model_calls'], rb['cost']['model_calls']), (1, 1))
        finally:
            gate.set()
            for worker in workers: worker.close()
            agent.close()

    async def test_nested_delegation_preserves_attribution_and_depth(self):
        for depth in [1, 2]:
            agent = await subject(); objective = Objective({**POLICY, 'max_depth': depth}); calls = []; worker_calls = []
            async def leaf_complete(request): calls.append(True); return {'content': 'fact', 'usage': {'input_tokens': 2, 'output_tokens': 1}}
            leaf = Agent(model='leaf', provider=Provider(leaf_complete))
            async def execute(args, context): return (await context.delegate(leaf, 'Check fact'))['content']
            async def complete(request):
                calls.append(True); worker_calls.append(True)
                if len(worker_calls) == 1:
                    return {'content': None, 'tool_calls': [{'id': 'c', 'name': 'delegate', 'arguments': {}}], 'usage': {'input_tokens': 3, 'output_tokens': 1}}
                return {'content': 'R7 viernes', 'usage': {'input_tokens': 4, 'output_tokens': 2}}
            worker = Agent(model='worker', provider=Provider(complete), tools=[Tool('delegate', 'Delegate', {'type': 'object'}, execute, with_context=True)])
            async def summarize(value, context): return (await context.delegate(worker, 'Summarize'))['content']
            try:
                if depth == 1:
                    with self.assertRaises(ObjectiveLimit): await agent.summarize('s', agent.capture_context(0, 2), summarize, objective=objective)
                    self.assertEqual(len(calls), 1)
                else:
                    result = await agent.summarize('s', agent.capture_context(0, 2), summarize, objective=objective)
                    self.assertEqual((result['cost']['model_calls'], result['cost']['tool_calls'], result['cost']['input_tokens'], result['cost']['output_tokens']), (3, 2, 9, 4))
            finally: worker.close(); leaf.close(); agent.close()

    async def test_failing_callback_cancels_and_joins_unawaited_children(self):
        agent = await subject(); objective = Objective(POLICY); entered = asyncio.Event(); stopped = []
        async def complete(request):
            entered.set()
            try: await asyncio.Future()
            finally: stopped.append(True)
        worker = Agent(model='worker', provider=Provider(complete))
        async def summarize(value, context):
            context.delegate(worker, 'Work'); await entered.wait(); raise RuntimeError('failed')
        try:
            with self.assertRaises(SummaryUncertain): await agent.summarize('s', agent.capture_context(0, 2), summarize, objective=objective)
            self.assertEqual(stopped, [True]); cost = objective.inspect()['summary_costs']['s']
            self.assertEqual((cost['pending_model_calls'], cost['reserved_output_tokens']), (1, 10))
        finally: worker.close(); agent.close()

    async def test_known_late_summary_commits_before_local_cancel_and_releases_store(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); objective = Objective(POLICY, store=store, objective_id='root'); agent = await subject(store)
            entered = asyncio.Event()
            async def summarize(value, context):
                entered.set()
                try: await asyncio.Future()
                except asyncio.CancelledError: return 'R7 viernes'
            try:
                source = agent.capture_context(0, 2); pending = asyncio.create_task(agent.summarize('s', source, summarize, objective=objective)); await entered.wait()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): agent.close()
                self.assertTrue(agent.cancel())
                with self.assertRaises(asyncio.CancelledError): await pending
                known = await agent.summarize('s', source, lambda value, context: self.fail('no replay'), objective=objective)
                self.assertEqual(known['summary']['content'], 'R7 viernes')
            finally: agent.close(); store.close()

    async def test_repeated_task_cancel_waits_for_sync_summary_and_preserves_known_text(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); objective = Objective(POLICY, store=store, objective_id='root'); agent = await subject(store)
            entered = asyncio.Event(); release = threading.Event(); loop = asyncio.get_running_loop()
            def summarize(value, context): loop.call_soon_threadsafe(entered.set); release.wait(); return 'R7 viernes'
            pending = asyncio.create_task(agent.summarize('s', agent.capture_context(0, 2), summarize, objective=objective))
            try:
                await entered.wait(); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                self.assertFalse(pending.done())
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                release.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                self.assertEqual(objective.inspect()['summaries']['s']['content'], 'R7 viernes')
            finally:
                release.set(); await asyncio.gather(pending, return_exceptions=True); agent.close(); store.close()

    async def test_actual_process_exit_requires_reconciliation_without_callback_replay(self):
        agent = await subject(); source = agent.capture_context(0, 2); value = {'source': source, 'messages': agent.read_context(source)}
        with tempfile.TemporaryDirectory() as directory:
            script = '''import asyncio,json,os,sys
from pathlib import Path
from hive_sdk import Objective,SessionStore
store=SessionStore(sys.argv[1]);root=Objective(%r,store=store,objective_id='root')
def callback(value,context):
    Path(sys.argv[1],'effect').write_text('confirmed');os._exit(19)
asyncio.run(root.summarize('s',json.loads(sys.argv[2]),callback))
''' % POLICY
            env = dict(os.environ, PYTHONPATH=str(Path(__file__).resolve().parents[1] / 'python'))
            child = await asyncio.to_thread(subprocess.run, [sys.executable, '-c', script, directory, json.dumps(value)], env=env, capture_output=True, timeout=30)
            self.assertEqual(child.returncode, 19, child.stderr); self.assertEqual(Path(directory, 'effect').read_text(), 'confirmed')
            store = SessionStore(directory); objective = Objective(POLICY, store=store, objective_id='root')
            try:
                with self.assertRaises(SummaryUncertain): await agent.summarize('s', source, lambda value, context: self.fail('no replay'), objective=objective)
                objective.resolve_summary(key='s', execution_id=objective.inspect()['summaries']['s']['execution_id'], content='R7 viernes')
                known = await agent.summarize('s', source, lambda value, context: self.fail('no replay'), objective=objective)
                self.assertEqual((known['cost']['model_calls'], known['cost']['tool_calls']), (0, 1))
            finally: store.close(); agent.close()
