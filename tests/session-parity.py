"""Prove a Node-created session can continue in Python and reopen again in Node."""
import asyncio
import json
from pathlib import Path
import subprocess
import tempfile

from hive_sdk import Agent, SessionStore, Tool

root = Path(__file__).resolve().parent.parent
module_url = (root / "packages/npm/index.mjs").as_uri()


def node(directory, reopen=False):
    script = f"""
import assert from 'node:assert/strict';
import {{ Agent, SessionStore }} from {json.dumps(module_url)};
const store = new SessionStore({{ path: process.argv[1], tenantId: 'parity' }});
try {{
  const agent = new Agent({{ model: 'test', system: 'Same contract', store,
    agentId: 'agent', sessionId: 'session', provider: {{ complete: () => ({{content:'From Node'}}) }} }});
  if (process.argv[2] === 'reopen') {{
    assert.equal(agent.messages.length, 5);
    assert.equal(agent.messages.at(-1).content, 'From Python');
  }} else {{ await agent.run('Hello from Node'); }}
  agent.close();
}} finally {{ store.close(); }}
"""
    subprocess.run(["node", "--input-type=module", "-e", script, directory,
                    "reopen" if reopen else "create"], check=True)


async def main():
    class Provider:
        async def complete(self, request):
            assert request["messages"][-2]["content"] == "From Node"
            return {"content": "From Python"}

    with tempfile.TemporaryDirectory() as directory:
        node(directory)
        store = SessionStore(directory, tenant_id="parity")
        try:
            agent = Agent(model="test", system="Same contract", store=store,
                          agent_id="agent", session_id="session", provider=Provider())
            assert len(agent.messages) == 3
            result = await agent.run("Continue from Python")
            assert result["content"] == "From Python"
            agent.close()
        finally:
            store.close()
        node(directory, reopen=True)
        script = f"""
import assert from 'node:assert/strict';
import {{ Agent, SessionStore }} from {json.dumps(module_url)};
const store = new SessionStore({{path:process.argv[1],tenantId:'parity'}});
let events=0;
try {{
  const agent = new Agent({{model:'test',system:'Same contract',store,agentId:'agent',sessionId:'checkpoint',
    tools:[{{name:'lookup',description:'Lookup',parameters:{{type:'object'}},execute:()=>42}}],
    provider:{{complete:()=>({{content:null,tool_calls:[{{id:'c',name:'lookup',arguments:{{}}}}],
      usage:{{input_tokens:3,output_tokens:2}}}})}},
    onEvent(action) {{if(action.type==='model' && ++events===2) throw new Error('checkpoint-interruption');}}
  }});
  await assert.rejects(agent.run('Lookup'),/checkpoint-interruption/);
  assert.equal(store.inspect({{agentId:'agent',sessionId:'checkpoint'}}).run.in_flight,null);
}} finally {{store.close();}}
"""
        subprocess.run(["node", "--input-type=module", "-e", script, directory], check=True, timeout=15)
        store = SessionStore(directory, tenant_id="parity")
        replays = 0

        def lookup(args):
            nonlocal replays
            replays += 1
            return 0

        class Continued:
            async def complete(self, request):
                assert request["messages"][-1]["content"] == "42"
                return {"content": "42", "usage": {"input_tokens": 4, "output_tokens": 1}}

        try:
            agent = Agent(model="test", system="Same contract", store=store, agent_id="agent",
                session_id="checkpoint", tools=[Tool("lookup", "Lookup", {"type": "object"}, lookup)],
                provider=Continued(), resume=True)
            result = await agent.resume()
            assert result["stats"]["model_calls"] == 2
            assert result["stats"]["input_tokens"] == 7
            assert result["stats"]["tool_calls"] == 1
            assert replays == 0
            agent.close()
        finally:
            store.close()
        script = f"""
import assert from 'node:assert/strict';
import {{ Agent, SessionStore }} from {json.dumps(module_url)};
const store = new SessionStore({{path:process.argv[1],tenantId:'parity'}});
let interrupt=false;
try {{
  const agent = new Agent({{model:'test',system:'System',store,agentId:'agent',sessionId:'bounded',
    policy:{{max_context_bytes:286,context_overflow:'drop_oldest_turns',
      input_token_budget:{{encoding:'cl100k_base',max_tokens:160,overhead_tokens:7}}}},
    provider:{{complete:()=>({{content:'finished'}})}},
    onEvent(action) {{if(interrupt&&action.type==='model') throw new Error('bounded-interruption');}}
  }});
  await agent.run('old'.repeat(60)); interrupt=true;
  await assert.rejects(agent.run('current'.repeat(20)),/bounded-interruption/);
  assert.equal(store.inspect({{agentId:'agent',sessionId:'bounded'}}).run.state.action.request.context.dropped_messages,2);
}} finally {{store.close();}}
"""
        subprocess.run(["node", "--input-type=module", "-e", script, directory], check=True, timeout=15)
        store = SessionStore(directory, tenant_id="parity")
        expected_context = store.inspect(agent_id="agent", session_id="bounded")["run"]["state"]["action"]["request"]["context"]
        assert expected_context["token_estimate"]["total_tokens"] <= 160
        class Bounded:
            async def complete(self, request):
                assert request["context"] == expected_context
                assert request["context"]["serialized_bytes"] == 246
                assert request["context"]["dropped_messages"] == 2
                assert [m["content"] for m in request["messages"]] == ["System", "current" * 20]
                return {"content":"From Python"}
        try:
            agent = Agent(model="test", system="System", store=store, agent_id="agent", session_id="bounded",
                policy={"max_context_bytes":286,"context_overflow":"drop_oldest_turns",
                        "input_token_budget":{"encoding":"cl100k_base","max_tokens":160,"overhead_tokens":7}},
                provider=Bounded(), resume=True)
            assert (await agent.resume())["status"] == "completed"
            assert len(agent.messages) == 5
            assert agent.messages[1]["content"] == "old" * 60
        finally:
            store.close()
        signed_parts = [{"text":"","thoughtSignature":"text-sig"},
                        {"functionCall":{"id":"signed-c","name":"lookup","args":{}},"thoughtSignature":"tool-sig"}]
        script = f"""
import assert from 'node:assert/strict';
import {{ Agent, SessionStore }} from {json.dumps(module_url)};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
let models=0;
try {{
  const agent=new Agent({{model:'test',system:'Same contract',store,agentId:'agent',sessionId:'signed',
    policy:{{max_output_tokens:4,max_total_output_tokens:5}},
    tools:[{{name:'lookup',description:'Lookup',parameters:{{type:'object'}},execute:()=>42}}],
    provider:{{complete:()=>({{content:'',tool_calls:[{{id:'signed-c',name:'lookup',arguments:{{}}}}],
      provider_state:{{provider:'gemini',version:1,parts:{json.dumps(signed_parts)}}},usage:{{input_tokens:10,output_tokens:2}}}})}},
    onEvent(action){{if(action.type==='model'&&++models===2)throw new Error('signed-interruption');}}
  }});
  await assert.rejects(agent.run('Lookup'),/signed-interruption/);
}} finally {{store.close();}}
"""
        subprocess.run(["node","--input-type=module","-e",script,directory],check=True,timeout=15)
        store = SessionStore(directory, tenant_id="parity")
        class Signed:
            async def complete(self, request):
                assert request["messages"][-2]["provider_state"]["parts"] == signed_parts
                assert request["max_output_tokens"] == 3
                return {"content":"done","provider_state":{"provider":"gemini","version":1,
                    "parts":[{"text":"done","thoughtSignature":"tail"}]},"usage":{"input_tokens":5,"output_tokens":1}}
        def no_replay(args):
            raise AssertionError("known tool must not run again")
        try:
            agent=Agent(model="test",system="Same contract",store=store,agent_id="agent",session_id="signed",
                policy={"max_output_tokens":4,"max_total_output_tokens":5},tools=[Tool("lookup","Lookup",{"type":"object"},no_replay)],
                provider=Signed(),resume=True)
            result=await agent.resume()
            assert result["stats"]["input_tokens"] == 15 and result["stats"]["output_tokens"] == 3
            assert agent.messages[-1]["provider_state"]["parts"][0]["thoughtSignature"] == "tail"
        finally:
            store.close()
        script=f"""
import assert from 'node:assert/strict';
import {{Agent,SessionStore}} from {json.dumps(module_url)};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
try {{
  const agent=new Agent({{model:'test',system:'Same contract',store,agentId:'agent',sessionId:'signed',
    policy:{{max_output_tokens:4,max_total_output_tokens:5}},
    tools:[{{name:'lookup',description:'Lookup',parameters:{{type:'object'}},execute:()=>assert.fail('no replay')}}],
    provider:{{complete:()=>assert.fail('no new model call')}}}});
  assert.deepEqual(agent.messages[2].provider_state.parts,{json.dumps(signed_parts)});
  assert.equal(agent.messages.at(-1).provider_state.parts[0].thoughtSignature,'tail');
}}finally{{store.close();}}
"""
        subprocess.run(["node","--input-type=module","-e",script,directory],check=True,timeout=15)
    await input_budget_parity()
    print("Node/Python history, tools, bounded context, input budget and signed Gemini checkpoints: OK")


