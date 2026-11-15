"""Summary/retrieval plan and full tool history round-trip Node -> Python -> Node."""
import asyncio
import json
from pathlib import Path
import subprocess
import tempfile
from hive_sdk import Agent, SessionStore, Tool

module = json.dumps((Path(__file__).resolve().parent.parent / 'packages/npm/index.mjs').as_uri())


async def main():
    with tempfile.TemporaryDirectory() as directory:
        create = f'''
import assert from 'node:assert/strict';
import {{Agent,SessionStore}} from {module};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const agent=new Agent({{model:'test',system:'System',store,agentId:'a',sessionId:'s',
tools:[{{name:'read',description:'Read',parameters:{{type:'object'}},execute:()=>5}}],
provider:{{complete:()=>({{content:'R7 viernes 🦀'}})}}}});
await agent.run('Pedido R7');
const source=agent.captureContext(1,3);
agent.setContextPlan({{summary:{{id:'node-summary',source,content:'R7 viernes 🦀'}},retrieved:[source]}});
assert.equal(agent.searchContext('R7 viernes').hits[0].source.sha256,source.sha256);agent.close();store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', create, directory], check=True, timeout=20)
        store = SessionStore(directory, tenant_id='parity'); calls = []
        class Provider:
            async def complete(self, request):
                calls.append(True)
                assert request['context']['summary_id'] == 'node-summary'
                if len(calls) == 1:
                    return {'content': None, 'tool_calls': [{'id': 'c', 'name': 'read', 'arguments': {'receipt': 'R8', 'n': 1.0, 'z': -0.0}}]}
                return {'content': 'R8 pagada'}
        try:
            agent = Agent(model='test', system='System', store=store, agent_id='a', session_id='s', provider=Provider(),
                tools=[Tool('read', 'Read', {'type': 'object'}, lambda args: {'confirmed': args['receipt']})])
            assert agent.context_plan['summary']['id'] == 'node-summary'
            assert len(agent.read_context(agent.context_plan['retrieved'][0])) == 2
            await agent.run('Factura R8'); assert len(calls) == 2
            source = agent.capture_context(3, 7)
            agent.set_context_plan({'summary': {'id': 'python-summary', 'source': agent.capture_context(1, 7), 'content': 'R7 viernes, R8 pagada'}, 'retrieved': [source]})
            agent.close()
        finally: store.close()
        inspect = f'''
import assert from 'node:assert/strict';
import {{Agent,SessionStore}} from {module};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});let calls=0;
const agent=new Agent({{model:'test',system:'System',store,agentId:'a',sessionId:'s',
tools:[{{name:'read',description:'Read',parameters:{{type:'object'}},execute:()=>assert.fail('no replay')}}],
provider:{{complete(request){{calls++;assert.equal(request.context.summary_id,'python-summary');
assert.equal(request.messages[3].tool_calls[0].arguments.n,1);return {{content:'final'}}}}}}}});
assert.equal(agent.contextPlan.summary.id,'python-summary');
const original=agent.readContext(agent.contextPlan.retrieved[0]);assert.equal(original.length,4);
assert.equal(original[1].tool_calls[0].arguments.n,1);assert.equal(original[2].tool_call_id,'c');
await agent.run('Final');assert.equal(calls,1);assert.equal(agent.messages.length,9);
agent.close();store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', inspect, directory], check=True, timeout=20)
    print('Summary sources, retrieval, integral floats and durable plans Node -> Python -> Node: OK')


asyncio.run(main())
