"""Structured policy survives Node -> Python -> Node without replaying a tool."""
import asyncio
import json
from pathlib import Path
import subprocess
import tempfile
from hive_sdk import Agent, SessionStore, Tool

MODULE = json.dumps((Path(__file__).resolve().parent.parent / 'packages/npm/index.mjs').as_uri())
OUTPUT = {'name': 'answer_v1', 'schema': {'type': 'object', 'properties': {'answer': {'type': 'string'}}, 'required': ['answer'], 'additionalProperties': False}}
POLICY = {'structured_output': OUTPUT, 'max_output_tokens': 8, 'max_total_output_tokens': 10}
TEXT = ' \n{"answer":"sí 🦀"}\t'


async def main():
    with tempfile.TemporaryDirectory() as directory:
        create = f'''
import assert from 'node:assert/strict';
import {{Agent,SessionStore}} from {MODULE};
const store=new SessionStore({{path:process.argv[1]}});let models=0;let tools=0;
const agent=new Agent({{model:'test',policy:{json.dumps(POLICY)},store,agentId:'a',sessionId:'s',
tools:[{{name:'read',description:'Read',parameters:{{type:'object'}},execute:()=>{{tools++;return 5;}}}}],
provider:{{complete(request){{assert.deepEqual(request.structured_output,{json.dumps(OUTPUT)});
return {{content:'Reading',tool_calls:[{{id:'c',name:'read',arguments:{{}}}}],usage:{{input_tokens:3,output_tokens:2}}}};}}}},
onEvent(action){{if(action.type==='model'&&++models===2)throw new Error('pause after known tool');}}}});
await assert.rejects(agent.run('Answer'),/pause after known tool/);assert.equal(tools,1);
const run=store.inspect({{agentId:'a',sessionId:'s'}}).run;assert.equal(run.in_flight,null);
assert.equal(run.state.stats.model_calls,1);agent.close();store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', create, directory], check=True, timeout=20)
        store = SessionStore(directory)
        class Provider:
            async def complete(self, request):
                assert request['structured_output'] == OUTPUT
                assert request['max_output_tokens'] == 8
                assert request['messages'][-1]['content'] == '5'
                return {'content': TEXT, 'usage': {'input_tokens': 4, 'output_tokens': 3}}
        def no_replay(args): raise AssertionError('known tool must not replay')
        config = dict(model='test', store=store, agent_id='a', session_id='s', provider=Provider(),
                      tools=[Tool('read', 'Read', {'type': 'object'}, no_replay)])
        try:
            altered = dict(POLICY, structured_output=dict(OUTPUT, name='different'))
            try: Agent(**config, policy=altered, resume=True)
            except RuntimeError as error: assert 'SESSION_CONFIG_MISMATCH' in str(error)
            else: raise AssertionError('changed schema policy must not reopen')
            agent = Agent(**config, policy=POLICY, resume=True)
            result = await agent.resume()
            assert result['status'] == 'completed' and result['content'] == TEXT
            assert result['stats']['model_calls'] == 2 and result['stats']['tool_calls'] == 1
            assert result['stats']['input_tokens'] == 7 and result['stats']['output_tokens'] == 5
            agent.close()
        finally: store.close()
        inspect = f'''
import assert from 'node:assert/strict';
import {{Agent,SessionStore}} from {MODULE};
const store=new SessionStore({{path:process.argv[1]}});
const agent=new Agent({{model:'test',policy:{json.dumps(POLICY)},store,agentId:'a',sessionId:'s',
tools:[{{name:'read',description:'Read',parameters:{{type:'object'}},execute:()=>assert.fail('no replay')}}],
provider:{{complete:()=>assert.fail('no extra model')}}}});
assert.equal(agent.messages.at(-1).content,{json.dumps(TEXT)});assert.equal(agent.messages.length,4);
const run=store.inspect({{agentId:'a',sessionId:'s'}}).run;
assert.equal(run.in_flight,null);assert.equal(run.state.action.type,'complete');
assert.equal(run.state.stats.model_calls,2);assert.equal(run.state.stats.output_tokens,5);agent.close();store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', inspect, directory], check=True, timeout=20)
    print('Structured schema, final raw JSON and known tool Node -> Python -> Node: OK')


asyncio.run(main())
