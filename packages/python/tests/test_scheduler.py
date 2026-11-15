import asyncio
import tempfile
import unittest
from hive_sdk import SessionStore, Objective, Agent


class SchedulerTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='hive-scheduler-')
        self.store = await SessionStore.open(self.directory.name)
    async def asyncTearDown(self):
        await self.store.close_async()
        self.directory.cleanup()

    async def test_scheduled_agent_inherits_root_without_replaying_publication(self):
        root = await Objective.open({'max_model_calls': 1, 'max_tool_calls': 1}, store=self.store, objective_id='root')
        queue = await self.store.open_queue('calendar')
        spec = {'schedule': {'kind': 'at', 'unix_ms': 0}, 'misfire_policy': 'fire_once', 'max_runs': 1,
                'job': {'kind': 'scheduled', 'lane': 'agent', 'payload': {'prompt': 'Run fixture'}, 'objective_id': 'root'}}
        self.assertFalse((await queue.create_schedule('one', spec))['enabled'])
        self.assertEqual((await queue.tick_schedule('one'))['outcome'], 'idle')
        await queue.enable_schedule('one')
        values = await asyncio.gather(queue.tick_schedule('one'), queue.tick_schedule('one'))
        self.assertEqual(sum(v['outcome'] == 'published' for v in values), 1)
        self.assertEqual((await root.inspect_async())['tool_calls'], 0)
        class Provider:
            async def complete(self, request):
                return {'content': 'fixture', 'usage': {'input_tokens': 1, 'output_tokens': 1}}
        agent = await Agent.open(store=self.store, objective=root, agent_id='worker', session_id='scheduled', model='fixture', provider=Provider())
        try:
            async def execute(job, context):
                self.assertEqual(job['input']['payload']['schedule']['scheduled_for'], 0)
                return {'type': 'completed', 'result': await context.run_agent(agent, job['input']['payload']['prompt'])}
            done = await queue.run_once({'scheduled': execute})
            self.assertEqual(done[0]['status'], 'completed')
            self.assertEqual((await queue.schedule('one'))['status'], 'completed')
            self.assertEqual((await queue.create_schedule('one', spec))['published'], 1)
            self.assertEqual((await root.inspect_async())['tool_calls'], 1)
        finally: agent.close()

    async def test_calendar_scopes_and_validation(self):
        queue = await self.store.open_queue('calendar')
        self.assertEqual(await queue.next_occurrence({'kind': 'cron', 'expression': '*/15 * * * * *', 'timezone': 'UTC'}, 0), 15000)
        with self.assertRaisesRegex(RuntimeError, 'timezone'):
            await queue.next_occurrence({'kind': 'cron', 'expression': '* * * * *', 'timezone': 'invalid'}, 0)
        for name in ['a', 'b', 'c']:
            await queue.create_schedule(name, {'schedule': {'kind': 'at', 'unix_ms': 0}, 'job': {'kind': 'test', 'lane': name, 'payload': {}}})
        page = await queue.list_schedules(limit=2)
        self.assertEqual(len(page['items']), 2)
        self.assertEqual(len((await queue.list_schedules(cursor=page['next_cursor']))['items']), 1)
        other = await self.store.open_queue('other')
        self.assertIsNone(await other.schedule('a'))
        with self.assertRaisesRegex(RuntimeError, 'scope'): await other.list_schedules(cursor=page['next_cursor'])

    async def test_opt_in_producer_stops_without_executing_queued_jobs(self):
        queue = await self.store.open_queue('watch')
        await queue.create_schedule('one', {'schedule': {'kind': 'at', 'unix_ms': 0}, 'misfire_policy': 'fire_once',
            'job': {'kind': 'fixture', 'lane': 'one', 'payload': {}}})
        await queue.enable_schedule('one')
        stop = asyncio.Event()
        producer = asyncio.create_task(queue.run_scheduler(poll_ms=10, cancel_event=stop))
        try:
            deadline = asyncio.get_running_loop().time() + 2
            while (await queue.schedule('one'))['published'] != 1:
                self.assertLess(asyncio.get_running_loop().time(), deadline)
                await asyncio.sleep(.005)
            with self.assertRaisesRegex(RuntimeError, 'BUSY'): await self.store.close_async()
        finally: stop.set(); await producer
        job = (await queue.list())['items'][0]
        self.assertEqual(job['status'], 'pending'); self.assertEqual(job['attempts'], 0)
