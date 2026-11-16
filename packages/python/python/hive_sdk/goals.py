"""Drive a goal with the host's agents.

The SDK still runs no model on its own: ``agent_for`` supplies the agent of every job,
and this loop only claims jobs, runs them through ``context.run_agent`` (which charges
the root objective) and ticks the goal.
"""
from __future__ import annotations

import asyncio
import json
import re
from typing import Any, Awaitable, Callable

_TERMINAL = {'accepted', 'failed', 'cancelled'}
_JSON_STAGES = {'plan', 'review'}


def extract_json(text: Any) -> dict | None:
    """The JSON object in a model reply: bare, fenced, or surrounded by prose."""
    if not isinstance(text, str):
        return None
    attempts = [text.strip()]
    fenced = re.search(r'```(?:json)?\s*([\s\S]*?)```', text, re.IGNORECASE)
    if fenced:
        attempts.append(fenced.group(1).strip())
    first, last = text.find('{'), text.rfind('}')
    if 0 <= first < last:
        attempts.append(text[first:last + 1])
    for attempt in attempts:
        try:
            value = json.loads(attempt)
        except ValueError:
            continue
        if isinstance(value, dict):
            return value
    return None


def _executor(agent_for):
    async def run(job, context):
        payload = job['input']['payload']
        binding = payload.get('delegation') or payload['goal']
        stage = binding['stage']
        agent = agent_for({'agent_id': binding['agent_id'], 'session_id': binding['session_id'], 'stage': stage,
                           'round': binding.get('round'), 'task_id': binding.get('task_id'), 'job': job})
        if asyncio.iscoroutine(agent):
            agent = await agent
        try:
            result = await context.run_agent(agent, payload['prompt'])
            if result.get('status') != 'completed' or result.get('content') is None:
                return {'type': 'failed', 'error': f"the agent stopped without an answer ({result.get('stop_reason') or 'no reply'})", 'retryable': False}
            if stage not in _JSON_STAGES:
                return {'type': 'completed', 'result': {'text': result['content']}}
            parsed = extract_json(result['content'])
            if parsed is None:
                return {'type': 'failed', 'error': f'the {stage} answer was not a JSON object', 'retryable': False}
            return {'type': 'completed', 'result': parsed}
        finally:
            close = getattr(agent, 'close', None)
            if close:
                close()
    return run


async def run_goal(queue, goal_id: str, *, agent_for: Callable[[dict], Any | Awaitable[Any]],
                   cancel_event: asyncio.Event | None = None, max_steps: int = 200) -> dict:
    """Run a goal until it is accepted, failed or cancelled, or nothing can run.

    That happens when it waits on jobs other workers own, or a job's outcome is
    uncertain (resolve it with ``queue.reconcile``; it is never replayed). Returns the
    last goal report.
    """
    if not callable(agent_for):
        raise TypeError('run_goal requires an agent_for function')
    for _ in range(max_steps):
        if cancel_event is not None and cancel_event.is_set():
            return await queue.goal(goal_id)
        report = await queue.tick_goal(goal_id)
        if report['status'] in _TERMINAL or report['blocked_uncertain']:
            return report
        run = _executor(agent_for)
        executors = {report['kinds']['plan']: run, report['kinds']['review']: run}
        for entry in report['kinds']['rounds']:
            executors[entry['specialist']] = run
            executors[entry['synthesis']] = run
        if not await queue.run_once(executors, cancel_event=cancel_event):
            return report
    return await queue.tick_goal(goal_id)
