import asyncio
from contextlib import asynccontextmanager
import json
import tempfile
import threading
import unittest

from hive_sdk import Agent, OpenAICompatible, SessionStore, Tool


@asynccontextmanager
async def serve(handler):
    tasks, writers = set(), set()

    async def connected(reader, writer):
        task = asyncio.current_task(); tasks.add(task); writers.add(writer)
        try:
            header = await reader.readuntil(b"\r\n\r\n")
            length = next(int(line.split(b":", 1)[1]) for line in header.lower().split(b"\r\n") if line.startswith(b"content-length:"))
            body = json.loads(await reader.readexactly(length))
            await handler(body, reader, writer)
        finally:
            writer.close()
            await writer.wait_closed()
            writers.discard(writer); tasks.discard(task)

    server = await asyncio.start_server(connected, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    try:
        yield lambda **kwargs: OpenAICompatible(base_url=f"http://127.0.0.1:{port}/v1", **kwargs)
    finally:
        server.close(); await server.wait_closed()
        for writer in list(writers): writer.close()
        pending = list(tasks)
        for task in pending: task.cancel()
        if pending: await asyncio.gather(*pending, return_exceptions=True)


async def headers(writer):
    writer.write(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
    await writer.drain()


async def data(writer, value):
    writer.write(("data: " + json.dumps(value, ensure_ascii=False) + "\n\n").encode())
    await writer.drain()


async def finish(writer, reason, usage):
    await data(writer, {"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
    if usage is not None: await data(writer, {"choices": [], "usage": usage})
    writer.write(b"data: [DONE]\n\n"); await writer.drain()


def add(execute, **kwargs):
    return Tool("add", "Add", {"type": "object", "properties": {"a": {"type": "number"},
        "b": {"type": "number"}}, "required": ["a", "b"]}, execute, **kwargs)


class StreamingTests(unittest.IsolatedAsyncioTestCase):
    async def test_native_sse_tools_text_and_usage(self):
        requests, executions = 0, 0
        gate = asyncio.Event()

        async def handler(body, reader, writer):
            nonlocal requests
            self.assertTrue(body["stream"]); self.assertTrue(body["stream_options"]["include_usage"])
            requests += 1; await headers(writer)
            if requests == 1:
                await data(writer, {"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "a", "function": {"name": "ad", "arguments": '{"a":2,'}}]}}]})
                await data(writer, {"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "d", "arguments": '"b":3}'}}]}}]})
                await finish(writer, "tool_calls", {"prompt_tokens": 5, "completion_tokens": 3})
            else:
                self.assertEqual(body["messages"][-1]["content"], "5")
                await data(writer, {"choices": [{"delta": {"content": "Hola "}}]})
                await gate.wait()
                await data(writer, {"choices": [{"delta": {"content": "🐝"}}]})
                await finish(writer, "stop", {"prompt_tokens": 7, "completion_tokens": 2})

        def execute(args):
            nonlocal executions
            executions += 1; return args["a"] + args["b"]

        async with serve(handler) as provider:
            agent = Agent(model="test", provider=provider(), tools=[add(execute)])
            text, result = "", None
            async for event in agent.stream("Add"):
                if event["type"] == "tool_call_delta": self.assertEqual(executions, 0)
                if event["type"] == "text_delta":
                    text += event["text"]
                    if event["text"] == "Hola ": gate.set()
                if event["type"] == "result": result = event["result"]
            self.assertEqual(text, "Hola 🐝"); self.assertEqual(result["content"], text)
            self.assertEqual(result["stats"]["model_calls"], 2); self.assertEqual(result["stats"]["tool_calls"], 1)
            self.assertEqual(result["stats"]["input_tokens"], 12); self.assertEqual(result["stats"]["output_tokens"], 5)
            self.assertEqual(executions, 1); self.assertEqual(requests, 2)

    async def test_cancel_before_headers_closes_socket_and_keeps_uncertain_operation(self):
        received, closed = asyncio.Event(), asyncio.Event()

        async def handler(body, reader, writer):
            received.set(); await reader.read(); closed.set()

        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                async with serve(handler) as provider:
                    config = dict(model="test", provider=provider(), store=store, agent_id="a", session_id="s")
                    agent = Agent(**config); task = asyncio.create_task(agent.run("Wait"))
                    await asyncio.wait_for(received.wait(), 3)
                    self.assertTrue(agent.cancel())
                    with self.assertRaises(asyncio.CancelledError): await task
                    await asyncio.wait_for(closed.wait(), 3)
                    self.assertEqual(store.inspect(agent_id="a", session_id="s")["run"]["in_flight"]["kind"], "model")
                    with self.assertRaisesRegex(RuntimeError, "SESSION_OPERATION_UNCERTAIN"):
                        Agent(**config, resume=True)
                    self.assertFalse(agent.cancel())
            finally: store.close()

    async def test_aclose_cancels_a_partial_response(self):
        closed = asyncio.Event()

        async def handler(body, reader, writer):
            await headers(writer); await data(writer, {"choices": [{"delta": {"content": "partial"}}]})
            await reader.read(); closed.set()

        async with serve(handler) as provider:
            agent = Agent(model="test", provider=provider()); events = agent.stream("Wait")
            while (await events.__anext__())["type"] != "text_delta": pass
            await events.aclose(); await asyncio.wait_for(closed.wait(), 3)
            self.assertEqual(agent.messages, []); self.assertFalse(agent.cancel())

    async def test_deadline_closes_stream_without_retry(self):
        closed = asyncio.Event(); requests = 0

        async def handler(body, reader, writer):
            nonlocal requests
            requests += 1; await headers(writer)
            await data(writer, {"choices": [{"delta": {"content": "partial"}}]})
            await reader.read(); closed.set()

        async with serve(handler) as provider:
            agent = Agent(model="test", provider=provider(timeout_ms=250)); text = []
            with self.assertRaises(TimeoutError):
                async for event in agent.stream("Wait"):
                    if event["type"] == "text_delta": text.append(event["text"])
            await asyncio.wait_for(closed.wait(), 3)
            self.assertEqual(text, ["partial"]); self.assertEqual(requests, 1)

    async def test_partial_sse_does_not_execute_tool(self):
        executions = 0

        def execute(args):
            nonlocal executions
            executions += 1; return 5

        async def handler(body, reader, writer):
            await headers(writer)
            await data(writer, {"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "a",
                "function": {"name": "add", "arguments": '{"a":2,"b":3}'}}]}, "finish_reason": "tool_calls"}]})

        async with serve(handler) as provider:
            agent = Agent(model="test", provider=provider(), tools=[add(execute)])
            with self.assertRaisesRegex(RuntimeError, r"\[DONE\]"):
                async for event in agent.stream("Add"): pass
            self.assertEqual(executions, 0)

    async def test_pre_cancelled_event_does_not_invoke_provider(self):
        calls = 0

        class Provider:
            async def complete(self, request):
                nonlocal calls
                calls += 1; return {"content": "unused"}

        event = asyncio.Event(); event.set(); agent = Agent(model="test", provider=Provider())
        with self.assertRaises(asyncio.CancelledError): await agent.run("Wait", cancel_event=event)
        self.assertEqual(calls, 0)

    async def test_sync_tool_settles_and_checkpoints_before_cancel_releases_agent(self):
        started = threading.Event(); release = threading.Event(); calls = 0

        class Provider:
            async def complete(self, request):
                nonlocal calls
                calls += 1
                return {"content": None, "tool_calls": [{"id": "a", "name": "add", "arguments": {"a": 2, "b": 3}}]}

        def execute(args, context):
            started.set()
            if not release.wait(3): raise TimeoutError("test tool gate timed out")
            return {"cancelled": context.cancel_event.is_set()}

        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                agent = Agent(model="test", provider=Provider(), store=store, agent_id="a", session_id="s",
                    tools=[add(execute, with_context=True)])
                task = asyncio.create_task(agent.run("Add"))
                self.assertTrue(await asyncio.to_thread(started.wait, 3)); agent.cancel()
                self.assertFalse(task.done())
                with self.assertRaisesRegex(RuntimeError, "BUSY"): agent.close()
                release.set()
                with self.assertRaises(asyncio.CancelledError): await task
                run = store.inspect(agent_id="a", session_id="s")["run"]
                self.assertIsNone(run["in_flight"]); self.assertEqual(run["tools"][0]["result"], {"cancelled": True})
                self.assertEqual(calls, 1)
            finally: release.set(); store.close()

    async def test_stream_only_custom_provider_works_in_both_modes(self):
        class Provider:
            async def stream(self, request):
                yield {"type": "text_delta", "text": "Hello"}
                yield {"type": "reply", "reply": {"content": "Hello"}}

        agent = Agent(model="test", provider=Provider()); events = []
        async for event in agent.stream("Hello"): events.append(event)
        self.assertEqual(len([e for e in events if e["type"] == "text_delta"]), 1)
        self.assertEqual((await agent.run("Again"))["content"], "Hello")

    async def test_external_event_cancels_http_while_iterator_is_paused(self):
        closed, cancelled = asyncio.Event(), asyncio.Event()

        async def handler(body, reader, writer):
            await headers(writer); await data(writer, {"choices": [{"delta": {"content": "partial"}}]})
            await reader.read(); closed.set()

        async with serve(handler) as provider:
            agent = Agent(model="test", provider=provider()); events = agent.stream("Wait", cancel_event=cancelled)
            while (await events.__anext__())["type"] != "text_delta": pass
            cancelled.set(); await asyncio.wait_for(closed.wait(), 3)
            await events.aclose(); self.assertFalse(agent.cancel())

    async def test_resume_stream_keeps_usage_and_does_not_replay_tool(self):
        requests, executions, events = 0, 0, 0

        async def handler(body, reader, writer):
            nonlocal requests
            requests += 1; await headers(writer)
            if requests == 1:
                await data(writer, {"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "a", "function": {
                    "name": "add", "arguments": '{"a":2,"b":3}'}}]}}]})
                await finish(writer, "tool_calls", {"prompt_tokens": 5, "completion_tokens": 3})
            else:
                await data(writer, {"choices": [{"delta": {"content": "5"}}]})
                await finish(writer, "stop", {"prompt_tokens": 7, "completion_tokens": 2})

        def execute(args):
            nonlocal executions
            executions += 1; return 5

        async def on_event(action):
            nonlocal events
            if action["type"] == "model":
                events += 1
                if events == 2: raise ValueError("checkpoint interruption")

        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                async with serve(handler) as provider:
                    config = dict(model="test", provider=provider(), store=store, agent_id="a", session_id="s", tools=[add(execute)])
                    first = Agent(**config, on_event=on_event)
                    with self.assertRaisesRegex(ValueError, "checkpoint interruption"):
                        async for event in first.stream("Add"): pass
                    self.assertEqual(requests, 1)
                    resumed = Agent(**config, resume=True); result = None
                    async for event in resumed.resume_stream():
                        if event["type"] == "result": result = event["result"]
                    self.assertEqual(result["content"], "5"); self.assertEqual(result["stats"]["model_calls"], 2)
                    self.assertEqual(result["stats"]["input_tokens"], 12); self.assertEqual(result["stats"]["output_tokens"], 5)
                    self.assertEqual(result["stats"]["tool_calls"], 1); self.assertEqual(executions, 1); self.assertEqual(requests, 2)
            finally: store.close()
