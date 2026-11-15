import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from hive_sdk import Agent, SessionStore, Tool


def tool(execute, with_context=False):
    return Tool("pay", "Payment", {"type": "object"}, execute, with_context=with_context)


REPLY = {"content": None, "tool_calls": [{"id": "payment", "name": "pay", "arguments": {"amount": 10}}],
         "usage": {"input_tokens": 5, "output_tokens": 3}}


def options(store, tools, provider, **extra):
    return dict(model="test", store=store, tools=tools, provider=provider,
                agent_id="a", session_id="s", **extra)


class CheckpointTests(unittest.IsolatedAsyncioTestCase):
    async def test_confirmed_tool_is_not_repeated_on_resume(self):
        calls, payments = 0, 0
        context = None

        async def pay(args, ctx):
            nonlocal payments, context
            payments += 1
            context = ctx
            return {"receipt": "r1"}

        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1
                if calls == 1:
                    return REPLY
                self.assertEqual(json.loads(request["messages"][-1]["content"])["receipt"], "r1")
                return {"content": "Paid", "usage": {"input_tokens": 7, "output_tokens": 2}}

        events = 0

        async def on_event(action):
            nonlocal events
            if action["type"] == "model":
                events += 1
                if events == 2:
                    raise ValueError("interruption")

        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                tools = [tool(pay, with_context=True)]
                provider = Provider()
                agent = Agent(**options(store, tools, provider, on_event=on_event))
                with self.assertRaisesRegex(ValueError, "interruption"):
                    await agent.run("Pay")
                snapshot = store.inspect(agent_id="a", session_id="s")
                self.assertEqual(snapshot["run"]["tools"][0]["execution_id"], context["execution_id"])
                resumed = Agent(**options(store, tools, provider, resume=True))
                result = await resumed.resume()
                self.assertEqual(result["content"], "Paid")
                self.assertEqual(result["stats"]["model_calls"], 2)
                self.assertEqual(result["stats"]["input_tokens"], 12)
                self.assertEqual(result["stats"]["tool_calls"], 1)
                self.assertEqual(payments, 1)
                self.assertEqual(calls, 2)
            finally:
                store.close()

    async def test_process_exit_during_tool_needs_resolution(self):
        script = '''
import asyncio, json, os, sys
from pathlib import Path
from hive_sdk import Agent, SessionStore, Tool
class Provider:
    async def complete(self, request):
        return json.loads(sys.argv[2])
def pay(args, ctx):
    Path(sys.argv[1], 'receipt.json').write_text(json.dumps(ctx))
    os._exit(17)
async def main():
    store = SessionStore(sys.argv[1])
    agent = Agent(model='test',store=store,agent_id='a',session_id='s',provider=Provider(),
        tools=[Tool('pay','Payment',{'type':'object'},pay,with_context=True)])
    await agent.run('Pay')
asyncio.run(main())
'''
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run([sys.executable, "-c", script, directory, json.dumps(REPLY)],
                                    capture_output=True, text=True, timeout=10, env=dict(os.environ))
            self.assertEqual(result.returncode, 17, result.stderr)
            store = SessionStore(directory)
            try:
                replays = 0
                def forbidden(args):
                    nonlocal replays
                    replays += 1
                    return {"receipt": "unexpected"}

                class Provider:
                    async def complete(inner, request):
                        return {"content": "Paid"}

                tools, provider = [tool(forbidden)], Provider()
                with self.assertRaisesRegex(RuntimeError, "SESSION_OPERATION_UNCERTAIN"):
                    Agent(**options(store, tools, provider, resume=True))
                snapshot = store.inspect(agent_id="a", session_id="s")
                receipt = json.loads(Path(directory, "receipt.json").read_text())
                self.assertEqual(snapshot["run"]["in_flight"]["execution_id"], receipt["execution_id"])
                store.resolve_tool(agent_id="a", session_id="s", revision=snapshot["revision"],
                    execution_id=receipt["execution_id"], result={"receipt": "verified-externally"})
                resumed = Agent(**options(store, tools, provider, resume=True))
                self.assertEqual((await resumed.resume())["content"], "Paid")
                self.assertEqual(replays, 0)
            finally:
                store.close()

    async def test_uncertain_model_can_be_resolved_without_another_call(self):
        calls = 0

        class Provider:
            async def complete(self, request):
                nonlocal calls
                calls += 1
                raise ValueError("connection lost")

        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                provider = Provider()
                with self.assertRaisesRegex(ValueError, "connection lost"):
                    await Agent(**options(store, [], provider)).run("Hello")
                with self.assertRaisesRegex(RuntimeError, "SESSION_OPERATION_UNCERTAIN"):
                    Agent(**options(store, [], provider, resume=True))
                snapshot = store.inspect(agent_id="a", session_id="s")
                store.resolve_model(agent_id="a", session_id="s", revision=snapshot["revision"],
                    execution_id=snapshot["run"]["in_flight"]["execution_id"],
                    reply={"content": "Retrieved response", "usage": {"input_tokens": 4, "output_tokens": 2}})
                resumed = Agent(**options(store, [], provider, resume=True))
                self.assertEqual((await resumed.resume())["content"], "Retrieved response")
                self.assertEqual(calls, 1)
            finally:
                store.close()
