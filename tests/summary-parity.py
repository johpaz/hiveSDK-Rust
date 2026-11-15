"""Node summary child checkpoint -> Python continuation -> Node cached receipt."""
import asyncio
import json
from pathlib import Path
import subprocess
import tempfile
from hive_sdk import Agent, Objective, SessionStore, SummaryUncertain, Tool

module = json.dumps((Path(__file__).resolve().parent.parent / 'packages/npm/index.mjs').as_uri())
policy = {'max_model_calls': 2, 'max_output_tokens': 8, 'max_total_output_tokens': 10}


async def main():
    with tempfile.TemporaryDirectory() as directory:
        create = f'''
import assert from 'node:assert/strict';
import {{Agent,Objective,SessionStore,SummaryUncertain}} from {module};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const objective=new Objective({json.dumps(policy)},{{store,id:'root'}});
const subject=new Agent({{model:'source',store,agentId:'subject',sessionId:'s',provider:{{complete:()=>({{content:'R7 viernes'}})}}}});
await subject.run('Pedido R7');let calls=0;
const worker=new Agent({{model:'summary',store,objective,agentId:'worker',sessionId:'s',
tools:[{{name:'read',description:'Read',parameters:{{type:'object'}},execute:()=>5}}],
provider:{{complete(){{calls++;return {{content:null,tool_calls:[{{id:'c',name:'read',arguments:{{}}}}],usage:{{input_tokens:2,output_tokens:3}}}}}}}},
onEvent(action){{if(action.type==='model'&&calls)throw new Error('pause')}}}});
await assert.rejects(subject.summarize('summary-v1',subject.captureContext(0,2),async(_,ctx)=>(await ctx.delegate(worker,'Summarize')).content,{{objective}}),SummaryUncertain);
assert.equal(store.inspect({{agentId:'worker',sessionId:'s'}}).run.summary_key,'summary-v1');
assert.equal(objective.inspect().summary_costs['summary-v1'].model_calls,1);
worker.close();subject.close();store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', create, directory], check=True, timeout=20)
        store = SessionStore(directory, tenant_id='parity'); objective = Objective(policy, store=store, objective_id='root'); calls = []
        class Provider:
            async def complete(self, request):
                calls.append(True); assert request['max_output_tokens'] == 7
                assert request['messages'][-1]['role'] == 'tool'
                return {'content': 'R7 viernes', 'usage': {'input_tokens': 4, 'output_tokens': 2}}
        def fail(args): raise AssertionError('known tool must not replay')
        class Source:
            async def complete(self, request): raise AssertionError('subject must not infer')
        try:
            worker = Agent(model='summary', store=store, objective=objective, agent_id='worker', session_id='s', resume=True,
                tools=[Tool('read', 'Read', {'type': 'object'}, fail)], provider=Provider())
            result = await worker.resume(); worker.close(); assert len(calls) == 1
            objective.resolve_summary(key='summary-v1', execution_id=objective.inspect()['summaries']['summary-v1']['execution_id'], content=result['content'])
            subject = Agent(model='source', store=store, agent_id='subject', session_id='s', provider=Source())
            def no_replay(value, context): raise AssertionError('summary callback must not replay')
            receipt = await subject.summarize('summary-v1', subject.capture_context(0, 2), no_replay, objective=objective)
            assert (receipt['cost']['model_calls'], receipt['cost']['tool_calls'], receipt['cost']['input_tokens'], receipt['cost']['output_tokens']) == (2, 2, 6, 5)
            subject.set_context_plan({'summary': receipt['summary']}); subject.close()
        finally: store.close()
        inspect = f'''
import assert from 'node:assert/strict';
import {{Agent,Objective,SessionStore}} from {module};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const objective=new Objective({json.dumps(policy)},{{store,id:'root'}});
const subject=new Agent({{model:'source',store,agentId:'subject',sessionId:'s',provider:{{complete:()=>assert.fail('no inference')}}}});
const receipt=await subject.summarize('summary-v1',subject.captureContext(0,2),()=>assert.fail('no callback replay'),{{objective}});
assert.equal(receipt.cost.model_calls,2);assert.equal(receipt.cost.tool_calls,2);
assert.equal(receipt.cost.input_tokens,6);assert.equal(receipt.cost.output_tokens,5);assert.equal(receipt.cost.reserved_output_tokens,0);
assert.equal(subject.contextPlan.summary.id,'summary-v1');assert.equal(objective.inspect().model_calls,2);
subject.close();store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', inspect, directory], check=True, timeout=20)
    print('Summary attribution, remaining cap and child checkpoint Node -> Python -> Node: OK')


asyncio.run(main())
