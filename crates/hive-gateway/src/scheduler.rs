//! Native task scheduling on the shared SDK kernel. `cronJobs` rows stay the
//! definition the desktop edits; each active row is mirrored as an immutable kernel
//! schedule, whose occurrences become queue jobs that a `WorkerPool` runs, and
//! `taskRuns` is a view derived from the queue's event log. A run whose outcome is
//! unknown (the process died mid-run) is `uncertain` and is never replayed, neither
//! by the schedule nor by a manual trigger.
use crate::{
    Result, channels,
    config::{self, App},
    management, server,
};
use chrono::{DateTime, Utc};
use hive_agent_core::{
    harness::{DurableQueue, Job, JobFilter, JobOutcome, JobStatus, QueuePolicy, WorkerPool},
    provider::CancellationToken,
    scheduler::Schedule,
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
/// The user a task runs as: its `user_id`, or the owner for shared rows with none.
fn task_owner(app: &App, row: &Value) -> Result<String> {
    match row["user_id"].as_str().filter(|id| !id.is_empty()) {
        Some(id) => Ok(id.to_owned()),
        None => app.owner_id(),
    }
}
/// Each user's tasks live in that user's own store and queue, so one user's schedules,
/// jobs and events are never visible to, or counted against, another's.
fn queue_for(app: &App, user: &str) -> Result<DurableQueue> {
    let queue = app.store_handle(user)?.queue(QUEUE)?;
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
/// Where a task's result is also sent: `all` (every active channel of its owner) or one
/// channel type. `auto` and `webchat` keep it in the task's conversation only.
fn delivery_target(row: &Value) -> Option<String> {
    row["channel"]
        .as_str()
        .filter(|channel| *channel == "all" || channels::DELIVERABLE.contains(channel))
        .map(str::to_owned)
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
    user: &str,
) -> Result<Option<(u16, Value)>> {
    let user = user.to_owned();
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
        .write()
        .map_err(|_| "Scheduler bloqueado")?;
    if method == "GET" && id.is_empty() {
        return Ok(Some((
            200,
            json!({"tasks":config::docs(&app.db,"cronJobs")?.into_iter().filter(|r|task_owner(app,r).is_ok_and(|owner|owner==user)).collect::<Vec<_>>()}),
        )));
    }
    if method == "GET" && id == "status" {
        return Ok(Some((
            200,
            json!({"running":true,"backend":"rust","automatic":true,"engine":"shared-scheduler"}),
        )));
    }
    if method == "POST" && id.is_empty() {
        return Ok(Some(create_task(app, &user, body)?));
    }
    let Some(row) = app.db.col_get("cronJobs", &id)? else {
        return Ok(Some((404, json!({"error":"Tarea desconocida"}))));
    };
    if task_owner(app, &row.doc)? != user {
        return Ok(Some((404, json!({"error":"Tarea desconocida"}))));
    };
    if method == "PATCH" && action.is_empty() {
        return Ok(Some(edit_task(app, &user, &row.doc, row.version, body)?));
    }
    if method == "GET" && action == "schedule" {
        // What the shared scheduler holds for this task: its immutable schedule, whether it is
        // enabled, the next due time, how many runs it published and the job it waits on.
        let queue = queue_for(app, &user)?;
        let report = match row.doc["schedule_id"].as_str() {
            Some(sid) => queue.get_schedule(sid)?,
            None => None,
        };
        return Ok(Some((
            200,
            json!({"scheduleId":row.doc["schedule_id"],"schedule":report}),
        )));
    }
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
        if let Ok(queue) = queue_for(app, &user) {
            let _ = sync_locked(app, &queue, &user);
        }
        return Ok(Some((200, json!({"ok":true}))));
    }
    if method == "POST" && action == "trigger" {
        return Ok(Some(match trigger(app, &row.doc, &user, "manual") {
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
        if let Ok(queue) = queue_for(app, &user) {
            let _ = sync_locked(app, &queue, &user);
        }
        return Ok(Some((200, json!({"ok":true}))));
    }
    Ok(Some((405, json!({"error":"Método de tarea no permitido"}))))
}

const FIELDS: [&str; 12] = [
    "name",
    "task",
    "task_type",
    "interval_sec",
    "cron_expression",
    "timezone",
    "dom_and_dow",
    "fire_at",
    "start_at",
    "stop_at",
    "max_runs",
    "channel",
];

/// Check the editable fields of a task definition before it is stored. A definition that
/// would only fail later (an impossible cron, an interval of zero, a tool with no adapter)
/// is refused now, with the reason, instead of creating a task that silently never runs.
fn validate_definition(doc: &Value) -> std::result::Result<(), String> {
    let text = |key: &str, max: usize| {
        doc[key]
            .as_str()
            .is_none_or(|v| v.len() <= max && !v.chars().any(char::is_control))
    };
    if doc["name"]
        .as_str()
        .is_none_or(|name| name.trim().is_empty())
        || !text("name", 200)
    {
        return Err("name es obligatorio (hasta 200 caracteres)".into());
    }
    if doc["task"]
        .as_str()
        .is_none_or(|task| task.trim().is_empty() || task.len() > 100_000)
    {
        return Err("task es obligatorio (hasta 100000 caracteres)".into());
    }
    if doc["tool_name"]
        .as_str()
        .is_some_and(|tool| !tool.is_empty())
    {
        return Err("Las tareas con herramienta externa no están soportadas".into());
    }
    if doc["channel"].as_str().is_some_and(|channel| {
        !matches!(channel, "auto" | "webchat" | "")
            && channel != "all"
            && !channels::DELIVERABLE.contains(&channel)
    }) {
        return Err("channel admite auto, webchat, all (todos los canales activos), telegram, discord, slack o whatsapp".into());
    }
    if !text("cron_expression", 200) || !text("timezone", 100) {
        return Err("cron_expression o timezone no válidos".into());
    }
    if doc["max_runs"]
        .as_u64()
        .is_some_and(|runs| runs == 0 || runs > 10_000)
        || (!doc["max_runs"].is_null() && doc["max_runs"].as_u64().is_none())
    {
        return Err("max_runs debe estar entre 1 y 10000".into());
    }
    for key in ["start_at", "stop_at"] {
        if !doc[key].is_null() && timestamp(&doc[key]).is_none() {
            return Err(format!("{key} debe ser una fecha RFC 3339 o milisegundos"));
        }
    }
    let kind = doc["task_type"].as_str().unwrap_or_default();
    if !matches!(kind, "interval" | "one_shot" | "recurring") {
        return Err("task_type debe ser interval, one_shot o recurring".into());
    }
    next(doc, server::now())
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn create_task(app: &App, user: &str, body: &[u8]) -> Result<(u16, Value)> {
    let body: Value = serde_json::from_slice(body)?;
    let Some(fields) = body.as_object() else {
        return Ok((400, json!({"error":"Se esperaba un objeto"})));
    };
    if let Some(unknown) = fields.keys().find(|key| !FIELDS.contains(&key.as_str())) {
        return Ok((
            400,
            json!({"error":format!("Campo no permitido: {unknown}")}),
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let mut doc = json!({"id":id,"user_id":user,"status":"active","run_count":0,"error_count":0,"created_at":stamp(server::now())});
    doc.as_object_mut().expect("object").extend(fields.clone());
    if let Err(reason) = validate_definition(&doc) {
        return Ok((400, json!({"error":reason})));
    }
    // The first occurrence, as the desktop shows it; the shared scheduler owns it from here.
    if let Ok(Some(at)) = next(&doc, server::now()) {
        doc["next_run_at"] = json!(stamp(at));
    }
    app.db.col_put(
        "cronJobs",
        &id,
        &doc,
        PutOptions {
            expected_version: Some(0),
        },
    )?;
    if let Ok(queue) = queue_for(app, user) {
        let _ = sync_locked(app, &queue, user);
    }
    Ok((
        201,
        json!({"task":app.db.col_get("cronJobs", &id)?.map(|row| row.doc)}),
    ))
}

/// Edit the definition. Status, ownership and the run counters are not editable (pause and
/// resume change the status). The mirrored schedule is immutable, so an edit that changes
/// when the task fires creates a new one and retires the old; runs already made stay in the history.
fn edit_task(
    app: &App,
    user: &str,
    current: &Value,
    version: u64,
    body: &[u8],
) -> Result<(u16, Value)> {
    let body: Value = serde_json::from_slice(body)?;
    let Some(fields) = body.as_object().filter(|fields| !fields.is_empty()) else {
        return Ok((
            400,
            json!({"error":"Se esperaba un objeto con campos a cambiar"}),
        ));
    };
    if let Some(unknown) = fields.keys().find(|key| !FIELDS.contains(&key.as_str())) {
        return Ok((
            400,
            json!({"error":format!("Campo no editable: {unknown}")}),
        ));
    }
    let mut doc = current.clone();
    doc.as_object_mut().expect("object").extend(fields.clone());
    // Switching type leaves the other type's fields behind; they are ignored, not erased.
    if let Err(reason) = validate_definition(&doc) {
        return Ok((400, json!({"error":reason})));
    }
    let id = config::string(current, "id")?.to_owned();
    if let Ok(Some(at)) = next(&doc, server::now()) {
        doc["next_run_at"] = json!(stamp(at));
    }
    doc["last_error"] = Value::Null;
    app.db.col_put(
        "cronJobs",
        &id,
        &doc,
        PutOptions {
            expected_version: Some(version),
        },
    )?;
    if let Ok(queue) = queue_for(app, user) {
        let _ = sync_locked(app, &queue, user);
    }
    Ok((
        200,
        json!({"task":app.db.col_get("cronJobs", &id)?.map(|row| row.doc)}),
    ))
}

/// Queue one run by hand. Refused while a run of the task is queued, running or
/// uncertain, so a manual trigger can never replay interrupted work.
fn trigger(app: &App, task: &Value, user: &str, reason: &str) -> Result<()> {
    let id = config::string(task, "id")?;
    let queue = queue_for(app, user)?;
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
fn sync_locked(app: &App, queue: &DurableQueue, user: &str) -> Result<()> {
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
            || task_owner(app, &row)? != user
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
        if let Some(delivery) = job
            .result
            .as_ref()
            .map(|r| &r["delivery"])
            .filter(|d| d.is_object())
        {
            doc["delivery"] = delivery.clone();
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
    user: &str,
) -> Result<Vec<tokio::task::JoinHandle<()>>> {
    let mut pool = WorkerPool::new(queue.clone(), format!("gateway-cron:{user}"))?;
    let host = app.clone();
    let owner = user.to_owned();
    pool.register(KIND, move |job: Job, _token: CancellationToken| {
        let host = host.clone();
        let owner = owner.clone();
        async move {
            let task = job.input.payload["task_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            Ok(match execute(host, &owner, &task).await {
                Ok((text, delivery)) => JobOutcome::Completed {
                    result: json!({"text":bounded(&text),"delivery":delivery}),
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
fn cycle(app: &App, queue: &DurableQueue, projection: &mut Projection, user: &str) -> Result<()> {
    let _guard = app
        .activity_lock
        .write()
        .map_err(|_| "Scheduler bloqueado")?;
    // Project first: a run that just became uncertain must disable its schedule in this
    // same cycle, not one second later.
    projection.apply(app, queue, user)?;
    sync_locked(app, queue, user)?;
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

/// One user's scheduler: their queue, its event cursor and their workers.
struct UserCron {
    queue: DurableQueue,
    projection: Option<Projection>,
    workers: Vec<tokio::task::JoinHandle<()>>,
}

pub(crate) async fn run(app: Arc<App>) {
    adopt_legacy_runs(&app);
    let cancel = CancellationToken::new();
    let mut users: std::collections::HashMap<String, UserCron> = Default::default();
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {_=app.shutdown.cancelled()=>break,_=clock.tick()=>{}}
        // Every user has their own queue and workers; users added later are picked up here.
        let ids: Vec<String> = config::docs(&app.db, "users")
            .unwrap_or_default()
            .iter()
            .filter_map(|row| row["id"].as_str().map(str::to_owned))
            .collect();
        for id in ids {
            if !users.contains_key(&id) {
                let Ok(queue) = queue_for(&app, &id) else {
                    continue;
                };
                let Ok(workers) = start_workers(&app, &queue, &cancel, &id) else {
                    continue;
                };
                users.insert(
                    id.clone(),
                    UserCron {
                        queue,
                        projection: Some(Projection { after: 0 }),
                        workers,
                    },
                );
            }
            let Some(entry) = users.get_mut(&id) else {
                continue;
            };
            let (host, q, user) = (app.clone(), entry.queue.clone(), id.clone());
            let mut state = entry.projection.take().unwrap_or(Projection { after: 0 });
            let result = tokio::task::spawn_blocking(move || {
                let outcome = cycle(&host, &q, &mut state, &user);
                (state, outcome)
            })
            .await;
            entry.projection = Some(match result {
                Ok((state, _)) => state,
                Err(_) => Projection { after: 0 },
            });
        }
    }
    cancel.cancel();
    for (_, entry) in users {
        for handle in entry.workers {
            let _ = handle.await;
        }
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
async fn execute(app: Arc<App>, user: &str, id: &str) -> Result<(String, Option<Value>)> {
    let row = app
        .db
        .col_get("cronJobs", id)?
        .ok_or("La tarea fue eliminada")?
        .doc;
    if row["tool_name"].as_str().is_some_and(|s| !s.is_empty()) {
        return Err("Esta tarea necesita una herramienta externa aún no integrada".into());
    }
    let user = user.to_owned();
    if task_owner(&app, &row)? != user {
        return Err("La tarea pertenece a otro usuario".into());
    }
    let target = delivery_target(&row);
    if row["agent_id"].as_str().is_some_and(|id| !id.is_empty()) {
        let selected = app.selection_for_user(&user)?;
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
        user.clone(),
        thread,
        config::string(&row, "task")?.into(),
        tx,
        false,
    )?;
    let result = tokio::select! {_=app.shutdown.cancelled()=>return Err(UncertainTask("Gateway detenido durante la tarea".into()).into()),result=rx.recv()=>result.ok_or_else(||UncertainTask("La tarea cerró sin resultado".into()))?};
    let name = row["name"].as_str().unwrap_or("tarea").to_owned();
    if result["ok"] == true {
        let text: String = result["content"].as_str().unwrap_or_default().into();
        // The result also goes to the channels the task asked for, so it is not left in a
        // conversation nobody is reading.
        let report = match &target {
            Some(target) => {
                Some(channels::deliver(&app, &user, target, &format!("⏰ {name}\n\n{text}")).await)
            }
            None => None,
        };
        Ok((text, report))
    } else {
        // A failed or uncertain run is reported too (in plain words, without internals).
        if let Some(target) = &target {
            let alert = format!(
                "⚠️ La tarea programada «{name}» no se pudo completar. Revisa su historial antes de repetirla."
            );
            let _ = channels::deliver(&app, &user, target, &alert).await;
        }
        Err(UncertainTask(
            result["error"]
                .as_str()
                .unwrap_or("Falló la tarea después del despacho")
                .to_owned(),
        )
        .into())
    }
}

/// Hooks for the multi-user tests in `server::tests`.
#[cfg(test)]
pub(crate) fn stamp_for_tests(ms: u64) -> String {
    stamp(ms)
}
#[cfg(test)]
pub(crate) fn queue_for_tests(app: &App, user: &str) -> DurableQueue {
    queue_for(app, user).unwrap()
}
#[cfg(test)]
pub(crate) fn sync_for_tests(app: &App, queue: &DurableQueue, user: &str) {
    let _guard = app.activity_lock.write().unwrap();
    sync_locked(app, queue, user).unwrap();
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
        cycle(app, queue, projection, "u1").unwrap();
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
        cycle(app, queue, projection, "u1").unwrap();
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
        let queue = queue_for(&app, "u1").unwrap();
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
        assert!(trigger(&app, &row(&app, "task"), "u1", "manual").is_err());
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
        let queue = queue_for(&app, "u1").unwrap();
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
        let queue = queue_for(&app, "u1").unwrap();
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
        let call =
            |method: &str, path: &str| route(&app, method, path, "", b"", "u1").unwrap().unwrap();
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
        let queue = queue_for(&app, "u1").unwrap();
        let mut projection = Projection { after: 0 };
        let call = |path: &str| route(&app, "POST", path, "", b"", "u1").unwrap().unwrap();
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
        let queue = queue_for(&app, "u1").unwrap();
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

    #[test]
    fn tasks_can_be_created_edited_and_inspected_through_the_api_and_are_validated_first() {
        let (_folder, app) = open();
        let api = |method: &str, path: &str, body: Value, user: &str| {
            route(&app, method, path, "", body.to_string().as_bytes(), user)
                .unwrap()
                .unwrap()
        };
        let (status, created) = api(
            "POST",
            "/api/cron",
            json!({"name":"Resumen","task":"Resume las noticias","task_type":"interval","interval_sec":3600}),
            "u1",
        );
        assert_eq!(status, 201, "{created}");
        let task = &created["task"];
        let id = task["id"].as_str().unwrap().to_owned();
        assert_eq!(
            (
                task["user_id"].clone(),
                task["status"].clone(),
                task["run_count"].clone()
            ),
            (json!("u1"), json!("active"), json!(0))
        );
        assert!(
            task["next_run_at"].is_string()
                && task["schedule_id"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("cron.{id}.")),
            "the schedule exists at once: {task}"
        );
        // The shared scheduler's view of it.
        let (_, inspected) = api("GET", &format!("/api/cron/{id}/schedule"), json!({}), "u1");
        assert_eq!(
            (
                inspected["schedule"]["enabled"].clone(),
                inspected["schedule"]["status"].clone()
            ),
            (json!(true), json!("active"))
        );
        assert_eq!(inspected["scheduleId"], task["schedule_id"]);
        // Nobody else sees, edits or inspects it.
        for (method, path) in [
            ("PATCH", format!("/api/cron/{id}")),
            ("GET", format!("/api/cron/{id}/schedule")),
        ] {
            assert_eq!(
                api(method, &path, json!({"name":"x"}), "u2").0,
                404,
                "{method} {path}"
            );
        }
        assert_eq!(
            api("GET", "/api/cron", json!({}), "u2").1["tasks"],
            json!([])
        );
        // Editing when it fires retires the old schedule and creates a new one.
        let first_schedule = task["schedule_id"].as_str().unwrap().to_owned();
        let (status, edited) = api(
            "PATCH",
            &format!("/api/cron/{id}"),
            json!({"interval_sec":7200,"name":"Resumen largo"}),
            "u1",
        );
        assert_eq!(status, 200, "{edited}");
        assert_eq!(
            (
                edited["task"]["name"].clone(),
                edited["task"]["interval_sec"].clone()
            ),
            (json!("Resumen largo"), json!(7200))
        );
        assert_ne!(edited["task"]["schedule_id"], json!(first_schedule));
        let queue = queue_for(&app, "u1").unwrap();
        let schedules = queue.list_schedules(50, None).unwrap();
        let enabled: Vec<&str> = schedules["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["enabled"] == true)
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            (enabled.len(), schedules["items"].as_array().unwrap().len()),
            (1, 2),
            "one live schedule, the old one retired: {schedules}"
        );
        assert_eq!(enabled[0], edited["task"]["schedule_id"].as_str().unwrap());
        // Switching to a cron expression works too.
        let (status, cron) = api(
            "PATCH",
            &format!("/api/cron/{id}"),
            json!({"task_type":"recurring","cron_expression":"0 8 * * *","timezone":"America/Bogota"}),
            "u1",
        );
        assert_eq!(status, 200, "{cron}");
        // What is not editable: ownership, status, counters and unknown fields.
        for bad in [
            json!({"user_id":"u2"}),
            json!({"status":"completed"}),
            json!({"run_count":99}),
            json!({"id":"other"}),
            json!({"bogus":1}),
            json!({}),
        ] {
            assert_eq!(
                api("PATCH", &format!("/api/cron/{id}"), bad.clone(), "u1").0,
                400,
                "{bad}"
            );
        }
        assert_eq!(
            app.db.col_get("cronJobs", &id).unwrap().unwrap().doc["user_id"],
            "u1"
        );
    }

    #[test]
    fn invalid_definitions_are_refused_at_creation_and_on_edit_not_left_to_fail_later() {
        let (_folder, app) = open();
        let api = |method: &str, path: &str, body: Value| {
            route(&app, method, path, "", body.to_string().as_bytes(), "u1")
                .unwrap()
                .unwrap()
        };
        let base =
            json!({"name":"Tarea","task":"Haz algo","task_type":"interval","interval_sec":60});
        let with = |extra: Value| {
            let mut value = base.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().cloned().unwrap());
            value
        };
        for (bad, why) in [
            (with(json!({"interval_sec":0})), "interval of zero"),
            (
                with(json!({"interval_sec":999_999_999})),
                "interval beyond a year",
            ),
            (
                with(
                    json!({"task_type":"recurring","cron_expression":"80 * * * *","timezone":"UTC"}),
                ),
                "impossible cron",
            ),
            (
                with(
                    json!({"task_type":"recurring","cron_expression":"* * * * *","timezone":"Mars/Base"}),
                ),
                "unknown timezone",
            ),
            (
                with(json!({"task_type":"one_shot"})),
                "one-shot without a date",
            ),
            (with(json!({"task_type":"daily"})), "unknown type"),
            (with(json!({"tool_name":"unsupported"})), "external tool"),
            (
                with(json!({"channel":"email"})),
                "a channel that is not one of ours",
            ),
            (with(json!({"max_runs":0})), "zero runs"),
            (with(json!({"start_at":"mañana"})), "bad date"),
            (with(json!({"name":"  "})), "empty name"),
            (with(json!({"task":""})), "empty task"),
            (with(json!({"user_id":"u2"})), "choosing the owner"),
            (with(json!({"status":"completed"})), "choosing the status"),
            (json!({"name":"x"}), "missing fields"),
            (json!([1, 2]), "not an object"),
        ] {
            let (status, body) = api("POST", "/api/cron", bad.clone());
            assert_eq!(status, 400, "{why}: {body}");
        }
        assert!(
            config::docs(&app.db, "cronJobs").unwrap().is_empty(),
            "nothing invalid was stored"
        );
        let (_, created) = api("POST", "/api/cron", base.clone());
        let id = created["task"]["id"].as_str().unwrap().to_owned();
        assert_eq!(
            api(
                "PATCH",
                &format!("/api/cron/{id}"),
                json!({"interval_sec":0})
            )
            .0,
            400
        );
        assert_eq!(
            app.db.col_get("cronJobs", &id).unwrap().unwrap().doc["interval_sec"],
            60,
            "a refused edit changes nothing"
        );
        // A one-shot and a bounded recurring task are fine.
        let at = stamp(server::now() + 3_600_000);
        assert_eq!(
            api(
                "POST",
                "/api/cron",
                with(json!({"task_type":"one_shot","fire_at":at}))
            )
            .0,
            201
        );
        assert_eq!(
            api(
                "POST",
                "/api/cron",
                with(json!({"max_runs":3,"stop_at":stamp(server::now() + 86_400_000)}))
            )
            .0,
            201
        );
    }
}
