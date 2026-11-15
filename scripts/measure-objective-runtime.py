"""Local counterpart to measure-objective-runtime.mjs; no remote models or billing."""
import asyncio
import json
import math
from pathlib import Path
import sys
import tempfile
import time
from hive_sdk import Agent, Objective, SessionStore, Tool

USAGE = {'input_tokens': 3, 'output_tokens': 2}


async def measure(concurrency, history_chars):
    with tempfile.TemporaryDirectory() as directory:
        store = await SessionStore.open(directory); pulse = None
        try:
            objective = await Objective.open({'max_model_calls': 128, 'max_tool_calls': 64, 'max_output_tokens': 8, 'max_total_output_tokens': 256}, store=store, objective_id='root')
            agents, inputs = [], []
            class Provider:
                async def complete(self, request):
                    last = request['messages'][-1]
                    if last['role'] == 'user' and last['content'].startswith('Task'):
                        return {'tool_calls': [{'id': 'c', 'name': 'read', 'arguments': {'task': last['content']}}], 'usage': USAGE}
                    return {'content': 'Known' if last['role'] == 'tool' else 'x' * history_chars, 'usage': USAGE}
            async def read(args): return {'known': args['task']}
            for i in range(concurrency):
                agent = await Agent.open(model='fixture', store=store, objective=objective, agent_id=f'a{i}', session_id='s',
                    policy={'max_context_bytes': 16384, 'context_overflow': 'drop_oldest_turns'}, provider=Provider(), tools=[Tool('read', 'Read', {'type': 'object'}, read)])
                for j in range(4): await agent.run(f'History {j}: ' + 'h' * history_chars)
                source = agent.capture_context(0, 2); inputs.append({'source': source, 'messages': agent.read_context(source)}); agents.append(agent)
            async def event_callback(value, context): return value
            async def verdict(value, context): return {'passed': True}
            async def summary(value, context): return 'Known'
            triggers = []
            for i in range(concurrency):
                event = await objective.trigger_async(f'events{i}', {'kind': 'event'}, event_callback); await event.enable_async(); triggers.append(event)
            before = await objective.inspect_async(); delays = []
            async def monitor():
                previous = time.perf_counter()
                while True:
                    await asyncio.sleep(.01); now = time.perf_counter(); delays.append(max(0, (now - previous) * 1000 - 10)); previous = now
            async def tasks(agent, i):
                for j in range(4): await agent.run(f'Task {i}/{j}')
            async def verifications(i):
                for j in range(2): await objective.verify(f'v{i}/{j}', {'known': True}, verdict)
            async def events(i):
                for j in range(2): await triggers[i].emit(f'e{i}/{j}', {'known': True})
            pulse = asyncio.create_task(monitor()); start = time.perf_counter()
            outcomes = await asyncio.gather(*(task for i, agent in enumerate(agents) for task in (tasks(agent, i), verifications(i), events(i), objective.summarize(f's{i}', inputs[i], summary))), return_exceptions=True)
            failure = next((value for value in outcomes if isinstance(value, BaseException)), None)
            if failure is not None: raise failure
            after = await objective.inspect_async(); elapsed = (time.perf_counter() - start) * 1000
            pulse.cancel(); await asyncio.gather(pulse, return_exceptions=True); pulse = None; delays.sort()
            usage = {key: after[key] - before[key] for key in ('model_calls', 'tool_calls', 'input_tokens', 'output_tokens')}
            assert usage['model_calls'] == concurrency * 8 and usage['tool_calls'] == concurrency * 9 and after['reservations'] == {}
            def percentile(q): return delays[max(0, math.ceil(q * len(delays)) - 1)] if delays else None
            return {'concurrency': concurrency, 'history_turns_per_agent': 4, 'history_chars_per_message': history_chars,
                'tasks_per_agent': 4, 'context_max_bytes': 16384, 'interval_ms': 10, 'elapsed_ms': elapsed, 'samples': len(delays),
                'observed_lag_ms': {'p50': percentile(.5), 'p95': percentile(.95), 'max': delays[-1] if delays else None},
                'warmup_model_calls': before['model_calls'], 'measured_synthetic_usage': usage, 'total_model_calls': after['model_calls']}
        finally:
            if pulse is not None: pulse.cancel(); await asyncio.gather(pulse, return_exceptions=True)
            await store.close_async()


async def main():
    cases = [await measure(1, 1024), await measure(4, 4096)]
    report = {'runtime': sys.version.split()[0], 'scope': 'Observed local event-loop lag with actual HiveDB and concurrent SDK control; synthetic callbacks/usage, no remote models. Not a production latency bound or billing estimate. Timings vary by machine/load.', 'cases': cases}
    destination = Path(__file__).resolve().parent.parent / 'evals/objective-runtime-python.json'
    destination.parent.mkdir(exist_ok=True); destination.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(cases))


asyncio.run(main())
