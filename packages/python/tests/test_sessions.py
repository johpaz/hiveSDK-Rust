import tempfile
import unittest
import asyncio
from hive_sdk import Agent, SessionStore


class Provider:
    async def complete(self, request):
        return {"content": "Hi"}


def options(store):
    return dict(model="test", system="Be brief", provider=Provider(), store=store,
                agent_id="agent", session_id="session")


class SessionTests(unittest.IsolatedAsyncioTestCase):
    async def test_reopen_preserves_history_and_checks_config(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory, tenant_id="tenant")
            try:
                agent = Agent(**options(store))
                await agent.run("Hello")
                agent.close()
                store.close()
                store = SessionStore(directory, tenant_id="tenant")
                reopened = Agent(**options(store))
                self.assertEqual(len(reopened.messages), 3)
                await reopened.run("Follow up")
                self.assertEqual(len(reopened.messages), 5)
                changed = {**options(store), "model": "other"}
                with self.assertRaisesRegex(RuntimeError, "SESSION_CONFIG_MISMATCH"):
                    Agent(**changed)
            finally:
                store.close()

    async def test_conflict_precedes_provider_call(self):
        calls = 0

        class Counting:
            async def complete(self, request):
                nonlocal calls
                calls += 1
                return {"content": "Hi"}

        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                config = {**options(store), "provider": Counting()}
                first, stale = Agent(**config), Agent(**config)
                await first.run("Hello")
                with self.assertRaisesRegex(RuntimeError, "SESSION_CONFLICT"):
                    await stale.run("Stale")
                self.assertEqual(calls, 1)
            finally:
                store.close()

    async def test_failed_run_needs_explicit_recovery(self):
        class Failing:
            async def complete(self, request):
                raise ValueError("offline")

        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                agent = Agent(**options(store))
                await agent.run("Hello")
                failing = Agent(**{**options(store), "provider": Failing()})
                with self.assertRaisesRegex(ValueError, "offline"):
                    await failing.run("Work")
                with self.assertRaisesRegex(RuntimeError, "SESSION_INTERRUPTED"):
                    Agent(**options(store))
                failing.close()
                store.recover(agent_id="agent", session_id="session")
                recovered = Agent(**options(store))
                self.assertEqual(len(recovered.messages), 3)
                self.assertEqual((await recovered.run("Explicit retry"))["status"], "completed")
            finally:
                store.close()

    async def test_tenants_keep_separate_histories(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory, tenant_id="one")
            other = store.for_tenant("two")
            try:
                await Agent(**options(store)).run("Private to one")
                self.assertEqual(len(Agent(**options(other)).messages), 1)
                self.assertEqual(len(Agent(**options(store)).messages), 3)
            finally:
                other.close()
                store.close()

    async def test_active_run_blocks_close_and_recovery(self):
        event = asyncio.Event()

        class Waiting:
            async def complete(self, request):
                await event.wait()
                return {"content": "Done"}

        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                agent = Agent(**{**options(store), "provider": Waiting()})
                task = asyncio.create_task(agent.run("Wait"))
                await asyncio.sleep(0)
                with self.assertRaisesRegex(RuntimeError, "BUSY"):
                    store.close()
                with self.assertRaisesRegex(RuntimeError, "BUSY"):
                    store.recover(agent_id="agent", session_id="session")
                event.set()
                await task
            finally:
                event.set()
                store.close()