async def input_budget_parity():
    with tempfile.TemporaryDirectory() as directory:
        script = f'''
import assert from 'node:assert/strict';
import {{writeFileSync}} from 'node:fs';
import {{join}} from 'node:path';
import {{Agent,SessionStore}} from {json.dumps(module_url)};
const tools=[{{name:'read',description:'Read',parameters:{{type:'object'}},execute:()=>assert.fail('paused before effect')}}];
const policy={{max_output_tokens:10,input_token_budget:{{encoding:'cl100k_base',max_tokens:1000,framing:'openai_chat_json',context_window_tokens:1000}}}};
let estimate;
const probe=new Agent({{model:'test',tools,policy,provider:{{complete(request){{estimate=request.context.token_estimate.total_tokens;return {{content:'done'}}}}}}}});
await probe.run('Read');probe.close();policy.input_token_budget.max_total_tokens=estimate;
writeFileSync(join(process.argv[1],'policy.json'),JSON.stringify(policy));
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const agent=new Agent({{model:'test',tools,policy,store,agentId:'a',sessionId:'s',
provider:{{complete(){{return {{content:null,tool_calls:[{{id:'c',name:'read',arguments:{{}}}}],usage:{{input_tokens:1,output_tokens:1}}}}}}}},
onEvent(action){{if(action.type==='tool')throw new Error('pause')}}}});
await assert.rejects(agent.run('Read'),/pause/);agent.close();store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', script, directory], check=True, timeout=20)
        policy = json.loads(Path(directory, 'policy.json').read_text())
        store = SessionStore(directory, tenant_id='parity'); effects = []
        class Provider:
            async def complete(self, request): raise AssertionError('no new paid continuation')
        def execute(args): effects.append(True); return 5
        try:
            agent = Agent(model='test', policy=policy, store=store, agent_id='a', session_id='s', resume=True,
                provider=Provider(), tools=[Tool('read', 'Read', {'type': 'object'}, execute)])
            result = await agent.resume(); agent.close()
            assert result['stop_reason'] == 'input_limit' and len(effects) == 1
            assert result['stats']['charged_input_tokens'] == policy['input_token_budget']['max_total_tokens']
            assert result['stats']['estimated_input_tokens'] == result['stats']['charged_input_tokens']
            assert result['stats']['input_tokens'] == 1 and result['stats']['reserved_input_tokens'] == 0
        finally: store.close()
        inspect = f'''
import assert from 'node:assert/strict';
import {{SessionStore}} from {json.dumps(module_url)};
const store=new SessionStore({{path:process.argv[1],tenantId:'parity'}});
const run=store.inspect({{agentId:'a',sessionId:'s'}}).run;
assert.equal(run.state.action.reason,'input_limit');assert.equal(run.tools[0].status,'completed');
assert.equal(run.state.stats.charged_input_tokens,{policy['input_token_budget']['max_total_tokens']});
assert.equal(run.state.stats.model_calls,1);assert.equal(run.state.stats.tool_calls,1);store.close();
'''
        subprocess.run(['node', '--input-type=module', '-e', inspect, directory], check=True, timeout=20)


if __name__ == "__main__":
    asyncio.run(main())
