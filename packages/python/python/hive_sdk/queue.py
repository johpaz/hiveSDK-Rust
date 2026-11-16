"""Rust durable queue with explicitly driven Python executor callbacks."""
from __future__ import annotations
import asyncio
import copy
import inspect
import uuid
from concurrent.futures import ThreadPoolExecutor
from ._storage import _join
from typing import Any, Awaitable, Callable, Literal, TypedDict


class SpecialistTask(TypedDict):
    id: str
    specialist_id: str
    prompt: str


class _GroupRequired(TypedDict):
    objective_id: str
    coordinator_agent_id: str
    tasks: list[SpecialistTask]


class GroupSpec(_GroupRequired, total=False):
    depth: int
    require_verification: bool


class _GoalRequired(TypedDict):
    objective_id: str
    goal: str
    planner_agent_id: str
    coordinator_agent_id: str


class GoalSpec(_GoalRequired, total=False):
    reviewer_agent_id: str | None
    specialist_ids: list[str]
    max_rounds: int
    max_tasks_per_round: int
    depth: int
    require_verification: bool


class QueuePolicy(TypedDict, total=False):
    max_concurrency: int
    lease_ms: int
    retry_delay_ms: int
    task_timeout_ms: int | None
    max_jobs: int
    max_payload_bytes: int


class _JobInputRequired(TypedDict):
    kind: str
    lane: str
    payload: dict[str, Any]


class JobInput(_JobInputRequired, total=False):
    priority: int
    not_before: int | None
    max_attempts: int
    retry_safe: bool
    idempotency_key: str | None
    objective_id: str | None
    depth: int


class AtSchedule(TypedDict):
    kind: Literal['at']
    unix_ms: int


class _IntervalRequired(TypedDict):
    kind: Literal['interval']
    every_ms: int


class IntervalSchedule(_IntervalRequired, total=False):
    anchor_unix_ms: int | None


class _CronRequired(TypedDict):
    kind: Literal['cron']
    expression: str
    timezone: str


class CronSchedule(_CronRequired, total=False):
    dom_and_dow: bool


ScheduleDefinition = AtSchedule | IntervalSchedule | CronSchedule


class _ScheduleRequired(TypedDict):
    schedule: ScheduleDefinition
    job: JobInput


class ScheduleSpec(_ScheduleRequired, total=False):
    misfire_policy: Literal['skip', 'fire_once']
    misfire_grace_ms: int
    start_at: int | None
    stop_at: int | None
    max_runs: int | None


JobStatus = Literal['pending', 'running', 'cancelling', 'uncertain', 'completed', 'failed', 'cancelled']


class Job(TypedDict):
    id: str
    revision: int
    input: JobInput
    status: JobStatus
    sequence: int
    attempts: int
    not_before: int
    created_at: int
    started_at: int | None
    attempt_started_at: int | None
    finished_at: int | None
    execution_id: str | None
    worker_id: str | None
    lease_expires_at: int | None
    cancellation_requested: bool
    result: Any
    error: str | None


class CompletedJob(TypedDict):
    type: Literal['completed']
    result: Any


class _FailedJobRequired(TypedDict):
    type: Literal['failed']
    error: str


class FailedJob(_FailedJobRequired, total=False):
    retryable: bool


class CancelledJob(TypedDict):
    type: Literal['cancelled']


class UncertainJob(TypedDict):
    type: Literal['uncertain']
    error: str


class RetryJob(TypedDict):
    type: Literal['retry']


JobOutcome = CompletedJob | FailedJob | CancelledJob | UncertainJob
JobResolution = CompletedJob | _FailedJobRequired | CancelledJob | RetryJob
JobExecutor = Callable[[Job, 'JobContext'], JobOutcome | Awaitable[JobOutcome]]


