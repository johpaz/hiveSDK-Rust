"""Node checkpoint -> Python continuation -> Node inspection of the same root ledger."""
import asyncio
import json
from pathlib import Path
import subprocess
import tempfile

from hive_sdk import Agent, Objective, SessionStore, Tool, VerificationUncertain, TriggerUncertain

root = Path(__file__).resolve().parent.parent
module = json.dumps((root / 'packages/npm/index.mjs').as_uri())
policy = {'max_model_calls': 2, 'max_output_tokens': 8, 'max_total_output_tokens': 10, 'max_depth': 1}


async def main():
    with tempfile.TemporaryDirectory() as directory:
        create = f'''
import assert from 'node:assert/strict';
import {{Agent,Objective,SessionStore}} from {module};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const objective=new Objective({json.dumps(policy)},{{store,id:'root'}});
let calls=0;
const agent=new Agent({{model:'test',store,objective,agentId:'a',sessionId:'s',
  tools:[{{name:'work',description:'Work',parameters:{{type:'object'}},execute:()=>5}}],
  provider:{{complete(){{calls++;return {{content:null,tool_calls:[{{id:'c',name:'work',arguments:{{}}}}],usage:{{input_tokens:2,output_tokens:3}}}}}}}},
  onEvent(action){{if(action.type==='model'&&calls)throw new Error('pause')}}}});
await assert.rejects(agent.run('Work'),/pause/);
assert.equal(objective.inspect().model_calls,1);
assert.equal(objective.inspect().tool_calls,1);
agent.close();store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', create, directory], check=True, timeout=20)
        store = SessionStore(directory, tenant_id='parity')
        try:
            objective = Objective(policy, store=store, objective_id='root')
            assert objective.inspect()['model_calls'] == 1
            class Provider:
                async def complete(self, request):
                    assert request['max_output_tokens'] == 7
                    assert request['messages'][-1]['role'] == 'tool'
                    return {'content': 'from Python', 'usage': {'input_tokens': 4, 'output_tokens': 2}}
            def no_replay(args): raise AssertionError('tool must not replay')
            agent = Agent(model='test', store=store, objective=objective, agent_id='a', session_id='s', resume=True,
                provider=Provider(), tools=[Tool(name='work', description='Work', parameters={'type': 'object'}, execute=no_replay)])
            assert (await agent.resume())['content'] == 'from Python'
            agent.close()
        finally: store.close()
        inspect = f'''
import assert from 'node:assert/strict';
import {{Agent,Objective,SessionStore}} from {module};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const objective=new Objective({json.dumps(policy)},{{store,id:'root'}});
const view=objective.inspect();
assert.equal(view.model_calls,2);assert.equal(view.tool_calls,1);assert.equal(view.output_tokens,5);assert.equal(view.input_tokens,6);
assert.deepEqual(view.reservations,{{}});
const agent=new Agent({{model:'test',store,objective,agentId:'a',sessionId:'s',
  tools:[{{name:'work',description:'Work',parameters:{{type:'object'}},execute:()=>assert.fail('no replay')}}],
  provider:{{complete:()=>assert.fail('no inference')}}}});
assert.equal(agent.messages.at(-1).content,'from Python');agent.close();store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', inspect, directory], check=True, timeout=20)
        facts = {'receipt': 'r1', 'value': 5, 'note': 'é😀\n"'}
        create_verification = f'''
import assert from 'node:assert/strict';
import {{Objective,SessionStore}} from {module};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const objective=new Objective({json.dumps(policy)},{{store,id:'root'}});
const result=await objective.verify('node-proof-v1',{json.dumps(facts)},()=>({{passed:true,evidence:{{receipt:'r1'}}}}));
assert.equal(result.verdict.passed,true);assert.equal(objective.inspect().model_calls,2);store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', create_verification, directory], check=True, timeout=20)
        store = SessionStore(directory, tenant_id='parity')
        try:
            objective = Objective(policy, store=store, objective_id='root')
            def no_replay(value, context): raise AssertionError('verification must not replay')
            reordered = {key: facts[key] for key in reversed(facts)}
            assert (await objective.verify('node-proof-v1', reordered, no_replay))['verdict']['passed']
            async def interrupted(value, context): raise RuntimeError('verification interrupted')
            try:
                await objective.verify('python-proof-v1', {'receipt': 'absent'}, interrupted)
                raise AssertionError('must remain uncertain')
            except VerificationUncertain:
                pass
            execution_id = objective.inspect()['verifications']['python-proof-v1']['execution_id']
            objective.resolve_verification(key='python-proof-v1', execution_id=execution_id, verdict={'passed': False, 'reason': 'Receipt absent'})
        finally: store.close()
        inspect_verification = f'''
