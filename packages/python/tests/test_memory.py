import hashlib
import json
import tempfile
import unittest
from hive_sdk import Agent, SessionStore, Tool


class Provider:
    def __init__(self, complete): self.complete = complete


async def populate(agent):
    await agent.run('Pedido R7, entrega viernes 🦀'); await agent.run('Factura R8')


def plan(agent):
    return {'summary': {'id': 'summary-v1', 'source': agent.capture_context(1, 5), 'content': 'R7 viernes, R8 pagada'}, 'retrieved': []}


class MemoryTests(unittest.IsolatedAsyncioTestCase):
    async def test_local_retrieval_and_summary_preserve_history_without_auxiliary_model(self):
        requests = []
        async def complete(request): requests.append(request); return {'content': 'done'}
        agent = Agent(model='test', system='System', provider=Provider(complete))
        await populate(agent); original = agent.messages; source = agent.capture_context(1, 3)
        encoded = json.dumps(original[1:3], separators=(',', ':'), ensure_ascii=False).encode()
        self.assertEqual(source['sha256'], hashlib.sha256(encoded).hexdigest())
        self.assertEqual(agent.read_context(source), original[1:3])
        found = agent.search_context('R7 viernes')
        self.assertEqual(found['hits'][0]['score'], 2); self.assertEqual(found['hits'][0]['source']['start'], 1)
        self.assertEqual(found['serialized_bytes'], len(json.dumps(found['hits'], separators=(',', ':'), ensure_ascii=False).encode()))
        self.assertEqual(agent.search_context('R7', max_bytes=2)['hits'], [])
        selected = plan(agent); selected['retrieved'].append(source); agent.set_context_plan(selected)
        self.assertEqual(len(requests), 2); await agent.run('Next'); request = requests[-1]
        self.assertEqual(request['context']['summary_id'], 'summary-v1'); self.assertEqual(request['context']['dropped_messages'], 2)
        self.assertEqual(request['messages'][1]['role'], 'user')
        self.assertEqual(json.loads(request['messages'][1]['content'])['context_summary']['id'], 'summary-v1')
        self.assertEqual(request['messages'][2]['content'], original[1]['content']); self.assertEqual(agent.messages[:5], original)
        agent.set_context_plan(None); await agent.run('Restored')
        self.assertEqual(requests[-1]['messages'][1]['content'], original[1]['content']); self.assertNotIn('context', requests[-1]); agent.close()

    async def test_plan_reopens_and_stale_writer_cannot_replace_it(self):
        async def complete(request): return {'content': 'done'}
        config = dict(model='test', system='System', agent_id='a', session_id='s', provider=Provider(complete))
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                agent = Agent(**config, store=store); await populate(agent)
                stale = Agent(**config, store=store); selected = plan(agent); agent.set_context_plan(selected)
                with self.assertRaisesRegex(RuntimeError, 'SESSION_CONFLICT'): stale.set_context_plan(None)
                stale.close(); agent.close(); store.close(); store = SessionStore(directory)
                agent = Agent(**config, store=store); self.assertEqual(agent.context_plan, selected)
                copy = agent.context_plan; copy['summary']['content'] = 'tampered'; self.assertEqual(agent.context_plan, selected)
                await agent.run('Next'); self.assertEqual(len(agent.messages), 7); agent.close()
            finally: store.close()

    async def test_invalid_sources_and_duplicate_retrieval_do_not_infer(self):
        calls = []
        async def complete(request): calls.append(True); return {'content': 'done'}
        agent = Agent(model='test', system='System', provider=Provider(complete)); await populate(agent)
        with self.assertRaisesRegex(RuntimeError, 'closed user turns'): agent.capture_context(1, 2)
        bad = plan(agent); bad['summary']['source']['sha256'] = '0' * 64
        with self.assertRaisesRegex(RuntimeError, 'hash'): agent.set_context_plan(bad)
        source = agent.capture_context(1, 3)
        with self.assertRaisesRegex(RuntimeError, 'distinct'): agent.set_context_plan({'retrieved': [source, source]})
        with self.assertRaisesRegex(RuntimeError, 'lexical terms'): agent.search_context('!!!')
        self.assertEqual(len(calls), 2); self.assertIsNone(agent.context_plan); agent.close()
        with self.assertRaisesRegex(RuntimeError, 'CLOSED'): agent.search_context('R7')

    async def test_oversized_pinned_summary_stops_before_new_model_reservation(self):
        calls = []
        async def complete(request): calls.append(True); return {'content': 'done', 'usage': {'input_tokens': 1, 'output_tokens': 1}}
        agent = Agent(model='test', system='System', provider=Provider(complete),
            policy={'max_context_bytes': 1400, 'context_overflow': 'drop_oldest_turns', 'max_total_output_tokens': 20})
        await populate(agent); selected = plan(agent); selected['summary']['content'] = 'x' * 2000; agent.set_context_plan(selected)
        result = await agent.run('Next')
        self.assertEqual(result['stop_reason'], 'context_limit'); self.assertEqual(len(calls), 2)
        self.assertEqual(result['stats']['reserved_output_tokens'], 0); self.assertEqual(len(agent.messages), 6); agent.close()

    async def test_retrieval_retains_tool_batch_and_busy_agent_rejects_context_work(self):
        calls = []
        async def complete(request):
            calls.append(True)
            return {'content': None, 'tool_calls': [{'id': 'c', 'name': 'lookup', 'arguments': {'receipt': 'R9'}}]} if len(calls) == 1 else {'content': 'R9 confirmed'}
        def execute(args):
            with self.assertRaisesRegex(RuntimeError, 'BUSY'): agent.search_context('R9')
            return {'delivered': 'martes'}
        agent = Agent(model='test', provider=Provider(complete), tools=[Tool('lookup', 'Lookup', {'type': 'object'}, execute)])
        await agent.run('Lookup receipt'); hit = agent.search_context('R9 martes')['hits'][0]
        self.assertEqual(len(hit['messages']), 4); self.assertEqual(hit['messages'][1]['tool_calls'][0]['id'], 'c')
        self.assertEqual(hit['messages'][2]['tool_call_id'], 'c'); agent.close()
