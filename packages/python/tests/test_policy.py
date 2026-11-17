import asyncio
import tempfile
import unittest
from hive_sdk import Agent, SessionStore


def scripted(calls):
    state = {'step': 0}
    seen = []

    class Provider:
        async def complete(self, request):
            last = request['messages'][-1]
            if last['role'] == 'tool':
                seen.append(last['content'])
            if state['step'] < len(calls):
                call = calls[state['step']]
                state['step'] += 1
                return {'content': None, 'tool_calls': [{'id': f"c{state['step']}", 'name': call['name'], 'arguments': call.get('arguments', {})}],
                        'usage': {'input_tokens': 1, 'output_tokens': 1}}
            return {'content': 'done', 'usage': {'input_tokens': 1, 'output_tokens': 1}}
    provider = Provider()
    provider.seen = seen
    return provider


def tool(name, log):
    from hive_sdk import Tool
    async def execute(args):
        log.append((name, args))
        return {'ok': name, 'args': args}
    return Tool(name=name, description=name, parameters={'type': 'object'}, execute=execute)


class PolicyTests(unittest.IsolatedAsyncioTestCase):
    async def test_policy_denies_allows_and_asks_and_denied_tools_never_run(self):
        ran = []
        provider = scripted([{'name': 'fs_write', 'arguments': {'path': '/etc/passwd'}}, {'name': 'fs_write', 'arguments': {'path': 'reports/a.md'}}, {'name': 'danger'}])
        agent = Agent(model='m', provider=provider, tools=[tool('fs_write', ran), tool('danger', ran)], max_turns=6,
            tool_policy={'default': 'allow', 'rules': [
                {'tools': ['fs_write'], 'decision': 'allow', 'when': [{'arg': '/path', 'op': 'prefix', 'value': 'reports/'}]},
                {'tools': ['fs_write'], 'decision': 'deny', 'reason': 'writes only under reports/'},
                {'tools': ['danger'], 'decision': 'ask', 'reason': 'dangerous'}]})
        self.assertEqual((await agent.run('go'))['content'], 'done')
        self.assertEqual([name for name, _ in ran], ['fs_write'])
        self.assertIn('POLICY_DENIED: writes only under reports/', provider.seen[0])
        self.assertIn('APPROVAL_UNAVAILABLE: dangerous', provider.seen[2])
        agent.close()

    async def test_approval_hooks_strictest_wins_and_failures_deny(self):
        ran, asked = [], []
        provider = scripted([{'name': n} for n in 'abcde'])
        def approve(ctx):
            asked.append((ctx['call']['name'], ctx['reason']))
            return True if ctx['call']['name'] == 'a' else {'approved': False, 'reason': 'not today'}
        def before(ctx):
            name = ctx['call']['name']
            return {'decision': 'deny', 'reason': 'hook says no'} if name == 'c' else {'decision': 'allow'} if name == 'd' else None
        agent = Agent(model='m', provider=provider, max_turns=10, tools=[tool(n, ran) for n in 'abcde'],
            tool_policy={'rules': [{'tools': ['a', 'b'], 'decision': 'ask', 'reason': 'needs a person'}, {'tools': ['e'], 'decision': 'deny', 'reason': 'never'}]},
            hooks={'approve': approve, 'before_tool': before})
        await agent.run('go')
        self.assertEqual([n for n, _ in ran], ['a', 'd'])
        self.assertEqual(asked, [('a', 'needs a person'), ('b', 'needs a person')])
        self.assertIn('not today', provider.seen[1])
        self.assertIn('hook says no', provider.seen[2])
        self.assertIn('never', provider.seen[4])
        agent.close()
        failing = []
        def broken_before(ctx):
            if ctx['call']['name'] == 'a': raise RuntimeError('boom')
            return {'decision': 'maybe'} if ctx['call']['name'] == 'b' else {'decision': 'ask'}
        def crash(ctx): raise RuntimeError('ui crashed')
        broken = Agent(model='m', provider=scripted([{'name': n} for n in 'abc']), max_turns=6, tools=[tool(n, failing) for n in 'abc'],
            hooks={'before_tool': broken_before, 'approve': crash})
        await broken.run('go')
        self.assertEqual(failing, [])
        broken.close()

    async def test_after_tool_redacts_and_a_failing_filter_withholds(self):
        ran = []
        provider = scripted([{'name': 'a'}, {'name': 'b'}])
        def after(ctx):
            if ctx['call']['name'] == 'a':
                return {'result': {**ctx['result'], 'args': 'REDACTED'}}
            raise RuntimeError('filter crashed')
        agent = Agent(model='m', provider=provider, max_turns=6, tools=[tool('a', ran), tool('b', ran)], hooks={'after_tool': after})
        await agent.run('go')
        self.assertEqual(len(ran), 2)
        self.assertIn('REDACTED', provider.seen[0])
        self.assertIn('POLICY_HOOK_FAILED: after_tool failed', provider.seen[1])
        agent.close()

    async def test_durable_denial_and_a_cancelled_approval_leaves_nothing_claimed(self):
        with tempfile.TemporaryDirectory() as directory:
            store = await SessionStore.open(directory, tenant_id='t')
            try:
                ran = []
                provider = scripted([{'name': 'a'}, {'name': 'b'}])
                denied = await Agent.open(store=store, agent_id='one', session_id='s', model='m', provider=provider, max_turns=6,
                    tools=[tool('a', ran), tool('b', ran)], tool_policy={'rules': [{'tools': ['a'], 'decision': 'deny', 'reason': 'no a'}]})
                self.assertEqual((await denied.run('go'))['content'], 'done')
                self.assertEqual([n for n, _ in ran], ['b'])
                self.assertIn('POLICY_DENIED: no a', provider.seen[0])
                denied.close()
                runs, asks = [], []
                async def approve(ctx):
                    asks.append(1)
                    if len(asks) == 1:
                        await asyncio.Event().wait()
                    return True
                options = dict(store=store, agent_id='two', session_id='s', model='m', max_turns=6, tools=[tool('a', runs)],
                               tool_policy={'default': 'ask'}, hooks={'approve': approve})
                first = await Agent.open(provider=scripted([{'name': 'a'}]), **options)
                task = asyncio.create_task(first.run('go'))
                await asyncio.sleep(0.2)
                first.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await task
                self.assertEqual(runs, [])
                first.close()
                second = await Agent.open(provider=scripted([]), resume=True, **options)
                self.assertEqual((await second.resume())['status'], 'completed')
                self.assertEqual(len(asks), 2)
                self.assertEqual(len(runs), 1)
                second.close()
            finally:
                await store.close_async()

    def test_invalid_options_are_rejected(self):
        with self.assertRaisesRegex(Exception, 'invalid tool policy'):
            Agent(model='m', tool_policy={'default': 'maybe'})
        with self.assertRaisesRegex(TypeError, 'must be callable'):
            Agent(model='m', hooks={'before_tool': 1})
        with self.assertRaisesRegex(TypeError, 'Unknown hook'):
            Agent(model='m', hooks={'afterwards': lambda c: None})


if __name__ == '__main__':
    unittest.main()
