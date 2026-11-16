import tempfile
import unittest
from pathlib import Path
from hive_sdk import Agent, SessionStore

PNG = 'iVBORw0KGgo='
IMAGE = {'kind': 'image', 'media_type': 'image/png', 'data': PNG}


class MultimodalTests(unittest.IsolatedAsyncioTestCase):
    async def test_attachments_reach_the_provider_and_text_turns_are_unchanged(self):
        seen = []

        class Provider:
            async def complete(inner, request):
                seen.append(request['messages'][-1])
                return {'content': 'done'}
        agent = Agent(model='test', provider=Provider())
        url_image = {'kind': 'image', 'media_type': 'image/jpeg', 'url': 'https://example.com/a.jpg'}
        await agent.run({'text': 'Look', 'attachments': [IMAGE, url_image]})
        await agent.run('Plain')
        agent.close()
        self.assertEqual(seen[0]['content'], 'Look')
        self.assertEqual(seen[0]['attachments'], [IMAGE, url_image])
        self.assertNotIn('attachments', seen[1])

    async def test_invalid_attachments_fail_before_the_provider_is_called(self):
        calls = []

        class Provider:
            async def complete(inner, request):
                calls.append(request)
                return {'content': 'x'}
        agent = Agent(model='test', provider=Provider())
        for attachments in ([{**IMAGE, 'data': 'not base64!'}], [{**IMAGE, 'media_type': 'audio/wav'}],
                            [{'kind': 'image', 'media_type': 'image/png', 'data': PNG, 'url': 'https://example.com/a.png'}]):
            with self.assertRaises(Exception):
                await agent.run({'text': 'x', 'attachments': attachments})
        with self.assertRaisesRegex(Exception, 'at most 8'):
            await agent.run({'text': 'x', 'attachments': [IMAGE] * 9})
        agent.close()
        self.assertEqual(calls, [])

    async def test_token_budget_needs_an_allowance_and_ignores_payload_size(self):
        estimates = []

        class Provider:
            async def complete(inner, request):
                estimates.append(request['context']['token_estimate'])
                return {'content': 'done'}
        budget = {'encoding': 'o200k_base', 'max_tokens': 100000, 'framing': 'openai_chat_json'}
        refused = Agent(model='test', provider=Provider(), policy={'input_token_budget': budget})
        with self.assertRaisesRegex(Exception, 'attachment_tokens'):
            await refused.run({'text': 'Look', 'attachments': [IMAGE]})
        refused.close()
        allowed = Agent(model='test', provider=Provider(),
                        policy={'input_token_budget': {**budget, 'attachment_tokens': 1200}})
        await allowed.run({'text': 'Look', 'attachments': [IMAGE]})
        await allowed.run({'text': 'Look', 'attachments': [{**IMAGE, 'data': 'A' * (2 * 1024 * 1024)}]})
        allowed.close()
        self.assertEqual(estimates[0]['overhead_tokens'], 1200)
        self.assertEqual(estimates[1]['overhead_tokens'], 2400)

    async def test_durable_sessions_persist_attachments_across_reopen(self):
        users = []

        class Provider:
            async def complete(inner, request):
                users.append([m for m in request['messages'] if m['role'] == 'user'])
                return {'content': 'done'}
        config = dict(model='test', provider=Provider(), agent_id='a', session_id='s')
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(directory)
            try:
                first = Agent(**config, store=store)
                await first.run({'text': 'Look', 'attachments': [IMAGE]})
                first.close(); store.close(); store = SessionStore(directory)
                again = Agent(**config, store=store)
                await again.run('Next')
                again.close()
            finally:
                store.close()
        self.assertEqual(users[0][0]['attachments'], [IMAGE])
        self.assertEqual(len(users[1]), 2)
        self.assertEqual(users[1][0]['attachments'], [IMAGE])
        self.assertNotIn('attachments', users[1][1])


if __name__ == '__main__':
    unittest.main()
