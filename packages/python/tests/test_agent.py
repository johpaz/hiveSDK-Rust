import asyncio
import json
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from hive_sdk import Agent, OpenAICompatible, Tool


def tool(execute=lambda args: args["a"] + args["b"]):
    return Tool("add", "Add numbers", {"type": "object", "properties": {
        "a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}, execute)


def call(id):
    return {"content": None, "tool_calls": [{"id": id, "name": "add", "arguments": {"a": 2, "b": 3}}],
            "usage": {"input_tokens": 10, "output_tokens": 2}}


class Scripted:
    def __init__(self, callback):
        self.callback = callback

    async def complete(self, request):
        return self.callback(request)


class AgentTests(unittest.IsolatedAsyncioTestCase):
    async def test_tool_roundtrip_and_history(self):
        calls = 0
        events = []

        def complete(request):
            nonlocal calls
            calls += 1
            if calls == 1:
                return call("a")
            if calls == 2:
                self.assertEqual(request["messages"][-1]["content"], "5")
            return {"content": "5"}

        async def add(args):
            return args["a"] + args["b"]

        agent = Agent(model="test", tools=[tool(add)], provider=Scripted(complete),
                      on_event=lambda event: events.append(event["type"]))
        result = await agent.run("Add")
        self.assertEqual(result["content"], "5")
        self.assertEqual(result["stats"]["model_calls"], 2)
        self.assertEqual(events, ["model", "tool", "model", "complete"])
        await agent.run("Again")
        self.assertEqual(len(agent.messages), 6)
        agent.messages.clear()
        self.assertEqual(len(agent.messages), 6)

    async def test_repetition_stops(self):
        calls = 0

        def complete(request):
            nonlocal calls
            calls += 1
            return call(str(calls))

        agent = Agent(model="test", tools=[tool()], provider=Scripted(complete))
        result = await agent.run("Add forever")
        self.assertEqual(result["stop_reason"], "repeated_tool_result")
        self.assertEqual(calls, 2)

    async def test_tool_error_is_feedback(self):
        calls = 0

        def fail(args):
            raise ValueError("offline")

        def complete(request):
            nonlocal calls
            calls += 1
            if calls == 1:
                return call("a")
            self.assertEqual(json.loads(request["messages"][-1]["content"])["error"]["message"], "offline")
            return {"content": "Could not add"}

        agent = Agent(model="test", tools=[tool(fail)], provider=Scripted(complete))
        self.assertEqual((await agent.run("Add"))["content"], "Could not add")

    async def test_failure_does_not_commit_history(self):
        def fail(request):
            raise ValueError("unavailable")

        agent = Agent(model="test", provider=Scripted(fail))
        with self.assertRaisesRegex(ValueError, "unavailable"):
            await agent.run("First")
        self.assertEqual(agent.messages, [])
        agent.close()
        with self.assertRaisesRegex(RuntimeError, "CLOSED"):
            await agent.run("Closed")

    async def test_concurrent_runs(self):
        event = asyncio.Event()

        class Waiting:
            async def complete(self, request):
                await event.wait()
                return {"content": "Done"}

        agent = Agent(model="test", provider=Waiting())
        task = asyncio.create_task(agent.run("First"))
        await asyncio.sleep(0)
        with self.assertRaisesRegex(RuntimeError, "BUSY"):
            await agent.run("Second")
        with self.assertRaisesRegex(RuntimeError, "BUSY"):
            agent.close()
        event.set()
        await task

    async def test_native_http_provider(self):
        received = []

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                received.append((self.path, json.loads(self.rfile.read(int(self.headers["content-length"])))))
                body = json.dumps({"choices": [{"finish_reason": "stop", "message": {"content": "HTTP works"}}],
                                   "usage": {"prompt_tokens": 7, "completion_tokens": 2}}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            agent = Agent(model="test", provider=OpenAICompatible(
                base_url=f"http://127.0.0.1:{server.server_port}/v1"))
            result = await agent.run("Hello")
            self.assertEqual(result["content"], "HTTP works")
            self.assertEqual(result["stats"]["input_tokens"], 7)
            self.assertEqual(received[0][0], "/v1/chat/completions")
        finally:
            await asyncio.to_thread(server.shutdown)
            server.server_close()
            thread.join()

    async def test_missing_usage_stops(self):
        reply = call("a")
        del reply["usage"]
        agent = Agent(model="test", tools=[tool()], policy={"max_total_tokens": 20},
                      provider=Scripted(lambda request: reply))
        self.assertEqual((await agent.run("Add"))["stop_reason"], "usage_unavailable")
