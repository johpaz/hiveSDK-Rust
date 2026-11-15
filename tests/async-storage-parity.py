"""Async public factories/maintenance share the same durable records across bindings."""
import asyncio
import json
from pathlib import Path
import subprocess
import tempfile
from hive_sdk import Agent, Objective, SessionStore

MODULE = json.dumps((Path(__file__).resolve().parent.parent / 'packages/npm/index.mjs').as_uri())
POLICY = {'max_model_calls': 2, 'max_output_tokens': 8, 'max_total_output_tokens': 16}


async def main():
    with tempfile.TemporaryDirectory() as directory:
        create = f'''
import assert from 'node:assert/strict';
import {{Agent,Objective,SessionStore,VerificationUncertain,TriggerUncertain}} from {MODULE};
const store=await SessionStore.open({{path:process.argv[1],tenantId:'parity'}});
const objective=await Objective.open({json.dumps(POLICY)},{{store,id:'root'}});
const agent=await Agent.open({{model:'test',store,objective,agentId:'a',sessionId:'s',
provider:{{complete:()=>({{content:'Known 🦀',usage:{{input_tokens:3,output_tokens:2}}}})}}}});
await agent.run('History');const source=agent.captureContext(0,2);
await agent.setContextPlanAsync({{summary:{{id:'s',source,content:'Known 🦀'}},retrieved:[]}});
await assert.rejects(objective.verify('v',{{receipt:'R7'}},()=>{{throw new Error('unknown');}}),VerificationUncertain);
const trigger=await objective.triggerAsync('event',{{kind:'event'}},()=>{{throw new Error('unknown event');}});await trigger.enableAsync();
await assert.rejects(trigger.emit('k',{{}}),TriggerUncertain);
assert.equal((await store.inspectAsync({{agentId:'a',sessionId:'s'}})).run.in_flight,null);
await store.closeAsync();
'''
        subprocess.run(['node', '--input-type=module', '-e', create, directory], check=True, timeout=30)
        store = await SessionStore.open(directory, tenant_id='parity')
        try:
            objective = await Objective.open(POLICY, store=store, objective_id='root')
            execution_id = (await objective.inspect_async())['verifications']['v']['execution_id']
            verdict = await objective.resolve_verification_async(key='v', execution_id=execution_id, verdict={'passed': True, 'evidence': {'language': 'python'}})
            assert verdict['verdict']['passed'] is True
            def no_replay(value, context): raise AssertionError('no replay of trigger callback')
            trigger = await objective.trigger_async('event', {'kind': 'event'}, no_replay)
            trigger_id = (await trigger.inspect_async())['executions']['k']['execution_id']
            await trigger.resolve_async(occurrence_key='k', execution_id=trigger_id, result='Known event')
            assert (await trigger.emit('k', {}))['result'] == 'Known event'
            class Provider:
                async def complete(self, request):
                    assert request['max_output_tokens'] == 8
                    assert request['context']['summary_id'] == 's'
                    return {'content': 'Answer 🦀', 'usage': {'input_tokens': 4, 'output_tokens': 3}}
            agent = await Agent.open(model='test', store=store, objective=objective, agent_id='a', session_id='s', provider=Provider())
            assert agent.context_plan['summary']['id'] == 's'
            assert (await agent.run('Question'))['content'] == 'Answer 🦀'
            assert len(agent.messages) == 4
        finally: await store.close_async()
        inspect = f'''
import assert from 'node:assert/strict';
import {{Agent,Objective,SessionStore}} from {MODULE};
const store=await SessionStore.open({{path:process.argv[1],tenantId:'parity'}});
const objective=await Objective.open({json.dumps(POLICY)},{{store,id:'root'}});
const snapshot=await objective.inspectAsync();assert.equal(snapshot.model_calls,2);assert.equal(snapshot.input_tokens,7);assert.equal(snapshot.output_tokens,5);
assert.equal(snapshot.verifications.v.verdict.passed,true);
const trigger=await objective.triggerAsync('event',{{kind:'event'}},()=>assert.fail('no replay'));
assert.equal((await trigger.emit('k',{{}})).result,'Known event');await trigger.disableAsync();assert.equal((await trigger.inspectAsync()).enabled,false);
assert.equal((await objective.inspectAsync()).tool_calls,2);
const agent=await Agent.open({{model:'test',store,objective,agentId:'a',sessionId:'s',provider:{{complete:()=>assert.fail('no extra call')}}}});
assert.equal(agent.messages.at(-1).content,'Answer 🦀');assert.equal(agent.contextPlan.summary.id,'s');
assert.equal((await store.inspectAsync({{agentId:'a',sessionId:'s'}})).run.in_flight,null);await store.closeAsync();
'''
        subprocess.run(['node', '--input-type=module', '-e', inspect, directory], check=True, timeout=30)
    print('Async factories, context plan, fenced verification/trigger and usage Node -> Python -> Node: OK')


asyncio.run(main())
