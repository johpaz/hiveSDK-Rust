import asyncio
import json
import tempfile
import threading
import unittest
from hive_sdk import Agent, Objective, SessionStore
from test_agent import Scripted

REPLY = {'content': 'Known 🦀', 'usage': {'input_tokens': 3, 'output_tokens': 2}}


class Gate:
    def __init__(self, handle, operation):
        self.handle = handle; self.operation = operation; self.entered = threading.Event(); self.released = threading.Event(); self.gated = False
    def dispatch(self, command):
        if not self.gated and json.loads(command)['op'] == self.operation:
            self.gated = True; self.entered.set(); self.released.wait()
        return self.handle.dispatch(command)
    async def ready(self):
        await asyncio.wait_for(asyncio.to_thread(self.entered.wait), 5)


class DurableHandleTests(unittest.IsolatedAsyncioTestCase):
    async def test_repeated_task_cancellation_joins_begin_worker_before_releasing_store(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); calls = []
            config = dict(model='test', store=store, agent_id='a', session_id='s', provider=Scripted(lambda request: calls.append(True) or REPLY))
            agent = Agent(**config); gate = Gate(agent._durable, 'begin_run'); agent._durable = gate
            pending = asyncio.create_task(agent.run('Work'))
            try:
                await gate.ready(); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                self.assertFalse(pending.done())
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): agent.close()
                self.assertEqual(agent.messages, [])
                gate.released.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                self.assertEqual(calls, []); self.assertIsNone(store.inspect(agent_id='a', session_id='s')['run']['in_flight'])
                resumed = Agent(**config, resume=True); self.assertEqual((await resumed.resume())['content'], REPLY['content']); self.assertEqual(len(calls), 1)
            finally:
                gate.released.set(); await asyncio.gather(pending, return_exceptions=True); store.close()

    async def test_cancelled_claim_never_dispatches_callback_and_remains_uncertain(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); calls = []
            config = dict(model='test', store=store, agent_id='a', session_id='s', provider=Scripted(lambda request: calls.append(True) or REPLY))
            agent = Agent(**config); gate = Gate(agent._durable, 'claim'); agent._durable = gate
            pending = asyncio.create_task(agent.run('Work'))
            try:
                await gate.ready(); agent.cancel(); self.assertFalse(pending.done()); gate.released.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                self.assertEqual(calls, []); self.assertEqual(store.inspect(agent_id='a', session_id='s')['run']['in_flight']['kind'], 'model')
                with self.assertRaisesRegex(RuntimeError, 'SESSION_OPERATION_UNCERTAIN'): Agent(**config, resume=True)
            finally:
                gate.released.set(); await asyncio.gather(pending, return_exceptions=True); store.close()

    async def test_cancelled_known_reply_is_settled_and_terminal_resume_needs_no_model(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); calls = []
            objective = Objective({'max_model_calls': 1, 'max_output_tokens': 8, 'max_total_output_tokens': 8}, store=store, objective_id='root')
            config = dict(model='test', store=store, objective=objective, agent_id='a', session_id='s', provider=Scripted(lambda request: calls.append(True) or REPLY))
            agent = Agent(**config); gate = Gate(agent._durable, 'advance'); agent._durable = gate
            pending = asyncio.create_task(agent.run('Work'))
            try:
                await gate.ready(); pending.cancel(); await asyncio.sleep(0); pending.cancel(); await asyncio.sleep(0)
                self.assertFalse(pending.done())
                with self.assertRaisesRegex(RuntimeError, 'BUSY'): store.close()
                gate.released.set()
                with self.assertRaises(asyncio.CancelledError): await pending
                snapshot = store.inspect(agent_id='a', session_id='s'); self.assertIsNone(snapshot['run']['in_flight']); self.assertEqual(snapshot['run']['state']['action']['type'], 'complete')
                self.assertEqual(objective.inspect()['output_tokens'], 2); self.assertEqual(objective.inspect()['reservations'], {})
                self.assertEqual((await Agent(**config, resume=True).resume())['content'], REPLY['content']); self.assertEqual(len(calls), 1)
            finally:
                gate.released.set(); await asyncio.gather(pending, return_exceptions=True); store.close()

    async def test_store_closure_invalidates_commands_but_preserves_cached_getters(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory); calls = []
            agent = Agent(model='test', store=store, agent_id='a', session_id='s', provider=Scripted(lambda request: calls.append(True) or REPLY))
            try:
                await agent.run('Work'); messages = agent.messages; store.close(); self.assertEqual(agent.messages, messages)
                source = agent.capture_context(0, 2); self.assertEqual(len(agent.read_context(source)), 2)
                with self.assertRaisesRegex(RuntimeError, 'CLOSED'): await agent.run('Closed')
                self.assertEqual(len(calls), 1)
            finally: store.close()
