import asyncio
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from hive_sdk import Agent, Objective, ObjectiveLimit, SessionStore, Tool, ToolOperationUncertain

POLICY = {'max_model_calls': 3, 'max_output_tokens': 10, 'max_total_output_tokens': 15, 'max_depth': 1}
def reply(content='done'):
    return {'content': content, 'usage': {'input_tokens': 4, 'output_tokens': 3}}
def call():
    return {**reply(None), 'tool_calls': [{'id': 'c', 'name': 'delegate', 'arguments': {}}]}
def tool(execute):
    return Tool(name='delegate', description='Explicit delegation', parameters={'type': 'object'}, execute=execute, with_context=True)
class Provider:
    def __init__(self, complete): self.complete = complete


class DurableObjectiveTests(unittest.IsolatedAsyncioTestCase):
    async def test_cancellation_blocks_store_close_and_persists(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                objective = Objective(POLICY, store=store, objective_id='root')
                entered = asyncio.Event()
                async def complete(request): entered.set(); await asyncio.Future()
                agent = Agent(model='test', store=store, objective=objective, agent_id='a', session_id='s', provider=Provider(complete))
                pending = asyncio.create_task(agent.run('Work')); await entered.wait()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                objective.cancel()
                with self.assertRaises(asyncio.CancelledError): await pending
                agent.close()
                self.assertEqual(objective.inspect()['stopped'], 'cancelled')
                self.assertEqual(objective.inspect()['reservations'], {'1': 10})
                store.close(); store = SessionStore(directory)
                self.assertEqual(Objective(POLICY, store=store, objective_id='root').inspect()['stopped'], 'cancelled')
            finally: store.close()

    async def test_parent_and_child_share_root_after_reopening(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                objective = Objective(POLICY, store=store, objective_id='root')
                caps = []; calls = 0
                async def child_reply(request): caps.append(request['max_output_tokens']); return reply('child')
                async def parent_reply(request):
                    nonlocal calls
                    caps.append(request['max_output_tokens']); calls += 1
                    return call() if calls == 1 else reply('finished')
                async def delegate(args, context): return (await context.delegate(child, 'Work'))['content']
                child = Agent(model='child', objective=Objective(POLICY, store=store, objective_id='root'), store=store, agent_id='a', session_id='child', provider=Provider(child_reply))
                base = dict(model='parent', provider=Provider(parent_reply), tools=[tool(delegate)], agent_id='a', session_id='parent')
                parent = Agent(**base, store=store, objective=objective)
                self.assertEqual((await parent.run('Work'))['content'], 'finished')
                self.assertEqual(caps, [10, 10, 9])
                view = objective.inspect()
                self.assertEqual((view['model_calls'], view['tool_calls'], view['output_tokens']), (3, 1, 9))
                self.assertEqual(view['reservations'], {})
                self.assertEqual(store.inspect(agent_id='a', session_id='child')['run']['objective_depth'], 1)
                parent.close(); child.close(); store.close(); store = SessionStore(directory)
                objective = Objective(POLICY, store=store, objective_id='root')
                self.assertEqual(objective.inspect()['model_calls'], 3)
                parent = Agent(**base, store=store, objective=objective)
                with self.assertRaises(ObjectiveLimit): await parent.run('Again')
                parent.close(); self.assertEqual(calls, 2)
                with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFIG_MISMATCH'):
                    Objective({**POLICY, 'max_model_calls': 4}, store=store, objective_id='root')
            finally: store.close()

    async def test_clean_checkpoint_reuses_root_without_replaying_tool(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); effects = []; calls = 0
            try:
                objective = Objective(POLICY, store=store, objective_id='root')
                async def execute(args, context): effects.append(True); return 'effect'
                async def complete(request):
                    nonlocal calls
                    calls += 1
                    return call() if calls == 1 else reply('resumed')
                def pause(action):
                    if action['type'] == 'model' and calls: raise RuntimeError('pause')
                base = dict(model='test', provider=Provider(complete), tools=[tool(execute)], agent_id='a', session_id='s')
                agent = Agent(**base, store=store, objective=objective, on_event=pause)
                with self.assertRaisesRegex(RuntimeError, 'pause'): await agent.run('Work')
                agent.close(); self.assertEqual(objective.inspect()['model_calls'], 1)
                store.close(); store = SessionStore(directory)
                objective = Objective(POLICY, store=store, objective_id='root')
                agent = Agent(**base, store=store, objective=objective, resume=True)
                self.assertEqual((await agent.resume())['content'], 'resumed'); agent.close()
                self.assertEqual((len(effects), calls, objective.inspect()['model_calls']), (1, 2, 2))
            finally: store.close()

    async def test_uncertain_model_reconciliation_settles_once(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); calls = 0
            try:
                objective = Objective(POLICY, store=store, objective_id='root')
                async def complete(request):
                    nonlocal calls
                    calls += 1; raise RuntimeError('lost')
                base = dict(model='test', provider=Provider(complete), agent_id='a', session_id='s')
                agent = Agent(**base, store=store, objective=objective)
                with self.assertRaisesRegex(RuntimeError, 'lost'): await agent.run('Work')
                agent.close(); self.assertEqual(objective.inspect()['reservations'], {'1': 10})
                store.close(); store = SessionStore(directory)
                objective = Objective(POLICY, store=store, objective_id='root')
                with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFIG_MISMATCH'): Agent(**base, store=store, resume=True)
                with self.assertRaisesRegex(RuntimeError, 'SESSION_OPERATION_UNCERTAIN'): Agent(**base, store=store, objective=objective, resume=True)
                inspection = store.inspect(agent_id='a', session_id='s')
                self.assertEqual(inspection['run']['in_flight']['objective_permit'], 1)
                resolution = dict(agent_id='a', session_id='s', revision=inspection['revision'], execution_id=inspection['run']['in_flight']['execution_id'], reply=reply('verified'))
                store.resolve_model(**resolution)
                with self.assertRaises(RuntimeError): store.resolve_model(**resolution)
                agent = Agent(**base, store=store, objective=objective, resume=True)
                self.assertEqual((await agent.resume())['content'], 'verified'); agent.close()
                self.assertEqual(calls, 1); self.assertEqual(objective.inspect()['model_calls'], 1)
                self.assertEqual(objective.inspect()['reservations'], {})
            finally: store.close()

    async def test_unmetered_reply_stops_root_and_blocks_other_sessions(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                objective = Objective(POLICY, store=store, objective_id='root')
                async def complete(request): return {'content': 'unmetered'}
                agent = Agent(model='test', store=store, objective=objective, agent_id='a', session_id='s', provider=Provider(complete))
                self.assertEqual((await agent.run('Work'))['stop_reason'], 'usage_unavailable'); agent.close()
                self.assertEqual(objective.inspect()['stopped'], 'usage_unavailable')
                self.assertEqual(objective.inspect()['reservations'], {'1': 10})
                agent = Agent(model='test', store=store, objective=objective, agent_id='a', session_id='other', provider=Provider(complete))
                with self.assertRaises(ObjectiveLimit): await agent.run('Again')
                agent.close()
            finally: store.close()

    async def test_child_resume_preserves_depth(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                objective = Objective(POLICY, store=store, objective_id='root'); grand_calls = []
                async def grand_reply(request): grand_calls.append(True); return reply()
                async def parent_reply(request): return call()
                grand = Agent(model='grand', provider=Provider(grand_reply))
                async def child_delegate(args, context): return await context.delegate(grand, 'Work')
                base = dict(model='child', store=store, objective=objective, agent_id='a', session_id='child',
                    provider=Provider(parent_reply), tools=[tool(child_delegate)])
                def pause(action): raise RuntimeError('pause child')
                child = Agent(**base, on_event=pause)
                async def parent_delegate(args, context): return await context.delegate(child, 'Work')
                parent = Agent(model='parent', store=store, objective=objective, agent_id='a', session_id='parent',
                    provider=Provider(parent_reply), tools=[tool(parent_delegate)])
                with self.assertRaises(ToolOperationUncertain): await parent.run('Work')
                parent.close(); child.close()
                child = Agent(**base, resume=True)
                with self.assertRaises(ObjectiveLimit): await child.resume()
                child.close(); grand.close(); self.assertEqual(grand_calls, [])
                self.assertEqual(store.inspect(agent_id='a', session_id='child')['run']['objective_depth'], 1)
            finally: store.close()

    async def test_actual_process_exit_preserves_root_before_manual_resolution(self):
        with tempfile.TemporaryDirectory() as directory:
            script = '''import asyncio, os, sys
from hive_sdk import Agent, Objective, SessionStore
store=SessionStore(sys.argv[1])
objective=Objective(%r,store=store,objective_id='root')
async def complete(request): os._exit(19)
class Provider: pass
provider=Provider();provider.complete=complete
asyncio.run(Agent(model='test',store=store,objective=objective,agent_id='a',session_id='s',provider=provider).run('Work'))
''' % POLICY
            env = dict(os.environ, PYTHONPATH=str(Path(__file__).resolve().parents[1] / 'python'))
            process = await asyncio.to_thread(subprocess.run, [sys.executable, '-c', script, directory], env=env, capture_output=True, timeout=30)
            self.assertEqual(process.returncode, 19, process.stderr)
            store = SessionStore(directory)
            try:
                objective = Objective(POLICY, store=store, objective_id='root'); calls = []
                async def complete(request): calls.append(True); return reply()
                base = dict(model='test', store=store, objective=objective, agent_id='a', session_id='s', provider=Provider(complete))
                self.assertEqual(objective.inspect()['reservations'], {'1': 10})
                with self.assertRaisesRegex(RuntimeError, 'SESSION_OPERATION_UNCERTAIN'): Agent(**base, resume=True)
                inspection = store.inspect(agent_id='a', session_id='s')
                store.resolve_model(agent_id='a', session_id='s', revision=inspection['revision'], execution_id=inspection['run']['in_flight']['execution_id'], reply=reply('verified after crash'))
                agent = Agent(**base, resume=True)
                self.assertEqual((await agent.resume())['content'], 'verified after crash'); agent.close()
                self.assertEqual(calls, []); self.assertEqual(objective.inspect()['model_calls'], 1)
            finally: store.close()