class DurableQueue:
    def __init__(self, store, id, policy):
        self._store, self._id, self._policy = store, id, policy

    @classmethod
    async def open(cls, store, *, id: str = 'default', policy: QueuePolicy | None = None) -> DurableQueue:
        value = await store._dispatch_async({'op': 'queue', 'id': id, 'command': {'op': 'open', 'policy': policy or {}}})
        return cls(store, id, value['policy'])

    @property
    def id(self): return self._id
    @property
    def policy(self): return copy.deepcopy(self._policy)
    async def _call(self, op, **fields):
        return await self._store._dispatch_async({'op': 'queue', 'id': self._id, 'command': {**fields, 'op': op}})
    async def inspect(self): return await self._call('inspect')
    async def enqueue(self, input: JobInput) -> Job: return await self._call('enqueue', input=input)
    async def get(self, job_id: str) -> Job | None: return await self._call('get', job_id=job_id)
    async def list(self, *, limit=50, cursor=None): return await self._call('list', limit=limit, cursor=cursor)
    async def claim(self, worker_id: str, kinds: list[str]) -> Job | None: return await self._call('claim', worker_id=worker_id, kinds=kinds)
    async def heartbeat(self, job_id: str, execution_id: str) -> Job: return await self._call('heartbeat', job_id=job_id, execution_id=execution_id)
    async def finish(self, job_id: str, execution_id: str, outcome: JobOutcome) -> Job: return await self._call('finish', job_id=job_id, execution_id=execution_id, outcome=outcome)
    async def cancel(self, job_id): return await self._call('cancel', job_id=job_id)
    async def cancel_execution(self, job_id, execution_id): return await self._call('cancel_execution', job_id=job_id, execution_id=execution_id)
    async def maintain(self): return await self._call('maintain')
    async def reconcile(self, job_id: str, execution_id: str, resolution: JobResolution) -> Job: return await self._call('reconcile', job_id=job_id, execution_id=execution_id, resolution=resolution)

    async def create_group(self, group_id: str, spec: GroupSpec): return await self._call('group_create', group_id=group_id, spec=spec)
    async def group(self, group_id: str): return await self._call('group_get', group_id=group_id)
    async def synthesize_group(self, group_id: str) -> Job: return await self._call('group_synthesize', group_id=group_id)
    async def cancel_group(self, group_id: str): return await self._call('group_cancel', group_id=group_id)
    async def list_jobs(self, filter: dict | None = None, *, limit: int = 50, cursor: str | None = None): return await self._call('jobs_list', filter=filter or {}, limit=limit, cursor=cursor)
    async def retry_job(self, job_id: str, *, force: bool = False) -> Job: return await self._call('job_retry', job_id=job_id, force=force)
    async def cancel_jobs(self, filter: dict): return await self._call('jobs_cancel_where', filter=filter)
    async def purge_jobs(self, filter: dict, finished_before: int, *, limit: int = 100): return await self._call('jobs_purge', filter=filter, finished_before=finished_before, limit=limit)
    async def events(self, *, after: int = 0, limit: int = 100): return await self._call('events', after=after, limit=limit)
    async def prune_events(self, up_to: int): return await self._call('events_prune', up_to=up_to)
    async def create_goal(self, goal_id: str, spec: GoalSpec): return await self._call('goal_create', goal_id=goal_id, spec=spec)
    async def goal(self, goal_id: str): return await self._call('goal_get', goal_id=goal_id)
    async def tick_goal(self, goal_id: str): return await self._call('goal_tick', goal_id=goal_id)
    async def cancel_goal(self, goal_id: str): return await self._call('goal_cancel', goal_id=goal_id)
    async def group_verification_input(self, group_id: str, task_id: str): return await self._call('group_verification_input', group_id=group_id, task_id=task_id)
    async def record_group_proof(self, group_id: str, task_id: str, verification_key: str): return await self._call('group_record_proof', group_id=group_id, task_id=task_id, verification_key=verification_key)

    async def create_schedule(self, schedule_id: str, spec: ScheduleSpec): return await self._call('schedule_create', schedule_id=schedule_id, spec=spec)
    async def schedule(self, schedule_id: str): return await self._call('schedule_get', schedule_id=schedule_id)
    async def enable_schedule(self, schedule_id: str, enabled=True): return await self._call('schedule_enable', schedule_id=schedule_id, enabled=enabled)
    async def tick_schedule(self, schedule_id: str, observed=None): return await self._call('schedule_tick', schedule_id=schedule_id, observed=observed)
    async def list_schedules(self, *, limit=50, cursor=None): return await self._call('schedule_list', limit=limit, cursor=cursor)
    async def next_occurrence(self, schedule: ScheduleDefinition, after: int): return await self._call('schedule_next', schedule=schedule, after=after)

    async def run_scheduler(self, *, poll_ms=1000, cancel_event: asyncio.Event | None = None):
        """Opt-in producer loop; Rust decides calendar state and job publication."""
        if isinstance(poll_ms, bool) or not isinstance(poll_ms, int) or not 10 <= poll_ms <= 60000:
            raise ValueError('Scheduler poll interval must be 10..60000 ms')
        self._store._retain()
        try:
            while cancel_event is None or not cancel_event.is_set():
                cursor = None
                while True:
                    if cancel_event is not None and cancel_event.is_set(): return
                    page = await self.list_schedules(limit=50, cursor=cursor)
                    for entry in page['items']:
                        if cancel_event is not None and cancel_event.is_set(): return
                        await self.tick_schedule(entry['id'])
                    cursor = page['next_cursor']
                    if cursor is None: break
                if cancel_event is None: await asyncio.sleep(poll_ms / 1000)
                else:
                    try: await asyncio.wait_for(cancel_event.wait(), poll_ms / 1000)
                    except asyncio.TimeoutError: pass
        finally: self._store._release()

    async def run_once(self, executors: dict[str, JobExecutor], *, worker_id: str | None = None, cancel_event: asyncio.Event | None = None) -> list[Job]:
        executors = dict(executors)
        if not executors or len(executors) > 256 or any(not callable(f) for f in executors.values()):
            raise TypeError('Register at least one job executor function')
        self._store._retain()
        # Host callbacks must not occupy the same thread pool used by durable
        # storage/heartbeats, even when the application raises concurrency.
        thread_pool = ThreadPoolExecutor(max_workers=self._policy['max_concurrency'], thread_name_prefix='hive-job') if any(not inspect.iscoroutinefunction(f) for f in executors.values()) else None
        tasks = []
        error = None
        stop = asyncio.Event()
        watcher = None
        worker_id = worker_id or str(uuid.uuid4())
        if cancel_event is not None:
            async def watch():
                await cancel_event.wait()
                stop.set()
            watcher = asyncio.create_task(watch())
            if cancel_event.is_set(): stop.set()
        try:
            if stop.is_set(): raise asyncio.CancelledError
            await self.maintain()
            for _ in range(self._policy['max_concurrency']):
                if stop.is_set(): break
                receipt = []
                try:
                    job = await self._store._dispatch_async({'op': 'queue', 'id': self._id,
                        'command': {'op': 'claim', 'worker_id': worker_id, 'kinds': list(executors)}},
                        on_result=receipt.append)
                except BaseException as failure:
                    stop.set()
                    # A cancelled storage wait may have committed a claim. Join
                    # it and acknowledge cancellation without invoking the host.
                    if receipt and receipt[0] is not None:
                        job = receipt[0]
                        tasks.append(asyncio.create_task(self._execute(job, executors[job['input']['kind']], stop, thread_pool)))
                    error = failure
                    break
                if job is None: break
                tasks.append(asyncio.create_task(self._execute(job, executors[job['input']['kind']], stop, thread_pool)))
            if tasks:
                gather = asyncio.ensure_future(asyncio.gather(*tasks, return_exceptions=True))
                try: results = await asyncio.shield(gather)
                except asyncio.CancelledError as interrupted:
                    stop.set()
                    results, _ = await _join(gather)
                    error = interrupted
            else: results = []
            if error is not None: raise error
            for result in results:
                if isinstance(result, BaseException): raise result
            return sorted(results, key=lambda job: job['sequence'])
        finally:
            if watcher is not None:
                watcher.cancel()
                await _join(asyncio.ensure_future(asyncio.gather(watcher, return_exceptions=True)))
            if thread_pool is not None: thread_pool.shutdown(wait=True)
            self._store._release()

    async def _execute(self, job, execute, external_cancel, thread_pool):
        cancel = asyncio.Event()
        stopped = asyncio.Event()
        lease_errors = []
        if external_cancel is not None and external_cancel.is_set(): cancel.set()
        async def renew():
            while not stopped.is_set():
                try: await asyncio.wait_for(stopped.wait(), min(self._policy['lease_ms'], self._policy['task_timeout_ms'] or float('inf')) / 3000)
                except asyncio.TimeoutError: pass
                if stopped.is_set(): return
                try:
                    if external_cancel is not None and external_cancel.is_set():
                        await self.cancel_execution(job['id'], job['execution_id'])
                    receipt = await self.heartbeat(job['id'], job['execution_id'])
                    if receipt['cancellation_requested']: cancel.set()
                except Exception as error:
                    lease_errors.append(error); cancel.set(); return
        heartbeat = asyncio.create_task(renew())
        context = JobContext(self._store, job, cancel)
        try:
            if cancel.is_set(): outcome = {'type': 'cancelled'}
            elif inspect.iscoroutinefunction(execute): outcome = await execute(copy.deepcopy(job), context)
            else:
                outcome, interrupted = await _join(asyncio.ensure_future(asyncio.get_running_loop().run_in_executor(thread_pool, execute, copy.deepcopy(job), context)))
                if interrupted is not None: cancel.set(); raise interrupted
                if inspect.isawaitable(outcome): outcome = await outcome
        except BaseException:
            outcome = {'type': 'uncertain', 'error': 'Executor failed without a confirmed outcome'}
        finally:
            stopped.set(); await _join(heartbeat)
        if lease_errors: raise lease_errors[0]
        try:
            if external_cancel is not None and external_cancel.is_set(): await self.cancel_execution(job['id'], job['execution_id'])
            return await self.finish(job['id'], job['execution_id'], outcome)
        except BaseException:
            try: await self.finish(job['id'], job['execution_id'], {'type': 'uncertain', 'error': 'Executor outcome could not be committed'})
            except Exception: pass
            raise


class JobContext:
    def __init__(self, store, job, cancel_event):
        self._store = store
        self._job_id, self._execution_id = job['id'], job['execution_id']
        self._objective_id, self._depth = job['input']['objective_id'], job['input']['depth']
        self._binding = copy.deepcopy(job['input']['payload'].get('delegation')) if job['input']['kind'].startswith(('hive.specialist.', 'hive.synthesis.')) else None
        self._cancel_event = cancel_event

    @property
    def job_id(self) -> str: return self._job_id
    @property
    def execution_id(self) -> str: return self._execution_id
    @property
    def objective_id(self) -> str | None: return self._objective_id
    @property
    def depth(self) -> int: return self._depth
    @property
    def cancel_event(self) -> asyncio.Event: return self._cancel_event

    async def run_agent(self, agent, prompt=None, *, resume=False):
        return await agent._run_queued(prompt, store=self._store, objective_id=self._objective_id,
                                       depth=self._depth, cancel_event=self._cancel_event, resume=resume, binding=self._binding)
