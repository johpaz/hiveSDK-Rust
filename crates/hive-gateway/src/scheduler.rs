//! Durable, native task scheduling. Runs are claimed before execution and never
//! replayed automatically after an uncertain shutdown.
use crate::{
    Result,
    config::{self, App},
    management, server,
};
use chrono::{DateTime, Utc};
use hivedb_core::{ColOp, PutOptions};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
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
fn next(row: &Value, now: u64) -> Result<Option<u64>> {
    use hive_agent_core::scheduler::Schedule;
    let schedule = match row["task_type"].as_str().unwrap_or("recurring") {
        "interval" => {
            let sec = row["interval_sec"]
                .as_u64()
                .filter(|n| *n > 0 && *n <= 31_536_000)
                .ok_or("Intervalo inválido")?;
            Schedule::Interval {
                every_ms: sec * 1000,
                anchor_unix_ms: None,
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
    };
    let next = schedule.next_after(now)?;
    if matches!(schedule, Schedule::Cron { .. }) && next.is_none() {
        return Err("No hay una fecha cron válida en ocho años".into());
    }
    Ok(next)
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
            json!({"running":true,"backend":"rust","automatic":true}),
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
        return Ok(Some((200, json!({"ok":true}))));
    }
    if method == "POST" && action == "trigger" {
        return Ok(Some(match queue(app, &row.doc, &user, "manual") {
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
        return Ok(Some((200, json!({"ok":true}))));
    }
    let _ = body;
    Ok(Some((405, json!({"error":"Método de tarea no permitido"}))))
}
fn queue(app: &App, job: &Value, user: &str, reason: &str) -> Result<()> {
    if config::docs(&app.db, "taskRuns")?.iter().any(|run| {
        run["task_id"] == job["id"]
            && matches!(
                run["status"].as_str(),
                Some("queued" | "running" | "uncertain")
            )
    }) {
        return Err("La tarea ya está en cola o en ejecución".into());
    }
    let id = uuid::Uuid::new_v4().to_string();
    app.db.col_put("taskRuns",&id,&json!({"id":id,"task_id":job["id"],"user_id":user,"status":"queued","started_at":stamp(server::now()),"native_runtime":true,"reason":reason}),PutOptions{expected_version:Some(0)})?;
    Ok(())
}
pub(crate) async fn run(app: Arc<App>) {
    // A previously running task has an uncertain outcome. Preserve it for review.
    if let Ok(rows) = config::docs(&app.db, "taskRuns") {
        for row in rows {
            if row["native_runtime"] == true && row["status"] == "running" {
                let _ = management::patch(
                    &app,
                    "taskRuns",
                    row["id"].as_str().unwrap_or_default(),
                    &json!({"status":"uncertain","error":"Ejecución interrumpida; revisar antes de reintentar"}),
                    &[("status", "status"), ("error", "error")],
                );
            }
        }
    }
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {_=app.shutdown.cancelled()=>return,_=clock.tick()=>{}}
        let host = app.clone();
        let _ = tokio::task::spawn_blocking(move || schedule(&host, server::now())).await;
        let pending = config::docs(&app.db, "taskRuns")
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r["native_runtime"] == true && r["status"] == "queued")
            .take(8)
            .collect::<Vec<_>>();
        for run in pending {
            let host = app.clone();
            tokio::spawn(async move {
                let id = run["id"].as_str().unwrap_or_default().to_owned();
                let Some(task_id) = run["task_id"].as_str() else {
                    return;
                };
                let claimed = {
                    let Ok(_guard) = host.activity_lock.lock() else {
                        return;
                    };
                    match host.db.col_get("taskRuns", &id) {
                        Ok(Some(current)) if current.doc["status"] == "queued" => {
                            management::patch(
                                &host,
                                "taskRuns",
                                &id,
                                &json!({"status":"running"}),
                                &[("status", "status")],
                            )
                            .unwrap_or(false)
                        }
                        _ => false,
                    }
                };
                if !claimed {
                    return;
                }
                let result = execute(host.clone(), task_id).await;
                let failure = result.as_ref().err().map(|error| error.to_string());
                let uncertain = result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.is::<UncertainTask>());
                if let Ok(_guard) = host.activity_lock.lock()
                    && let Ok(Some(row)) = host.db.col_get("cronJobs", task_id)
                {
                    let _ = management::patch(
                        &host,
                        "cronJobs",
                        task_id,
                        &json!({"last_run_at":stamp(server::now()),"run_count":row.doc["run_count"].as_u64().unwrap_or(0).saturating_add(1),"error_count":row.doc["error_count"].as_u64().unwrap_or(0).saturating_add(u64::from(failure.is_some())),"last_error":failure,"status":if row.doc["task_type"]=="one_shot" {json!(if failure.is_some(){"error"}else{"completed"})}else{row.doc["status"].clone()}}),
                        &[
                            ("last_run_at", "last_run_at"),
                            ("run_count", "run_count"),
                            ("error_count", "error_count"),
                            ("last_error", "last_error"),
                            ("status", "status"),
                        ],
                    );
                }
                let _ = management::patch(
                    &host,
                    "taskRuns",
                    &id,
                    &match result {
                        Ok(text) => {
                            json!({"status":"completed","result":text,"finished_at":stamp(server::now())})
                        }
                        Err(e) => {
                            json!({"status":if uncertain {"uncertain"} else {"failed"},"error":e.to_string(),"finished_at":stamp(server::now())})
                        }
                    },
                    &[
                        ("status", "status"),
                        ("result", "result"),
                        ("error", "error"),
                        ("finished_at", "finished_at"),
                    ],
                );
            });
        }
    }
}
fn schedule(app: &App, now: u64) -> Result<()> {
    let _guard = app
        .activity_lock
        .lock()
        .map_err(|_| "Scheduler bloqueado")?;
    let user = management::owner(app)?;
    for row in app.db.col_scan("cronJobs", &Default::default())? {
        let mut doc = row.doc;
        let payload = doc["payload"]
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or_else(|| doc["payload"].clone());
        if payload["_internal"] == true {
            continue;
        }
        if !management::owned(&doc, &user)
            || doc["status"] != "active"
            || doc["name"].as_str().is_some_and(|s| s.starts_with("_hive"))
        {
            continue;
        }
        let exhausted = doc["max_runs"]
            .as_u64()
            .is_some_and(|limit| limit > 0 && doc["run_count"].as_u64().unwrap_or(0) >= limit);
        let expired = timestamp(&doc["stop_at"]).is_some_and(|at| at <= now);
        if exhausted || expired {
            doc["status"] = json!("completed");
            doc["next_run_at"] = Value::Null;
            app.db.col_put(
                "cronJobs",
                config::string(&doc, "id")?,
                &doc,
                PutOptions {
                    expected_version: Some(row.version),
                },
            )?;
            continue;
        }
        if timestamp(&doc["start_at"]).is_some_and(|at| at > now) {
            continue;
        }
        let busy = config::docs(&app.db, "taskRuns")?.iter().any(|run| {
            run["task_id"] == doc["id"]
                && matches!(
                    run["status"].as_str(),
                    Some("queued" | "running" | "uncertain")
                )
        });
        let due = timestamp(&doc["next_run_at"]);
        if due.is_some_and(|at| at > now) {
            continue;
        }
        // Do not replay old missed occurrences when a desktop profile is reopened.
        let mut ops = vec![];
        if !busy && due.is_some_and(|at| now.saturating_sub(at) < 60_000) {
            let id = uuid::Uuid::new_v4().to_string();
            ops.push(ColOp::Put{collection:"taskRuns".into(),id:id.clone(),doc:json!({"id":id,"task_id":doc["id"],"user_id":user,"status":"queued","started_at":stamp(now),"native_runtime":true,"reason":"scheduled"}),expected_version:Some(0)});
        }
        match next(&doc, now) {
            Ok(Some(at)) => doc["next_run_at"] = json!(stamp(at)),
            Ok(None) => {
                doc["status"] = json!(if ops.is_empty() { "missed" } else { "queued" });
                doc["next_run_at"] = Value::Null;
            }
            Err(e) => {
                doc["status"] = json!("error");
                doc["last_error"] = json!(e.to_string());
            }
        }
        ops.push(ColOp::Put {
            collection: "cronJobs".into(),
            id: config::string(&doc, "id")?.into(),
            doc,
            expected_version: Some(row.version),
        });
        app.db.col_batch(&ops)?;
    }
    Ok(())
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
    #[tokio::test]
    async fn interrupted_scheduled_work_stays_uncertain_and_blocks_automatic_or_manual_replay() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(folder.path().join("data")).unwrap();
        {
            let db = hivedb_core::HiveDB::open(folder.path().join("data/hivedb")).unwrap();
            db.col_put("users", "u1", &json!({"id":"u1"}), Default::default())
                .unwrap();
            db.col_put("cronJobs", "task", &json!({"id":"task","user_id":"u1","name":"Fixture","status":"active","task_type":"interval","interval_sec":60,"next_run_at":stamp(server::now()-1000)}),Default::default()).unwrap();
            db.col_put(
                "taskRuns",
                "old",
                &json!({"id":"old","task_id":"task","native_runtime":true,"status":"running"}),
                Default::default(),
            )
            .unwrap();
        }
        let app = Arc::new(App::open(folder.path().to_owned()).unwrap());
        let host = app.clone();
        let runtime = tokio::spawn(async move { run(host).await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if app.db.col_get("taskRuns", "old").unwrap().unwrap().doc["status"] == "uncertain" {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        schedule(&app, server::now()).unwrap();
        assert_eq!(config::docs(&app.db, "taskRuns").unwrap().len(), 1);
        assert!(
            queue(
                &app,
                &app.db.col_get("cronJobs", "task").unwrap().unwrap().doc,
                "u1",
                "manual"
            )
            .is_err()
        );
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
    fn desktop_cron_advances_and_queues_atomically_without_duplicate_occurrences() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(folder.path().join("data")).unwrap();
        {
            let db = hivedb_core::HiveDB::open(folder.path().join("data/hivedb")).unwrap();
            db.col_put("users", "u1", &json!({"id":"u1"}), Default::default())
                .unwrap();
        }
        let app = App::open(folder.path().to_owned()).unwrap();
        let now = 1_800_000_000_000;
        for (id, user) in [("mine", "u1"), ("foreign", "u2")] {
            app.db.col_put("cronJobs",id,&json!({"id":id,"user_id":user,"name":"Tarea","status":"active","task_type":"interval","interval_sec":60,"next_run_at":stamp(now-1000),"task":"Resume"}),Default::default()).unwrap();
        }
        schedule(&app, now).unwrap();
        schedule(&app, now).unwrap();
        let runs = config::docs(&app.db, "taskRuns").unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["task_id"], "mine");
        assert_eq!(runs[0]["status"], "queued");
        assert!(
            timestamp(&app.db.col_get("cronJobs", "mine").unwrap().unwrap().doc["next_run_at"])
                .unwrap()
                > now
        );
        // Reopening after a long gap skips missed occurrences instead of replaying them.
        schedule(&app, now + 86_400_000).unwrap();
        assert_eq!(config::docs(&app.db, "taskRuns").unwrap().len(), 1);
    }
    #[tokio::test]
    async fn desktop_cron_runtime_records_unsupported_actions_without_faking_success() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(folder.path().join("data")).unwrap();
        {
            let db = hivedb_core::HiveDB::open(folder.path().join("data/hivedb")).unwrap();
            db.col_put("users", "u1", &json!({"id":"u1"}), Default::default())
                .unwrap();
        }
        let app = Arc::new(App::open(folder.path().to_owned()).unwrap());
        let due = server::now() - 1000;
        app.db.col_put("cronJobs","one",&json!({"id":"one","user_id":"u1","name":"Comprobación","task_type":"one_shot","task":"No ejecutar","tool_name":"unsupported","status":"active","next_run_at":stamp(due),"fire_at":stamp(due)}),Default::default()).unwrap();
        let host = app.clone();
        let runtime = tokio::spawn(async move { run(host).await });
        let limit = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if app.db.col_get("cronJobs", "one").unwrap().unwrap().doc["status"] == "error" {
                break;
            }
            assert!(tokio::time::Instant::now() < limit, "task did not settle");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        app.shutdown.cancel();
        runtime.await.unwrap();
        let row = app.db.col_get("cronJobs", "one").unwrap().unwrap().doc;
        assert_eq!(row["run_count"], 1);
        assert_eq!(row["error_count"], 1);
        let runs = config::docs(&app.db, "taskRuns").unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["status"], "failed");
    }
}
