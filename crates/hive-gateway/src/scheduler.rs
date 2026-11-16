//! Native task scheduling on the shared SDK kernel. `cronJobs` rows stay the
//! definition the desktop edits; each active row is mirrored as an immutable kernel
//! schedule, whose occurrences become queue jobs that a `WorkerPool` runs, and
//! `taskRuns` is a view derived from the queue's event log. A run whose outcome is
//! unknown (the process died mid-run) is `uncertain` and is never replayed, neither
//! by the schedule nor by a manual trigger.
use crate::{
    Result,
    config::{self, App},
    management, server,
};
use chrono::{DateTime, Utc};
use hive_agent_core::{
    harness::{DurableQueue, Job, JobFilter, JobOutcome, JobStatus, QueuePolicy, WorkerPool},
    provider::CancellationToken,
    scheduler::Schedule,
    sessions::SessionStore,
};
use hivedb_core::{ColOp, PutOptions};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const KIND: &str = "gateway.cron";
const QUEUE: &str = "gateway.cron";
/// Finished jobs older than this are purged; `taskRuns` keeps the history.
const KEEP_FINISHED_MS: u64 = 3_600_000;
const BUSY: [&str; 3] = ["queued", "running", "uncertain"];

fn policy() -> QueuePolicy {
    // A short lease: a worker that died is declared uncertain within seconds.
    QueuePolicy {
        max_concurrency: 4,
        lease_ms: 2_000,
        ..Default::default()
    }
}
fn stamp(ms: u64) -> String {
    DateTime::from_timestamp_millis(ms as i64)
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}
fn timestamp(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        DateTime::parse_from_rfc3339(value.as_str()?)
            .ok()
            .map(|d| d.timestamp_millis().max(0) as u64)
    })
}
fn store(app: &App) -> Result<SessionStore> {
    app.store
        .lock()
        .map_err(|_| "Store bloqueado")?
        .as_ref()
        .cloned()
        .ok_or_else(|| "Store cerrado".into())
}
fn queue_for(app: &App) -> Result<DurableQueue> {
    let queue = store(app)?.queue(QUEUE)?;
    queue.open(policy())?;
    Ok(queue)
}
fn bounded(text: &str) -> String {
    let mut end = text.len().min(4000);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
/// Stable (not cryptographic) fingerprint of a schedule definition.
fn fingerprint(value: &Value) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.to_string().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
fn schedule_of(row: &Value, anchored: bool) -> Result<Schedule> {
    Ok(match row["task_type"].as_str().unwrap_or("recurring") {
        "interval" => {
            let sec = row["interval_sec"]
                .as_u64()
                .filter(|n| *n > 0 && *n <= 31_536_000)
                .ok_or("Intervalo inválido")?;
            Schedule::Interval {
                every_ms: sec * 1000,
                anchor_unix_ms: if anchored {
                    timestamp(&row["next_run_at"])
                } else {
                    None
                },
            }
        }
        "one_shot" => Schedule::At {
            unix_ms: timestamp(&row["fire_at"]).ok_or("Fecha inválida")?,
        },
        "recurring" => Schedule::Cron {
            expression: row["cron_expression"].as_str().unwrap_or_default().into(),
            timezone: row["timezone"].as_str().unwrap_or("UTC").into(),
            dom_and_dow: row["dom_and_dow"] == true || row["dom_and_dow"] == 1,
        },
        _ => return Err("Tipo de tarea no compatible con el scheduler nativo".into()),
    })
}
fn next(row: &Value, now: u64) -> Result<Option<u64>> {
    let schedule = schedule_of(row, false)?;
    let next = schedule.next_after(now)?;
    if matches!(schedule, Schedule::Cron { .. }) && next.is_none() {
        return Err("No hay una fecha cron válida en ocho años".into());
    }
    Ok(next)
}
/// Kernel schedules are immutable, so the id carries a fingerprint of the definition:
/// editing a task yields a new schedule and retires the old one.
fn schedule_id(row: &Value) -> Result<String> {
    let id = config::string(row, "id")?;
    let definition = json!([
        row["task_type"],
        row["interval_sec"],
        row["cron_expression"],
        row["timezone"],
        row["dom_and_dow"],
        row["fire_at"],
        row["start_at"],
        row["stop_at"],
        row["max_runs"]
    ]);
    Ok(format!("cron.{id}.{}", fingerprint(&definition)))
}
fn spec_of(row: &Value) -> Result<Value> {
    let id = config::string(row, "id")?;
    let remaining = row["max_runs"]
        .as_u64()
        .filter(|limit| *limit > 0)
        .map(|limit| limit.saturating_sub(row["run_count"].as_u64().unwrap_or(0)));
    if remaining == Some(0) {
        return Err("La tarea alcanzó su máximo de ejecuciones".into());
    }
    Ok(json!({"schedule":schedule_of(row, true)?,
        "job":{"kind":KIND,"lane":format!("cron.{id}"),"payload":{"task_id":id},"max_attempts":1,"retry_safe":false},
        "misfire_policy":"skip","misfire_grace_ms":60_000,
        "start_at":timestamp(&row["start_at"]),"stop_at":timestamp(&row["stop_at"]),"max_runs":remaining}))
}
fn busy_runs(app: &App, task: &str) -> Result<bool> {
    Ok(config::docs(&app.db, "taskRuns")?.iter().any(|run| {
        run["task_id"] == task && run["status"].as_str().is_some_and(|s| BUSY.contains(&s))
    }))
}
pub(crate) fn route(
    app: &App,
    method: &str,
    path: &str,
    raw: &str,
    body: &[u8],
) -> Result<Option<(u16, Value)>> {
    let user = management::owner(app)?;
    let query = server::query(raw)?;
    let parts = path
        .trim_start_matches("/api/cron")
        .trim_matches('/')
        .split('/')
        .collect::<Vec<_>>();
    let id = server::decode(parts[0])?;
    let action = parts.get(1).copied().unwrap_or_default();
    let _guard = app
        .activity_lock
        .lock()
        .map_err(|_| "Scheduler bloqueado")?;
    if method == "GET" && id.is_empty() {
        return Ok(Some((
            200,
            json!({"tasks":config::docs(&app.db,"cronJobs")?.into_iter().filter(|r|management::owned(r,&user)).collect::<Vec<_>>()}),
        )));
    }
    if method == "GET" && id == "status" {
        return Ok(Some((
            200,
            json!({"running":true,"backend":"rust","automatic":true,"engine":"shared-scheduler"}),
        )));
    }
    let Some(row) = app.db.col_get("cronJobs", &id)? else {
        return Ok(Some((404, json!({"error":"Tarea desconocida"}))));
    };
    if !management::owned(&row.doc, &user) {
        return Ok(Some((404, json!({"error":"Tarea desconocida"}))));
    };
    if method == "GET" && action == "history" {
        let mut rows = config::docs(&app.db, "taskRuns")?
            .into_iter()
            .filter(|r| r["task_id"] == id)
            .collect::<Vec<_>>();
        rows.sort_by_key(|r| {
            std::cmp::Reverse(r["started_at"].as_str().unwrap_or_default().to_owned())
        });
        rows.truncate(
            query
                .get("limit")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(10)
                .min(100),
        );
        return Ok(Some((200, json!({"history":rows}))));
    }
    if method == "GET" {
        return Ok(Some((200, json!({"task":row.doc}))));
    }
    if method == "DELETE" {
        app.db.col_batch(&[ColOp::Delete {
            collection: "cronJobs".into(),
            id,
        }])?;
        // The mirrored schedule is retired on the next sync.
        if let Ok(queue) = queue_for(app) {
            let _ = sync_locked(app, &queue);
        }
        return Ok(Some((200, json!({"ok":true}))));
    }
    if method == "POST" && action == "trigger" {
        return Ok(Some(match trigger(app, &row.doc, "manual") {
            Ok(()) => (202, json!({"ok":true,"status":"queued"})),
            Err(error) => (409, json!({"ok":false,"error":error.to_string()})),
        }));
    }
    if method == "POST" && matches!(action, "pause" | "resume") {
        let mut doc = row.doc;
        doc["status"] = json!(if action == "pause" {
            "paused"
        } else {
            "active"
        });
        if action == "resume" {
            match next(&doc, server::now()) {
                Ok(at) => doc["next_run_at"] = at.map(|at| json!(stamp(at))).unwrap_or(Value::Null),
                Err(e) => return Ok(Some((400, json!({"error":e.to_string()})))),
            };
        }
        app.db.col_put(
            "cronJobs",
            &id,
            &doc,
            PutOptions {
                expected_version: Some(row.version),
            },
        )?;
        // Apply it now, not on the next tick: a paused task must not fire in between.
        if let Ok(queue) = queue_for(app) {
            let _ = sync_locked(app, &queue);
        }
        return Ok(Some((200, json!({"ok":true}))));
    }
    let _ = body;
    Ok(Some((405, json!({"error":"Método de tarea no permitido"}))))
}

/// Queue one run by hand. Refused while a run of the task is queued, running or
/// uncertain, so a manual trigger can never replay interrupted work.
fn trigger(app: &App, task: &Value, reason: &str) -> Result<()> {
    let id = config::string(task, "id")?;
    let queue = queue_for(app)?;
    let lane = format!("cron.{id}");
    let live = queue.list_where(
        &JobFilter {
            lane: Some(lane.clone()),
            status: vec![
                JobStatus::Pending,
                JobStatus::Running,
                JobStatus::Cancelling,
                JobStatus::Uncertain,
            ],
            ..Default::default()
        },
        1,
        None,
    )?;
    if busy_runs(app, id)? || !live["items"].as_array().is_none_or(Vec::is_empty) {
        return Err("La tarea ya está en cola o en ejecución".into());
    }
    queue.enqueue(serde_json::from_value(json!({"kind":KIND,"lane":lane,"payload":{"task_id":id,"reason":reason},"max_attempts":1,"retry_safe":false}))?)?;
    Ok(())
}

fn patch_task(app: &App, id: &str, body: &Value) {
    let _ = management::patch(
        app,
        "cronJobs",
        id,
        body,
        &[
            ("status", "status"),
            ("next_run_at", "next_run_at"),
            ("last_error", "last_error"),
            ("schedule_id", "schedule_id"),
        ],
    );
}

/// Mirror every `cronJobs` row into the kernel: create the schedule for its current
/// definition, enable it only while the task is active and has no uncertain run, and
/// retire schedules whose task was edited, paused or deleted. Callers hold the
/// activity lock.
fn sync_locked(app: &App, queue: &DurableQueue) -> Result<()> {
    let user = management::owner(app)?;
    let mut live = std::collections::BTreeSet::new();
    for row in config::docs(&app.db, "cronJobs")? {
        let Some(id) = row["id"].as_str().map(str::to_owned) else {
            continue;
        };
        let payload = row["payload"]
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or_else(|| row["payload"].clone());
        if payload["_internal"] == true
            || !management::owned(&row, &user)
            || row["name"].as_str().is_some_and(|s| s.starts_with("_hive"))
        {
            continue;
        }
        live.insert(id.clone());
        let previous = row["schedule_id"].as_str().map(str::to_owned);
        if row["status"] != "active" {
            if let Some(old) = previous {
                let _ = queue.enable_schedule(&old, false);
            }
            continue;
        }
        let wanted = match schedule_id(&row).and_then(|sid| Ok((sid, spec_of(&row)?))) {
            Ok(wanted) => wanted,
            Err(error) => {
                let finished = error.to_string().contains("máximo de ejecuciones");
                patch_task(
                    app,
                    &id,
                    &json!({"status":if finished {"completed"} else {"error"},"last_error":error.to_string(),"next_run_at":Value::Null}),
                );
                continue;
            }
        };
        let (sid, spec) = wanted;
        if previous.as_deref() != Some(sid.as_str()) {
            if let Some(old) = &previous {
                let _ = queue.enable_schedule(old, false);
            }
            if let Err(error) = queue.create_schedule(&sid, serde_json::from_value(spec)?) {
                patch_task(
                    app,
                    &id,
                    &json!({"status":"error","last_error":error.to_string(),"next_run_at":Value::Null}),
                );
                continue;
            }
            patch_task(app, &id, &json!({"schedule_id":sid}));
        }
        // An uncertain run blocks the task until someone reviews it.
        let blocked = config::docs(&app.db, "taskRuns")?
            .iter()
            .any(|run| run["task_id"] == row["id"] && run["status"] == "uncertain");
        let report = queue.enable_schedule(&sid, !blocked)?;
        let due = report["next_due"].as_u64();
        if report["status"] == "completed" {
            let ran = report["published"].as_u64().unwrap_or(0) > 0;
            patch_task(
                app,
                &id,
                &json!({"status":if ran {"completed"} else {"missed"},"next_run_at":Value::Null}),
            );
        } else if due != timestamp(&row["next_run_at"]) {
            patch_task(
                app,
                &id,
                &json!({"next_run_at":due.map(|at| json!(stamp(at))).unwrap_or(Value::Null)}),
            );
        }
    }
    // Schedules of tasks that no longer exist.
    let mut cursor = None::<String>;
    loop {
        let page = queue.list_schedules(100, cursor.as_deref())?;
        for entry in page["items"].as_array().into_iter().flatten() {
            let Some(sid) = entry["id"].as_str() else {
                continue;
            };
            let task = sid
                .strip_prefix("cron.")
                .and_then(|rest| rest.rsplit_once('.'))
                .map(|(task, _)| task);
            if entry["enabled"] == true && task.is_some_and(|task| !live.contains(task)) {
                let _ = queue.enable_schedule(sid, false);
            }
        }
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    Ok(())
}

fn run_status(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Pending => "queued",
        JobStatus::Running | JobStatus::Cancelling => "running",
        JobStatus::Uncertain => "uncertain",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
}

/// Keeps `taskRuns` (the history the desktop reads) in step with the queue by
/// consuming its event log, and counts each finished run once on its task.
struct Projection {
    after: u64,
}
impl Projection {
    fn apply(&mut self, app: &App, queue: &DurableQueue, user: &str) -> Result<()> {
        loop {
            let page = queue.events(self.after, 200)?;
            let items = page["items"].as_array().cloned().unwrap_or_default();
            for event in &items {
                if event["type"] == "job.status"
                    && event["subject"]["kind"] == KIND
                    && let Some(job) = event["subject"]["job_id"]
                        .as_str()
                        .and_then(|id| queue.get(id).ok().flatten())
                {
                    self.project_job(app, &job, user)?;
                }
            }
            self.after = page["next_after"].as_u64().unwrap_or(self.after);
            if items.len() < 200 {
                break;
            }
        }
        Ok(())
    }
    fn project_job(&self, app: &App, job: &Job, user: &str) -> Result<()> {
        let id = format!("run-{}", job.id);
        let task = job.input.payload["task_id"].as_str().unwrap_or_default();
        let existing = app.db.col_get("taskRuns", &id)?;
        let status = run_status(job.status);
        if existing
            .as_ref()
            .is_some_and(|row| row.doc["status"] == status)
        {
            return Ok(());
        }
        let reason = if job.input.payload["schedule"].is_object() {
            "scheduled"
        } else {
            job.input.payload["reason"].as_str().unwrap_or("manual")
        };
        let mut doc = json!({"id":id,"task_id":task,"user_id":user,"status":status,"started_at":stamp(job.created_at),
            "native_runtime":true,"reason":reason});
        if job.status.terminal_for_run() {
            doc["finished_at"] = json!(stamp(job.finished_at.unwrap_or_else(server::now)));
        }
        if let Some(text) = job.result.as_ref().and_then(|r| r["text"].as_str()) {
            doc["result"] = json!(text);
        }
        if let Some(error) = &job.error {
            doc["error"] = json!(error);
        }
        app.db.col_put(
            "taskRuns",
            &id,
            &doc,
            PutOptions {
                expected_version: Some(existing.as_ref().map_or(0, |row| row.version)),
            },
        )?;
        let first_end = job.status.terminal_for_run()
            && existing.as_ref().is_none_or(|row| {
                !matches!(
                    row.doc["status"].as_str(),
                    Some("completed" | "failed" | "cancelled" | "uncertain")
                )
            });
        if first_end && job.status != JobStatus::Cancelled {
            self.count(app, task, job)?;
        }
        Ok(())
    }
    fn count(&self, app: &App, task: &str, job: &Job) -> Result<()> {
        let Some(row) = app.db.col_get("cronJobs", task)? else {
            return Ok(());
        };
        let failure = job
            .error
            .as_ref()
            .filter(|_| job.status != JobStatus::Completed);
        management::patch(
            app,
            "cronJobs",
            task,
            &json!({"last_run_at":stamp(job.finished_at.unwrap_or_else(server::now)),
                "run_count":row.doc["run_count"].as_u64().unwrap_or(0).saturating_add(1),
                "error_count":row.doc["error_count"].as_u64().unwrap_or(0).saturating_add(u64::from(failure.is_some())),
                "last_error":failure,
                "status":if row.doc["task_type"]=="one_shot" {json!(if failure.is_some(){"error"}else{"completed"})}else{row.doc["status"].clone()}}),
            &[
                ("last_run_at", "last_run_at"),
                ("run_count", "run_count"),
                ("error_count", "error_count"),
                ("last_error", "last_error"),
                ("status", "status"),
            ],
        )?;
        Ok(())
    }
}
trait RunEnd {
    fn terminal_for_run(self) -> bool;
}
impl RunEnd for JobStatus {
    /// Uncertain is final for the run (it needs review) even though the queue keeps it on its lane.
    fn terminal_for_run(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Uncertain
        )
    }
}

/// Runs from before the shared scheduler (their ids are UUIDs, not `run-…`) are
/// untouched work of a previous process: a running one is uncertain, a queued one
/// never started and is cancelled so it cannot fire later by surprise.
fn adopt_legacy_runs(app: &App) {
    if let Ok(rows) = config::docs(&app.db, "taskRuns") {
        for row in rows {
            let id = row["id"].as_str().unwrap_or_default();
            if row["native_runtime"] != true || id.starts_with("run-") {
                continue;
            }
            let patch = match row["status"].as_str() {
                Some("running") => {
                    json!({"status":"uncertain","error":"Ejecución interrumpida; revisar antes de reintentar"})
                }
                Some("queued") => {
                    json!({"status":"cancelled","error":"Cola anterior al scheduler compartido; dispara la tarea de nuevo"})
                }
                _ => continue,
            };
            let _ = management::patch(
                app,
                "taskRuns",
                id,
                &patch,
                &[("status", "status"), ("error", "error")],
            );
        }
    }
}

fn start_workers(
    app: &Arc<App>,
    queue: &DurableQueue,
    cancel: &CancellationToken,
) -> Result<Vec<tokio::task::JoinHandle<()>>> {
    let mut pool = WorkerPool::new(queue.clone(), "gateway-cron".into())?;
    let host = app.clone();
    pool.register(KIND, move |job: Job, _token: CancellationToken| {
        let host = host.clone();
        async move {
            let task = job.input.payload["task_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            Ok(match execute(host, &task).await {
                Ok(text) => JobOutcome::Completed {
                    result: json!({"text":bounded(&text)}),
                },
                Err(error) if error.is::<UncertainTask>() => JobOutcome::Uncertain {
                    error: bounded(&error.to_string()),
                },
                Err(error) => JobOutcome::Failed {
                    error: bounded(&error.to_string()),
                    retryable: false,
                },
            })
        }
    })?;
    let pool = Arc::new(pool);
    let stopper = pool.clone();
    let token = cancel.clone();
    let mut handles = vec![tokio::spawn(async move {
        token.cancelled().await;
        stopper.stop();
    })];
    handles.push(tokio::spawn(async move {
        while pool.run(100).await.is_err() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }));
    let producer = queue.clone();
    let token = cancel.clone();
    handles.push(tokio::spawn(async move {
        while !token.is_cancelled() {
            if producer.run_scheduler(250, token.clone()).await.is_err() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }));
    Ok(handles)
}

/// One maintenance cycle: sync definitions, project finished runs, purge old jobs and
/// consumed events. Taken under the activity lock like every other `cronJobs` writer.
fn cycle(app: &App, queue: &DurableQueue, projection: &mut Projection) -> Result<()> {
    let _guard = app
        .activity_lock
        .lock()
        .map_err(|_| "Scheduler bloqueado")?;
    let user = management::owner(app)?;
    // Project first: a run that just became uncertain must disable its schedule in this
    // same cycle, not one second later.
    projection.apply(app, queue, &user)?;
    sync_locked(app, queue)?;
    queue.purge_jobs(
        &JobFilter {
            kind: Some(KIND.into()),
            ..Default::default()
        },
        server::now().saturating_sub(KEEP_FINISHED_MS),
        200,
    )?;
    if projection.after > 0 {
        queue.prune_events(projection.after)?;
    }
    Ok(())
}

pub(crate) async fn run(app: Arc<App>) {
    adopt_legacy_runs(&app);
    let cancel = CancellationToken::new();
    let mut workers: Vec<tokio::task::JoinHandle<()>> = vec![];
    let mut projection = Some(Projection { after: 0 });
    let mut queue: Option<DurableQueue> = None;
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {_=app.shutdown.cancelled()=>break,_=clock.tick()=>{}}
        if queue.is_none() {
            // The store appears once a user exists.
            if let Ok(opened) = queue_for(&app) {
                match start_workers(&app, &opened, &cancel) {
                    Ok(handles) => {
                        workers = handles;
                        queue = Some(opened);
                    }
                    Err(_) => continue,
                }
            } else {
                continue;
            }
        }
        let (host, q) = (app.clone(), queue.clone().expect("opened above"));
        let mut state = projection.take().expect("restored below");
        let result = tokio::task::spawn_blocking(move || {
            let outcome = cycle(&host, &q, &mut state);
            (state, outcome)
        })
        .await;
        match result {
            Ok((state, _)) => projection = Some(state),
            Err(_) => projection = Some(Projection { after: 0 }),
        }
    }
    cancel.cancel();
    for handle in workers {
        let _ = handle.await;
    }
}
#[derive(Debug)]
struct UncertainTask(String);
impl std::fmt::Display for UncertainTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UncertainTask {}
async fn execute(app: Arc<App>, id: &str) -> Result<String> {
    let row = app
        .db
        .col_get("cronJobs", id)?
        .ok_or("La tarea fue eliminada")?
        .doc;
    if row["tool_name"].as_str().is_some_and(|s| !s.is_empty()) {
        return Err("Esta tarea necesita una herramienta externa aún no integrada".into());
    }
    let user = management::owner(&app)?;
    if row["channel"]
        .as_str()
        .is_some_and(|channel| !matches!(channel, "auto" | "webchat" | ""))
    {
        return Err(
            "El canal de entrega de esta tarea no tiene un adaptador conectado en Rust".into(),
        );
    }
    if row["agent_id"].as_str().is_some_and(|id| !id.is_empty()) {
        let selected = app.selection()?;
        if row["agent_id"] != selected.agent["id"] {
            return Err(
                "Esta tarea apunta a un especialista; el scheduler nativo ejecuta el coordinador"
                    .into(),
            );
        }
    }
    let thread = format!("{user}/webchat/cron-{id}");
    if app.db.col_get("conversationThreads", &thread)?.is_none() {
        app.db.col_put("conversationThreads",&thread,&json!({"id":thread,"user_id":user,"channel":"webchat","title":row["name"],"peer_id":format!("cron-{id}"),"archived":false,"created_at":server::now(),"last_message_at":server::now(),"message_count":0}),PutOptions{expected_version:Some(0)})?;
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    server::start_live_work(
        app.clone(),
        user,
        thread,
        config::string(&row, "task")?.into(),
        tx,
    )?;
    let result = tokio::select! {_=app.shutdown.cancelled()=>return Err(UncertainTask("Gateway detenido durante la tarea".into()).into()),result=rx.recv()=>result.ok_or_else(||UncertainTask("La tarea cerró sin resultado".into()))?};
    if result["ok"] == true {
        Ok(result["content"].as_str().unwrap_or_default().into())
    } else {
        Err(UncertainTask(
            result["error"]
                .as_str()
                .unwrap_or("Falló la tarea después del despacho")
                .to_owned(),
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, Arc<App>) {
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(folder.path().join("data")).unwrap();
        {
            let db = hivedb_core::HiveDB::open(folder.path().join("data/hivedb")).unwrap();
            db.col_put("users", "u1", &json!({"id":"u1"}), Default::default())
                .unwrap();
        }
        let app = Arc::new(App::open(folder.path().to_owned()).unwrap());
        (folder, app)
    }
    fn task(app: &App, id: &str, extra: Value) {
        let mut row = json!({"id":id,"user_id":"u1","name":"Tarea","status":"active","task_type":"interval","interval_sec":60,"task":"Resume"});
        row.as_object_mut()
            .unwrap()
            .extend(extra.as_object().cloned().unwrap_or_default());
        app.db
            .col_put("cronJobs", id, &row, Default::default())
            .unwrap();
    }
    fn row(app: &App, id: &str) -> Value {
        app.db.col_get("cronJobs", id).unwrap().unwrap().doc
    }
    fn runs(app: &App) -> Vec<Value> {
        config::docs(&app.db, "taskRuns").unwrap()
    }
    /// Deterministic cycle: sync, tick every schedule at `now`, then project.
    fn settle(app: &App, queue: &DurableQueue, projection: &mut Projection, now: u64) {
        cycle(app, queue, projection).unwrap();
        let mut cursor = None::<String>;
        loop {
            let page = queue.list_schedules(100, cursor.as_deref()).unwrap();
            for entry in page["items"].as_array().unwrap() {
                queue
                    .tick_schedule(entry["id"].as_str().unwrap(), Some(now))
                    .unwrap();
            }
            match page["next_cursor"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
        }
        cycle(app, queue, projection).unwrap();
    }
    fn finish(queue: &DurableQueue, outcome: JobOutcome) -> Job {
        let job = queue
            .claim("test", &[KIND.into()])
            .unwrap()
            .expect("a pending cron job");
        queue
            .finish(&job.id, job.execution_id.as_deref().unwrap(), outcome)
            .unwrap()
    }
    async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while !check() {
            assert!(tokio::time::Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn interrupted_scheduled_work_stays_uncertain_and_blocks_automatic_or_manual_replay() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(folder.path().join("data")).unwrap();
        {
            let db = hivedb_core::HiveDB::open(folder.path().join("data/hivedb")).unwrap();
            db.col_put("users", "u1", &json!({"id":"u1"}), Default::default())
                .unwrap();
            db.col_put("cronJobs", "task", &json!({"id":"task","user_id":"u1","name":"Fixture","status":"active","task_type":"interval","interval_sec":60,"next_run_at":stamp(server::now()-1000)}),Default::default()).unwrap();
            // A run left by a previous process, from before the shared scheduler.
            db.col_put(
                "taskRuns",
                "old",
                &json!({"id":"old","task_id":"task","native_runtime":true,"status":"running"}),
                Default::default(),
            )
            .unwrap();
            db.col_put("taskRuns", "never-started", &json!({"id":"never-started","task_id":"other","native_runtime":true,"status":"queued"}), Default::default()).unwrap();
        }
        let app = Arc::new(App::open(folder.path().to_owned()).unwrap());
        let host = app.clone();
        let runtime = tokio::spawn(async move { run(host).await });
        eventually("legacy running run becomes uncertain", || {
            runs(&app)
                .iter()
                .any(|r| r["id"] == "old" && r["status"] == "uncertain")
        })
        .await;
        assert_eq!(
            app.db
                .col_get("taskRuns", "never-started")
                .unwrap()
                .unwrap()
                .doc["status"],
            "cancelled",
            "a queued run that never started must not fire by surprise"
        );
        let queue = queue_for(&app).unwrap();
        eventually("the task's schedule exists but is disabled", || {
            let page = queue.list_schedules(10, None).unwrap();
            page["items"]
                .as_array()
                .is_some_and(|items| items.len() == 1 && items[0]["enabled"] == false)
        })
        .await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            queue.list_where(&JobFilter::default(), 10, None).unwrap()["items"]
                .as_array()
                .unwrap()
                .is_empty(),
            "nothing was published while the run is uncertain"
        );
        assert_eq!(runs(&app).len(), 2);
        assert!(trigger(&app, &row(&app, "task"), "manual").is_err());
        app.shutdown.cancel();
        runtime.await.unwrap();
    }

    #[test]
    fn desktop_cron_respects_iana_timezone_and_rejects_invalid_schedules() {
        let now = DateTime::parse_from_rfc3339("2026-11-13T12:00:00Z")
            .unwrap()
            .timestamp_millis() as u64;
        let daily = json!({"task_type":"recurring","cron_expression":"0 8 * * *","timezone":"America/Bogota"});
        assert_eq!(next(&daily, now).unwrap(), Some(now + 3_600_000));
        assert_eq!(
            next(&json!({"task_type":"interval","interval_sec":60}), now).unwrap(),
            Some(now + 60_000)
        );
        assert!(
            next(
                &json!({"task_type":"recurring","cron_expression":"*/0 * * * *","timezone":"UTC"}),
                now
            )
            .is_err()
        );
        assert!(
            next(
                &json!({"task_type":"recurring","cron_expression":"80 * * * *","timezone":"UTC"}),
                now
            )
            .is_err()
        );
    }

    #[test]
    fn schedules_publish_once_per_occurrence_for_the_owner_and_skip_missed_ones() {
        let (_folder, app) = open();
        let now = server::now();
        task(&app, "mine", json!({"next_run_at":stamp(now - 1000)}));
        task(
            &app,
            "foreign",
            json!({"user_id":"u2","next_run_at":stamp(now - 1000)}),
        );
        let queue = queue_for(&app).unwrap();
        let mut projection = Projection { after: 0 };
        settle(&app, &queue, &mut projection, now);
        settle(&app, &queue, &mut projection, now);
        let all = runs(&app);
        assert_eq!(
            all.len(),
            1,
            "one occurrence, one run, and none for another user's task"
        );
        assert_eq!(
            (
                all[0]["task_id"].clone(),
                all[0]["status"].clone(),
                all[0]["reason"].clone()
            ),
            (json!("mine"), json!("queued"), json!("scheduled"))
        );
        assert!(all[0]["id"].as_str().unwrap().starts_with("run-"));
        assert!(timestamp(&row(&app, "mine")["next_run_at"]).unwrap() > now);
        // While that run is outstanding, later occurrences are not stacked on top of it.
        settle(&app, &queue, &mut projection, now + 86_400_000);
        assert_eq!(runs(&app).len(), 1);
        // Finish it; reopening after a long gap then skips what was missed instead of replaying it.
        finish(
            &queue,
            JobOutcome::Completed {
                result: json!({"text":"hecho"}),
            },
        );
        settle(&app, &queue, &mut projection, now + 86_400_000);
        let done = runs(&app);
        assert_eq!(
            (
                done.len(),
                done[0]["status"].clone(),
                done[0]["result"].clone()
            ),
            (1, json!("completed"), json!("hecho"))
        );
        assert!(
            queue
                .list_where(
                    &JobFilter {
                        status: vec![JobStatus::Pending],
                        ..Default::default()
                    },
                    10,
                    None
                )
                .unwrap()["items"]
                .as_array()
                .unwrap()
                .is_empty(),
            "missed occurrences are skipped, not replayed"
        );
        assert_eq!(
            (
                row(&app, "mine")["run_count"].clone(),
                row(&app, "mine")["error_count"].clone()
            ),
            (json!(1), json!(0))
        );
        // Projecting again from the start (a restart) never counts a run twice.
        let mut restarted = Projection { after: 0 };
        settle(&app, &queue, &mut restarted, now + 86_400_000);
        assert_eq!(row(&app, "mine")["run_count"], 1);
    }

    #[test]
    fn editing_pausing_and_deleting_a_task_retire_its_schedule() {
        let (_folder, app) = open();
        let now = 1_800_000_000_000;
        task(&app, "t", json!({"next_run_at":stamp(now + 60_000)}));
        let queue = queue_for(&app).unwrap();
        let mut projection = Projection { after: 0 };
        settle(&app, &queue, &mut projection, now);
        let enabled = |queue: &DurableQueue| -> Vec<String> {
            queue.list_schedules(50, None).unwrap()["items"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["enabled"] == true)
                .map(|e| e["id"].as_str().unwrap().to_owned())
                .collect()
        };
        let first = enabled(&queue);
        assert_eq!(first.len(), 1);
        assert_eq!(row(&app, "t")["schedule_id"], first[0].as_str());
        // Schedules are immutable: a new definition is a new schedule and the old one stops.
        let mut edited = row(&app, "t");
        edited["interval_sec"] = json!(120);
        let version = app.db.col_get("cronJobs", "t").unwrap().unwrap().version;
        app.db
            .col_put(
                "cronJobs",
                "t",
                &edited,
                PutOptions {
                    expected_version: Some(version),
                },
            )
            .unwrap();
        settle(&app, &queue, &mut projection, now);
        let second = enabled(&queue);
        assert_eq!(second.len(), 1);
        assert_ne!(second, first);
        assert_eq!(
            queue.list_schedules(50, None).unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        // Pause and resume through the API take effect immediately.
        let call = |method: &str, path: &str| route(&app, method, path, "", b"").unwrap().unwrap();
        assert_eq!(call("POST", "/api/cron/t/pause").0, 200);
        assert!(
            enabled(&queue).is_empty(),
            "a paused task must not fire in the next tick"
        );
        assert_eq!(call("POST", "/api/cron/t/resume").0, 200);
        assert_eq!(enabled(&queue).len(), 1);
        assert_eq!(call("DELETE", "/api/cron/t").0, 200);
        assert!(
            enabled(&queue).is_empty(),
            "deleting a task retires its schedule"
        );
        assert_eq!(
            call("GET", "/api/cron/status").1["engine"],
            "shared-scheduler"
        );
    }

    #[test]
    fn manual_triggers_share_the_lane_and_never_replay_uncertain_or_running_work() {
        let (_folder, app) = open();
        let now = 1_800_000_000_000;
        task(&app, "t", json!({"next_run_at":stamp(now + 3_600_000)}));
        let queue = queue_for(&app).unwrap();
        let mut projection = Projection { after: 0 };
        let call = |path: &str| route(&app, "POST", path, "", b"").unwrap().unwrap();
        assert_eq!(call("/api/cron/t/trigger").0, 202);
        assert_eq!(call("/api/cron/t/trigger").0, 409, "already queued");
        settle(&app, &queue, &mut projection, now);
        assert_eq!(runs(&app)[0]["reason"], "manual");
        // The worker lost the outcome: the run is uncertain and the task stays blocked.
        finish(
            &queue,
            JobOutcome::Uncertain {
                error: "worker desapareció".into(),
            },
        );
        settle(&app, &queue, &mut projection, now);
        let uncertain = runs(&app);
        assert_eq!(
            (
                uncertain[0]["status"].clone(),
                uncertain[0]["error"].clone()
            ),
            (json!("uncertain"), json!("worker desapareció"))
        );
        assert_eq!(call("/api/cron/t/trigger").0, 409);
        let schedule = queue.list_schedules(10, None).unwrap();
        assert_eq!(
            schedule["items"][0]["enabled"], false,
            "the schedule is disabled until the uncertain run is reviewed"
        );
        assert_eq!(row(&app, "t")["run_count"], 1);
        assert_eq!(row(&app, "t")["error_count"], 1);
    }

    #[test]
    fn limits_and_invalid_definitions_are_reported_on_the_task() {
        let (_folder, app) = open();
        let now = 1_800_000_000_000;
        task(
            &app,
            "bad",
            json!({"task_type":"recurring","cron_expression":"80 * * * *","timezone":"UTC"}),
        );
        task(
            &app,
            "spent",
            json!({"max_runs":2,"run_count":2,"next_run_at":stamp(now + 1000)}),
        );
        task(
            &app,
            "past",
            json!({"task_type":"one_shot","fire_at":stamp(now - 3_600_000)}),
        );
        let queue = queue_for(&app).unwrap();
        settle(&app, &queue, &mut Projection { after: 0 }, now);
        assert_eq!(row(&app, "bad")["status"], "error");
        assert!(
            row(&app, "bad")["last_error"]
                .as_str()
                .unwrap()
                .contains("cron")
        );
        assert_eq!(row(&app, "spent")["status"], "completed");
        assert_eq!(
            row(&app, "past")["status"],
            "missed",
            "a one-shot whose time has long passed is reported, not run"
        );
        assert!(runs(&app).is_empty());
    }

    #[tokio::test]
    async fn desktop_cron_runtime_records_unsupported_actions_without_faking_success() {
        let (_folder, app) = open();
        let due = server::now() - 1000;
        task(
            &app,
            "one",
            json!({"name":"Comprobación","task_type":"one_shot","task":"No ejecutar","tool_name":"unsupported","next_run_at":stamp(due),"fire_at":stamp(due)}),
        );
        let host = app.clone();
        let runtime = tokio::spawn(async move { run(host).await });
        eventually("the task settles", || row(&app, "one")["status"] == "error").await;
        app.shutdown.cancel();
        runtime.await.unwrap();
        let task = row(&app, "one");
        assert_eq!(
            (task["run_count"].clone(), task["error_count"].clone()),
            (json!(1), json!(1))
        );
        let all = runs(&app);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["status"], "failed");
        assert!(
            all[0]["error"]
                .as_str()
                .unwrap()
                .contains("herramienta externa")
        );
    }
}
