import asyncio
import tempfile
import unittest
from hive_sdk import Agent, Objective, SessionStore


def spec(lane): return {'kind': 'test', 'lane': lane, 'payload': {'text': 'Hello'}}


class QueueTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='hive-queue-')
        self.store = await SessionStore.open(self.directory.name, tenant_id='first')
    async def asyncTearDown(self):
        await self.store.close_async()
        self.directory.cleanup()

    async def test_dedup_scopes_and_cancellation_fences(self):
        queue = await self.store.open_queue('work', policy={'max_concurrency': 2})
        data = {**spec('a'), 'idempotency_key': 'key'}
        first, second = await asyncio.gather(queue.enqueue(data), queue.enqueue(data))
        self.assertEqual(first['id'], second['id'])
        with self.assertRaisesRegex(RuntimeError, 'IDEMPOTENCY_CONFLICT'):
            await queue.enqueue({**data, 'payload': {'changed': True}})
        other = await self.store.for_tenant_async('second')
        try: self.assertIsNone(await (await other.open_queue('work')).get(first['id']))
        finally: await other.close_async()
        await queue.enqueue(spec('a'))
        active = await queue.claim('worker', ['test'])
        self.assertIsNone(await queue.claim('other', ['test']))
        await queue.cancel(active['id'])
        result = await queue.finish(active['id'], active['execution_id'], {'type': 'completed', 'result': 'late'})
        self.assertEqual(result['status'], 'cancelled'); self.assertIsNone(result['result'])
        with self.assertRaisesRegex(RuntimeError, 'FENCE'):
            await queue.finish(active['id'], active['execution_id'], {'type': 'cancelled'})

    async def test_callbacks_renew_and_preserve_lane_concurrency(self):
        queue = await self.store.open_queue('pool', policy={'max_concurrency': 2, 'lease_ms': 450})
        for lane in ['a', 'a', 'b', 'b']: await queue.enqueue(spec(lane))
        lanes = set(); counts = {'active': 0, 'peak': 0}
        async def execute(job, context):
            lane = job['input']['lane']; self.assertNotIn(lane, lanes); lanes.add(lane)
            counts['active'] += 1; counts['peak'] = max(counts['peak'], counts['active'])
            await asyncio.sleep(.6)
            counts['active'] -= 1; lanes.remove(lane)
            return {'type': 'completed', 'result': {'ok': True}}
        for _ in range(4): await queue.run_once({'test': execute})
        self.assertEqual(counts['peak'], 2); self.assertEqual(counts['active'], 0)
        for job in (await queue.list())['items']:
            self.assertEqual(job['status'], 'completed'); self.assertEqual(job['attempts'], 1); self.assertGreater(job['revision'], 3)

    async def test_uncertain_callback_requires_explicit_reconciliation(self):
        queue = await self.store.open_queue('uncertain')
        await queue.enqueue(spec('a')); await queue.enqueue(spec('a'))
        async def execute(job, context): raise ValueError('external outcome unknown')
        job, = await queue.run_once({'test': execute})
        self.assertEqual(job['status'], 'uncertain')
        self.assertEqual(await queue.run_once({'test': execute}), [])
        await queue.reconcile(job['id'], job['execution_id'], {'type': 'completed', 'result': {'receipt': 'external-1'}})
        self.assertIsNotNone(await queue.claim('worker', ['test']))

    async def test_queued_agent_keeps_root_accounting(self):
        objective = await Objective.open({'max_model_calls': 1, 'max_tool_calls': 8}, store=self.store, objective_id='root')
        queue = await self.store.open_queue('agents', policy={'max_concurrency': 1})
        calls = []
        class Provider:
            async def complete(self, request):
                calls.append(request); return {'content': 'done', 'usage': {'input_tokens': 1, 'output_tokens': 1}}
        agent = await Agent.open(store=self.store, objective=objective, agent_id='worker', session_id='chat', model='fixture', provider=Provider())
        async def execute(job, context):
            with self.assertRaises(AttributeError): context.depth = 0
            try:
                result = await context.run_agent(agent, 'Run')
                return {'type': 'completed', 'result': result} if result['status'] == 'completed' else {'type': 'failed', 'error': result['stop_reason']}
            except Exception as error: return {'type': 'failed', 'error': str(error)}
        try:
            await queue.enqueue({**spec('agent'), 'objective_id': 'root'})
            self.assertEqual((await queue.run_once({'test': execute}))[0]['status'], 'completed')
            await queue.enqueue({**spec('agent'), 'objective_id': 'root'})
            self.assertEqual((await queue.run_once({'test': execute}))[0]['status'], 'failed')
            self.assertEqual(len(calls), 1)
            root = await objective.inspect_async(); self.assertEqual(root['model_calls'], 1); self.assertEqual(root['tool_calls'], 2)
        finally: agent.close()

    async def test_task_cancellation_joins_callback_before_releasing_store(self):
        queue = await self.store.open_queue('cancel', policy={'lease_ms': 300})
        job = await queue.enqueue(spec('a')); started = asyncio.Event(); joined = []
        async def execute(job, context):
            started.set(); await context.cancel_event.wait(); await asyncio.sleep(.45); joined.append(True)
            return {'type': 'cancelled'}
        task = asyncio.create_task(queue.run_once({'test': execute}))
        await asyncio.wait_for(started.wait(), 2)
        with self.assertRaisesRegex(RuntimeError, 'BUSY'): await self.store.close_async()
        task.cancel()
        with self.assertRaises(asyncio.CancelledError): await asyncio.wait_for(task, 2)
        self.assertTrue(joined)
        self.assertEqual((await queue.get(job['id']))['status'], 'cancelled')

    async def test_sync_callbacks_do_not_block_heartbeat_storage_workers(self):
        import threading
        import time
        queue = await self.store.open_queue('sync', policy={'lease_ms': 450, 'max_concurrency': 1})
        await queue.enqueue(spec('a'))
        main_thread = threading.get_ident()
        def execute(job, context):
            self.assertNotEqual(threading.get_ident(), main_thread)
            time.sleep(.6)
            return {'type': 'completed', 'result': True}
        job, = await queue.run_once({'test': execute})
        self.assertEqual(job['status'], 'completed')
        self.assertGreater(job['revision'], 3)

    async def test_group_fanin_verification_and_scoped_execution(self):
        objective = await Objective.open({'max_tool_calls': 8}, store=self.store, objective_id='root')
        queue = await self.store.open_queue('groups')
        group = await queue.create_group('turn', {'objective_id': 'root', 'coordinator_agent_id': 'lead',
            'require_verification': True, 'tasks': [{'id': 'a', 'specialist_id': 'research', 'prompt': 'Find evidence'}]})
        calls = []
        class Provider:
            async def complete(self, request):
                calls.append(request)
                return {'content': 'fixture answer', 'usage': {'input_tokens': 1, 'output_tokens': 1}}
        wrong = await Agent.open(store=self.store, objective=objective, agent_id='wrong', session_id='wrong', model='fixture', provider=Provider())
        async def execute(job, context):
            with self.assertRaisesRegex(ValueError, 'BINDING_MISMATCH'):
                await context.run_agent(wrong, job['input']['payload']['prompt'])
            binding = job['input']['payload']['delegation']
            agent = await Agent.open(store=self.store, objective=objective, agent_id=binding['agent_id'],
                session_id=binding['session_id'], model='fixture', provider=Provider())
            try: return {'type': 'completed', 'result': await context.run_agent(agent, job['input']['payload']['prompt'])}
            finally: agent.close()
        try:
            children = await queue.run_once({group['specialist_kind']: execute})
            self.assertEqual(children[0]['status'], 'completed')
            with self.assertRaisesRegex(RuntimeError, 'NOT_READY'): await queue.synthesize_group('turn')
            value = await queue.group_verification_input('turn', 'a')
            async def check(_value, _context): return {'passed': True, 'evidence': ['fixture assertion']}
            await objective.verify('check', value, check)
            self.assertTrue((await queue.record_group_proof('turn', 'a', 'check'))['met'])
            first, second = await asyncio.gather(queue.synthesize_group('turn'), queue.synthesize_group('turn'))
            self.assertEqual(first['id'], second['id'])
            result = await queue.run_once({group['synthesis_kind']: execute})
            self.assertEqual(result[0]['status'], 'completed'); self.assertEqual(len(calls), 2)
            self.assertEqual((await queue.group('turn'))['status'], 'completed')
        finally: wrong.close()


class GoalTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.store = await SessionStore.open(self.dir.name, tenant_id='first')

    async def asyncTearDown(self):
        await self.store.close_async()
        self.dir.cleanup()

    async def finish(self, queue, kind, result):
        job = await queue.claim('w', [kind])
        self.assertIsNotNone(job, kind)
        return await queue.finish(job['id'], job['execution_id'], {'type': 'completed', 'result': result})

    async def test_goal_runs_to_acceptance_replans_and_cancels(self):
        await Objective.open({'max_model_calls': 10}, store=self.store, objective_id='root')
        queue = await self.store.open_queue('goals')
        goal = await queue.create_goal('ship', {'objective_id': 'root', 'goal': 'Ship the report', 'planner_agent_id': 'planner', 'coordinator_agent_id': 'lead', 'max_rounds': 2})
        self.assertEqual(goal['status'], 'planning')
        tasks = [{'id': 'a', 'specialist_id': 'research', 'prompt': 'Find evidence'}]
        for index in (0, 1):
            await self.finish(queue, goal['kinds']['plan'], {'tasks': tasks})
            goal = await queue.tick_goal('ship')
            self.assertEqual(goal['status'], 'running')
            await self.finish(queue, goal['kinds']['rounds'][index]['specialist'], {'text': 'evidence'})
            goal = await queue.tick_goal('ship')
            await self.finish(queue, goal['kinds']['rounds'][index]['synthesis'], {'text': 'synthesis'})
            goal = await queue.tick_goal('ship')
            self.assertEqual(goal['status'], 'reviewing')
            verdict = {'accepted': True, 'evidence': 'report.pdf written'} if index else {'accepted': False, 'feedback': 'needs a chart'}
            await self.finish(queue, goal['kinds']['review'], verdict)
            goal = await queue.tick_goal('ship')
            self.assertEqual(goal['status'], 'accepted' if index else 'planning')
        self.assertEqual(len(goal['rounds']), 2)
        with self.assertRaisesRegex(Exception, 'GOAL_IDEMPOTENCY_CONFLICT'):
            await queue.create_goal('ship', {'objective_id': 'root', 'goal': 'Another', 'planner_agent_id': 'p', 'coordinator_agent_id': 'c'})
        other = await queue.create_goal('stop', {'objective_id': 'root', 'goal': 'Never mind', 'planner_agent_id': 'p', 'coordinator_agent_id': 'c'})
        self.assertEqual((await queue.cancel_goal('stop'))['status'], 'cancelled')
        self.assertIsNone(await queue.claim('w', [other['kinds']['plan']]))
        with self.assertRaisesRegex(Exception, 'GOAL_NOT_FOUND'):
            await queue.goal('missing')

    async def test_run_goal_drives_agents_and_fails_on_non_json(self):
        from hive_sdk import run_goal, extract_json
        self.assertEqual(extract_json('Sure!\n```json\n{"a":1}\n```'), {'a': 1})
        self.assertEqual(extract_json('prefix {"a":{"b":2}} suffix'), {'a': {'b': 2}})
        self.assertIsNone(extract_json('no json'))
        self.assertIsNone(extract_json('[1]'))
        objective = await Objective.open({'max_model_calls': 20}, store=self.store, objective_id='root')
        queue = await self.store.open_queue('driven')
        reviews = []

        class Provider:
            async def complete(self, request):
                text = str(request)
                content = 'specialist output'
                if 'Plan round' in text:
                    content = 'Plan:\n```json\n{"tasks":[{"id":"a","specialist_id":"research","prompt":"Find"},{"id":"b","specialist_id":"writer","prompt":"Draft"}]}\n```'
                elif 'Review round' in text:
                    reviews.append(1)
                    content = '{"accepted":false,"feedback":"add sources"}' if len(reviews) == 1 else '{"accepted":true,"evidence":"sources added"}'
                return {'content': content, 'usage': {'input_tokens': 1, 'output_tokens': 1}}

        class Babble:
            async def complete(self, request):
                return {'content': 'I will think', 'usage': {'input_tokens': 1, 'output_tokens': 1}}

        seen = []

        async def agent_for(request):
            seen.append(request['stage'])
            return await Agent.open(store=self.store, objective=objective, agent_id=request['agent_id'], session_id=request['session_id'], model='fixture', provider=Provider())

        await queue.create_goal('g', {'objective_id': 'root', 'goal': 'Write it up', 'planner_agent_id': 'planner', 'coordinator_agent_id': 'lead', 'max_rounds': 3})
        report = await run_goal(queue, 'g', agent_for=agent_for)
        self.assertEqual(report['status'], 'accepted')
        self.assertEqual(len(report['rounds']), 2)
        self.assertEqual(len(seen), 10)
        self.assertEqual((await objective.inspect_async())['model_calls'], 10)
        await queue.create_goal('bad', {'objective_id': 'root', 'goal': 'x', 'planner_agent_id': 'planner', 'coordinator_agent_id': 'lead'})

        async def babbling(request):
            return await Agent.open(store=self.store, objective=objective, agent_id=request['agent_id'], session_id=request['session_id'], model='fixture', provider=Babble())
        failed = await run_goal(queue, 'bad', agent_for=babbling)
        self.assertEqual(failed['status'], 'failed')
        self.assertIn('not a JSON object', failed['error'])
        with self.assertRaises(TypeError):
            await run_goal(queue, 'g', agent_for=None)

    async def test_job_administration_and_event_cursor(self):
        queue = await self.store.open_queue('admin')
        def job(kind, lane, **extra): return {'kind': kind, 'lane': lane, 'payload': {'n': 1}, **extra}
        await queue.enqueue(job('flaky', 'l1', max_attempts=2))
        for n in range(3):
            await queue.enqueue(job('batch', f'b{n}'))
        self.assertEqual(len((await queue.list_jobs({'kind': 'batch'}))['items']), 3)
        claimed = await queue.claim('w', ['flaky'])
        failed = await queue.finish(claimed['id'], claimed['execution_id'], {'type': 'failed', 'error': 'boom'})
        self.assertEqual((await queue.retry_job(failed['id']))['status'], 'pending')
        again = await queue.claim('w', ['flaky'])
        await queue.finish(again['id'], again['execution_id'], {'type': 'failed', 'error': 'boom'})
        with self.assertRaisesRegex(Exception, 'JOB_RETRY_NOT_ALLOWED'):
            await queue.retry_job(failed['id'])
        self.assertEqual((await queue.retry_job(failed['id'], force=True))['status'], 'pending')
        self.assertEqual((await queue.cancel_jobs({'status': ['completed', 'failed']}))['cancelled'], [])
        self.assertEqual(len((await queue.cancel_jobs({'kind': 'batch'}))['cancelled']), 3)
        page = await queue.events(after=0, limit=3)
        self.assertEqual([e['seq'] for e in page['items']], [1, 2, 3])
        rest = await queue.events(after=page['next_after'])
        self.assertEqual(rest['items'][0]['seq'], 4)
        self.assertTrue(any(e['type'] == 'job.retried' for e in rest['items']))
        with self.assertRaisesRegex(Exception, 'explicit kind'):
            await queue.purge_jobs({}, 2_000_000_000_000)
        self.assertEqual((await queue.purge_jobs({'kind': 'flaky', 'status': ['pending']}, 2_000_000_000_000))['purged'], 0)
        self.assertEqual((await queue.prune_events(3))['removed'], 3)
