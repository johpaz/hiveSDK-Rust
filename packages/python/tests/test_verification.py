import asyncio
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest

from hive_sdk import Agent, Objective, ObjectiveLimit, SessionStore, VerificationUncertain

POLICY = {'max_output_tokens': 10, 'max_total_output_tokens': 15}
REVIEW = {'evaluation_id': 'fixture-contract-v1', 'justification': 'Validate the budget contract, not production model quality'}
USAGE = {'input_tokens': 2, 'output_tokens': 3}
class Provider:
    def __init__(self, complete): self.complete = complete


class VerificationTests(unittest.IsolatedAsyncioTestCase):
    async def test_reviewer_cannot_bypass_depth_limit(self):
        objective = Objective({**POLICY, 'max_depth': 0}); calls = []
        async def complete(request): calls.append(True); return {'content': '{}', 'usage': USAGE}
        reviewer = Agent(model='reviewer', provider=Provider(complete))
        async def check(value, context): await context.delegate(reviewer, 'Work'); return {'passed': True}
        try:
            with self.assertRaises(ObjectiveLimit): await objective.verify('check', {}, check, model_review=REVIEW)
            self.assertEqual(calls, []); self.assertEqual(objective.inspect()['model_calls'], 0)
        finally: reviewer.close()

    async def test_successful_callback_waits_for_unawaited_reviewer(self):
        objective = Objective(POLICY); entered, gate = asyncio.Event(), asyncio.Event()
        async def complete(request): entered.set(); await gate.wait(); return {'content': 'done', 'usage': USAGE}
        reviewer = Agent(model='reviewer', provider=Provider(complete))
        async def check(value, context): context.delegate(reviewer, 'Work'); return {'passed': True}
        pending = asyncio.create_task(objective.verify('check', {}, check, model_review=REVIEW))
        try:
            await entered.wait(); self.assertFalse(pending.done()); gate.set()
            self.assertTrue((await pending)['verdict']['passed'])
            self.assertEqual(objective.inspect()['model_calls'], 1); self.assertEqual(objective.inspect()['reservations'], {})
        finally: gate.set(); await asyncio.gather(pending, return_exceptions=True); reviewer.close()

    async def test_explicit_verdict_and_cached_failure_do_not_add_model_calls(self):
        for durable in [False, True]:
            with tempfile.TemporaryDirectory() as directory:
                store = SessionStore(directory) if durable else None
                try:
                    objective = Objective(POLICY, store=store, objective_id='root' if store else None)
                    calls = []; checks = []; captured = []
                    async def complete(request): calls.append(True); return {'content': 'done', 'usage': USAGE}
                    agent = Agent(model='test', objective=objective, provider=Provider(complete))
                    await agent.run('Work'); agent.close()
                    self.assertEqual(len(calls), 1); self.assertEqual(objective.inspect()['verifications'], {})
                    value = {'receipt': 'missing', 'expected': 5}
                    def check(value, context):
                        checks.append(True); captured.append(context); value['expected'] = 999
                        self.assertEqual(sorted(context), ['execution_id', 'objective_id', 'verification_key'])
                        return {'passed': False, 'reason': 'Receipt is absent', 'evidence': {'receipt': None}}
                    result = await objective.verify('receipt-v1', value, check)
                    def fail(value, context): raise AssertionError('no replay')
                    self.assertEqual(await objective.verify('receipt-v1', value, fail), result)
                    self.assertEqual((len(checks), len(calls)), (1, 1)); self.assertEqual(value['expected'], 5)
                    with self.assertRaisesRegex(RuntimeError, 'CLOSED'): captured[0].delegate(agent, 'Late')
                    with self.assertRaisesRegex(RuntimeError, 'already bound'):
                        await objective.verify('receipt-v1', {**value, 'expected': 6}, fail)
                    self.assertEqual(objective.inspect()['tool_calls'], 1)
                finally:
                    if store: store.close()

    async def test_invalid_input_and_review_metadata_fail_before_claim(self):
        objective = Objective(); calls = []
        def check(value, context): calls.append(True); return {'passed': True}
        for key, value, options in [('', 1, {}), ('large', 'x' * 65537, {}),
                ('review', 1, {'model_review': {'evaluation_id': '', 'justification': 'why'}})]:
            with self.assertRaises(RuntimeError): await objective.verify(key, value, check, **options)
        self.assertEqual(calls, []); self.assertEqual(objective.inspect()['tool_calls'], 0)

    async def test_malformed_verdict_needs_fenced_resolution(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                objective = Objective(store=store, objective_id='root')
                with self.assertRaises(VerificationUncertain): await objective.verify('check', {'n': 1}, lambda value, context: {'passed': 'true'})
                def fail(value, context): raise AssertionError('no replay')
                with self.assertRaises(VerificationUncertain): await objective.verify('check', {'n': 1}, fail)
                execution_id = objective.inspect()['verifications']['check']['execution_id']
                with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFLICT'):
                    objective.resolve_verification(key='check', execution_id='stale', verdict={'passed': True})
                result = objective.resolve_verification(key='check', execution_id=execution_id, verdict={'passed': True, 'evidence': {'receipt': 'verified'}})
                with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFLICT'):
                    objective.resolve_verification(key='check', execution_id=execution_id, verdict={'passed': False})
                self.assertEqual(await objective.verify('check', {'n': 1}, fail), result)
                self.assertEqual(objective.inspect()['tool_calls'], 1)
            finally: store.close()

    async def test_model_review_is_opt_in_and_uses_remaining_root_allowance(self):
        for allowed in [False, True]:
            objective = Objective({**POLICY, 'max_model_calls': 1}); calls = []
            async def complete(request):
                calls.append(True); self.assertEqual(request['max_output_tokens'], 10)
                return {'content': '{"passed":true,"reason":"fixture"}', 'usage': USAGE}
            reviewer = Agent(model='reviewer', provider=Provider(complete))
            async def check(value, context): return json.loads((await context.delegate(reviewer, 'Check the receipt'))['content'])
            try:
                if not allowed:
                    with self.assertRaises(VerificationUncertain): await objective.verify('check', {'n': 1}, check)
                else:
                    result = await objective.verify('check', {'n': 1}, check, model_review=REVIEW)
                    self.assertTrue(result['verdict']['passed'])
                    with self.assertRaises(ObjectiveLimit): await objective.verify('another', {'n': 1}, check, model_review=REVIEW)
                    view = objective.inspect()
                    self.assertEqual(view['verifications']['check']['model_review']['evaluation_id'], REVIEW['evaluation_id'])
                    self.assertEqual(view['output_tokens'], 3)
                self.assertEqual(len(calls), int(allowed))
            finally: reviewer.close()

    async def test_failing_callback_cancels_and_joins_unawaited_reviewer(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            objective = Objective(POLICY, store=store, objective_id='root')
            entered = asyncio.Event(); stopped = []
            async def complete(request):
                entered.set()
                try: await asyncio.Future()
                finally: stopped.append(True)
            reviewer = Agent(model='reviewer', provider=Provider(complete))
            async def check(value, context):
                context.delegate(reviewer, 'Work'); await entered.wait()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                raise RuntimeError('callback failed')
            try:
                with self.assertRaises(VerificationUncertain): await objective.verify('check', {}, check, model_review=REVIEW)
                self.assertEqual(stopped, [True]); self.assertEqual(objective.inspect()['model_calls'], 1)
                self.assertEqual(objective.inspect()['reservations'], {'1': 10})
            finally: reviewer.close(); store.close()

    async def test_known_late_verdict_is_committed_before_cancel_is_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); objective = Objective(POLICY, store=store, objective_id='root')
            entered, gate, cancel = asyncio.Event(), asyncio.Event(), asyncio.Event()
            async def check(value, context):
                entered.set()
                try: await gate.wait()
                except asyncio.CancelledError: return {'passed': True}
                return {'passed': True}
            try:
                pending = asyncio.create_task(objective.verify('check', {'n': 1}, check, cancel_event=cancel))
                await entered.wait()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'):
                    objective.resolve_verification(key='check', execution_id='x', verdict={'passed': False})
                cancel.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                self.assertTrue(objective.inspect()['verifications']['check']['verdict']['passed'])
                self.assertTrue((await objective.verify('check', {'n': 1}, lambda value, context: self.fail('no replay')))['verdict']['passed'])
            finally: store.close()

    async def test_same_active_key_cannot_invoke_the_callback_twice(self):
        objective = Objective(); gate = asyncio.Event(); entered = asyncio.Event(); calls = []
        async def check(value, context): calls.append(True); entered.set(); await gate.wait(); return {'passed': True}
        pending = asyncio.create_task(objective.verify('check', {}, check)); await entered.wait()
        with self.assertRaises(VerificationUncertain): await objective.verify('check', {}, lambda value, context: self.fail('no replay'))
        gate.set(); await pending
        self.assertEqual(len(calls), 1); self.assertEqual(objective.inspect()['tool_calls'], 1)

    async def test_actual_process_exit_retains_verification_without_repeating_effect(self):
        with tempfile.TemporaryDirectory() as directory:
            script = '''import asyncio,os,sys
from pathlib import Path
from hive_sdk import Objective,SessionStore
store=SessionStore(sys.argv[1]);objective=Objective(%r,store=store,objective_id='root')
def check(value,context):
    Path(sys.argv[1],'receipt').write_text('confirmed');os._exit(19)
asyncio.run(objective.verify('receipt-v1',{'receipt':'r1'},check))
''' % POLICY
            env = dict(os.environ, PYTHONPATH=str(Path(__file__).resolve().parents[1] / 'python'))
            child = await asyncio.to_thread(subprocess.run, [sys.executable, '-c', script, directory], env=env, capture_output=True, timeout=30)
            self.assertEqual(child.returncode, 19, child.stderr)
            self.assertEqual(Path(directory, 'receipt').read_text(), 'confirmed')
            store = SessionStore(directory)
            try:
                objective = Objective(POLICY, store=store, objective_id='root')
                def fail(value, context): raise AssertionError('no replay')
                with self.assertRaises(VerificationUncertain): await objective.verify('receipt-v1', {'receipt': 'r1'}, fail)
                execution_id = objective.inspect()['verifications']['receipt-v1']['execution_id']
                objective.resolve_verification(key='receipt-v1', execution_id=execution_id, verdict={'passed': True, 'evidence': {'receipt': 'r1'}})
                self.assertTrue((await objective.verify('receipt-v1', {'receipt': 'r1'}, fail))['verdict']['passed'])
                self.assertEqual((objective.inspect()['tool_calls'], objective.inspect()['model_calls']), (1, 0))
            finally: store.close()

    async def test_repeated_task_cancel_keeps_sync_callback_scoped_until_settlement(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); objective = Objective(store=store, objective_id='root')
            entered = asyncio.Event(); release = threading.Event(); loop = asyncio.get_running_loop()
            def check(value, context):
                loop.call_soon_threadsafe(entered.set); release.wait(); return {'passed': True}
            pending = asyncio.create_task(objective.verify('check', {}, check))
            try:
                await entered.wait(); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                self.assertFalse(pending.done())
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                release.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                self.assertTrue(objective.inspect()['verifications']['check']['verdict']['passed'])
            finally:
                release.set()
                if not pending.done(): await asyncio.gather(pending, return_exceptions=True)
                store.close()

    async def test_cancellation_during_watcher_cleanup_releases_store_after_settlement(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); objective = Objective(store=store, objective_id='root')
            started, cleaning, release = asyncio.Event(), asyncio.Event(), asyncio.Event()
            class SlowCancellationEvent:
                def is_set(self): return False
                async def wait(self):
                    started.set()
                    try: await asyncio.Future()
                    finally: cleaning.set(); await release.wait()
            async def check(value, context): await started.wait(); return {'passed': True}
            pending = asyncio.create_task(objective.verify('check', {}, check, cancel_event=SlowCancellationEvent()))
            try:
                await cleaning.wait()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                pending.cancel(); await asyncio.sleep(0); release.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                self.assertTrue(objective.inspect()['verifications']['check']['verdict']['passed'])
            finally:
                release.set()
                if not pending.done(): await asyncio.gather(pending, return_exceptions=True)
                store.close()
