"""Run with the installed wheel's interpreter, without PYTHONPATH."""
import asyncio
import tempfile
from pathlib import Path

import hive_sdk
from hive_sdk import Agent, Objective, SessionStore, Tool


class Provider:
    def complete(self, request):
        last = request['messages'][-1]
        usage = {'input_tokens': 1, 'output_tokens': 1}
        if last['role'] == 'tool': return {'content': f"sum={last['content']}", 'usage': usage}
        return {'content': None, 'tool_calls': [{'id': 'sum-1', 'name': 'sum', 'arguments': {'a': 2, 'b': 3}}], 'usage': usage}


async def main():
    tools = [Tool(name='sum', description='Sum integers', parameters={'type': 'object', 'required': ['a', 'b'],
        'properties': {'a': {'type': 'integer'}, 'b': {'type': 'integer'}}}, execute=lambda args: args['a'] + args['b'])]
    with tempfile.TemporaryDirectory(prefix='hive-installed-') as directory:
        store = await SessionStore.open(str(Path(directory) / 'db'))
        durable = await Objective.open({'max_model_calls': 2, 'max_tool_calls': 1}, store=store, objective_id='installed-root')
        options = dict(model='installed', provider=Provider(), tools=tools, store=store, objective=durable, agent_id='smoke', session_id='test')
        agent = await Agent.open(**options)
        assert (await agent.run('Sum'))['content'] == 'sum=5'
        agent.close()
        reopened = await Agent.open(**options)
        assert reopened.messages[-1]['content'] == 'sum=5'
        reopened.close()
        usage = await durable.inspect_async()
        assert usage['model_calls'] == 2 and usage['tool_calls'] == 1
        assert usage['input_tokens'] == 2 and usage['output_tokens'] == 2
        await store.close_async()
        objective = Objective({'max_model_calls': 2})
        budgeted = Agent(model='installed', objective=objective, provider=Provider(), tools=tools)
        assert (await budgeted.run('Sum'))['content'] == 'sum=5'
        assert objective.inspect()['model_calls'] == 2
        budgeted.close()
    print(f'Installed Python binding OK: {hive_sdk.__file__}')


asyncio.run(main())
