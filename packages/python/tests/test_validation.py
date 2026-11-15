import json
import tempfile
import unittest
from hive_sdk import Agent, SessionStore, Tool

SCHEMA = {"type": "object", "properties": {"age": {"type": "integer", "minimum": 18}},
          "required": ["age"], "additionalProperties": False}


def tool(execute):
    return Tool("save", "Save", SCHEMA, execute)


def call(id, age):
    return {"content": None, "tool_calls": [{"id": id, "name": "save", "arguments": {"age": age}}]}


class ValidationTests(unittest.IsolatedAsyncioTestCase):
    async def test_invalid_schemas_and_external_references_fail_at_registration(self):
        for schema in [{"type": "object", "required": True},
                       {"type": "object", "$ref": "https://example.invalid/schema"},
                       {"type": "object", "$ref": "file:///tmp/not-a-schema.json"}]:
            with self.assertRaisesRegex(RuntimeError, "INVALID_TOOL_SCHEMA"):
                Agent(model="test", tools=[Tool("save", "Save", schema, lambda args: 0)])

    async def _exercise_repair(self, store=None):
        calls, executions = 0, 0
        events = []

        def execute(args):
            nonlocal executions
            executions += 1
            self.assertEqual(args["age"], 20)
            return "saved"

        class Provider:
            async def complete(inner, request):
                nonlocal calls
                calls += 1
                if calls == 1:
                    return call("bad", "20")
                if calls == 2:
                    feedback = json.loads(request["messages"][-1]["content"])
                    self.assertEqual(feedback["error"]["code"], "INVALID_TOOL_ARGUMENTS")
                    self.assertEqual(feedback["error"]["issues"][0]["instance_path"], "/age")
                    self.assertEqual(executions, 0)
                    return call("corrected", 20)
                return {"content": "Saved"}

        async def on_event(action):
            events.append(action["type"])

        options = dict(store=store, agent_id="a", session_id="s") if store is not None else {}
        agent = Agent(model="test", tools=[tool(execute)], provider=Provider(), on_event=on_event, **options)
        result = await agent.run("Save")
        self.assertEqual(result["content"], "Saved")
        self.assertEqual(executions, 1)
        self.assertEqual(result["stats"]["model_calls"], 3)
        self.assertEqual(result["stats"]["tool_calls"], 1)
        self.assertEqual(result["stats"]["tool_rejections"], 1)
        self.assertEqual(events, ["model", "tool_rejected", "model", "tool", "model", "complete"])
        if store is not None:
            ledger = store.inspect(agent_id="a", session_id="s")["run"]["tools"]
            self.assertEqual([entry["status"] for entry in ledger], ["rejected", "completed"])

    async def test_model_repairs_arguments_in_memory(self):
        await self._exercise_repair()

    async def test_model_repairs_arguments_with_hivedb(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                await self._exercise_repair(store)
            finally:
                store.close()

    async def test_repeated_invalid_arguments_stop(self):
        calls, executions = 0, 0

        def execute(args):
            nonlocal executions
            executions += 1
            return 0

        class Provider:
            async def complete(self, request):
                nonlocal calls
                calls += 1
                return call(str(calls), "20")

        result = await Agent(model="test", tools=[tool(execute)], provider=Provider()).run("Save forever")
        self.assertEqual(result["stop_reason"], "repeated_tool_result")
        self.assertEqual(result["stats"]["tool_rejections"], 2)
        self.assertEqual(calls, 2)
        self.assertEqual(executions, 0)

    async def test_rejection_survives_resume(self):
        calls, executions, events = 0, 0, 0

        def execute(args):
            nonlocal executions
            executions += 1
            return "saved"

        class Provider:
            async def complete(self, request):
                nonlocal calls
                calls += 1
                if calls == 1:
                    return call("bad", "20")
                if calls == 2:
                    return call("corrected", 20)
                return {"content": "Saved"}

        async def on_event(action):
            nonlocal events
            if action["type"] == "model":
                events += 1
                if events == 2:
                    raise ValueError("interrupted")

        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                config = dict(model="test", store=store, agent_id="a", session_id="s",
                              tools=[tool(execute)], provider=Provider())
                with self.assertRaisesRegex(ValueError, "interrupted"):
                    await Agent(**config, on_event=on_event).run("Save")
                snapshot = store.inspect(agent_id="a", session_id="s")
                self.assertEqual(snapshot["run"]["tools"][0]["status"], "rejected")
                self.assertIsNone(snapshot["run"]["in_flight"])
                result = await Agent(**config, resume=True).resume()
                self.assertEqual(result["stats"]["tool_rejections"], 1)
                self.assertEqual(result["stats"]["tool_calls"], 1)
                self.assertEqual(result["stats"]["model_calls"], 3)
                self.assertEqual(executions, 1)
            finally:
                store.close()