import assert from 'node:assert/strict';
import {{Objective,SessionStore}} from {module};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const objective=new Objective({json.dumps(policy)},{{store,id:'root'}});
const result=await objective.verify('python-proof-v1',{{receipt:'absent'}},()=>assert.fail('no replay'));
assert.equal(result.verdict.passed,false);assert.equal(result.verdict.reason,'Receipt absent');
assert.equal(objective.inspect().tool_calls,3);assert.equal(objective.inspect().model_calls,2);store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', inspect_verification, directory], check=True, timeout=20)
        create_triggers = f'''
import assert from 'node:assert/strict';
import {{writeFileSync}} from 'node:fs';
import {{join}} from 'node:path';
import {{Objective,SessionStore}} from {module};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const objective=new Objective({json.dumps(policy)},{{store,id:'root'}});
const event=objective.trigger('receipts',{{kind:'event'}},()=>null);event.enable();
assert.equal((await event.emit('known',{json.dumps(facts)})).result,null);
const interval=objective.trigger('interval',{{kind:'interval',everyMs:60000}},()=>assert.fail('not due'));
interval.enable();writeFileSync(join(process.argv[1],'trigger-calendar.json'),JSON.stringify(interval.inspect()));
const pending=objective.trigger('pending',{{kind:'event'}},()=>{{writeFileSync(join(process.argv[1],'trigger-effect'),'confirmed');process.exit(19)}});
pending.enable();await pending.emit('crashed',{{receipt:'r2'}});
'''
        child = subprocess.run(['node', '--input-type=module', '-e', create_triggers, directory], timeout=20)
        assert child.returncode == 19
        assert Path(directory, 'trigger-effect').read_text() == 'confirmed'
        store = SessionStore(directory, tenant_id='parity')
        try:
            objective = Objective(policy, store=store, objective_id='root')
            def fail_trigger(value, context): raise AssertionError('trigger must not replay')
            event = objective.trigger('receipts', {'kind': 'event'}, fail_trigger)
            assert event.inspect()['enabled']
            assert (await event.emit('known', reordered))['result'] is None
            interval = objective.trigger('interval', {'kind': 'interval', 'every_ms': 60_000}, fail_trigger)
            assert interval.inspect() == json.loads(Path(directory, 'trigger-calendar.json').read_text())
            pending = objective.trigger('pending', {'kind': 'event'}, fail_trigger)
            try:
                await pending.emit('crashed', {'receipt': 'r2'})
                raise AssertionError('pending occurrence must remain uncertain')
            except TriggerUncertain:
                pass
            execution_id = pending.inspect()['executions']['crashed']['execution_id']
            pending.resolve(occurrence_key='crashed', execution_id=execution_id, result={'receipt': 'r2'})
        finally: store.close()
        inspect_triggers = f'''
import assert from 'node:assert/strict';
import {{Objective,SessionStore}} from {module};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const objective=new Objective({json.dumps(policy)},{{store,id:'root'}});
const pending=objective.trigger('pending',{{kind:'event'}},()=>assert.fail('no replay'));
assert.deepEqual((await pending.emit('crashed',{{receipt:'r2'}})).result,{{receipt:'r2'}});
assert.equal(objective.inspect().tool_calls,5);assert.equal(objective.inspect().model_calls,2);store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', inspect_triggers, directory], check=True, timeout=20)
    print('Durable objective, checkpoint, verification and trigger recovery Node -> Python -> Node: OK')


asyncio.run(main())
