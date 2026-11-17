//! Durable narration and office snapshots. HiveDB is authoritative; broadcast is delivery only.
use crate::{
    Result,
    config::{self, App},
    server::now,
};
use hivedb_core::{ColOp, HiveDB};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const RUNS: &str = "hive_sdk_rust_activity_v1";
const REVISION: &str = "hive_sdk_rust_office_revision";

#[derive(Clone)]
pub struct Progress {
    pub app: Arc<App>,
    pub run: String,
    pub root: String,
    pub thread: String,
    pub user: String,
    pub agent: String,
    pub name: String,
    pub task: String,
    pub parent: Option<String>,
}

impl Progress {
    pub async fn emit(
        &self,
        key: &str,
        kind: &str,
        phase: &str,
        status: &str,
        label: &str,
        detail: Option<String>,
    ) -> Result<Value> {
        let progress = self.clone();
        let (key, kind, phase, status, label) = (
            key.to_owned(),
            kind.to_owned(),
            phase.to_owned(),
            status.to_owned(),
            label.to_owned(),
        );
        tokio::task::spawn_blocking(move || {
            progress.persist(&key, &kind, &phase, &status, &label, detail)
        })
        .await?
    }

    fn persist(
        &self,
        key: &str,
        kind: &str,
        phase: &str,
        status: &str,
        label: &str,
        detail: Option<String>,
    ) -> Result<Value> {
        let _guard = self
            .app
            .activity_lock
            .write()
            .map_err(|_| "Actividad bloqueada")?;
        let db = &self.app.db;
        let id = hex::encode(Sha256::digest(format!("{}\0{key}", self.run)));
        let logical = if key == "start"
            || key.starts_with("writing:")
            || matches!(key, "completed" | "failed" | "aborted")
        {
            "activity"
        } else {
            key.strip_suffix(":call")
                .or_else(|| key.strip_suffix(":result"))
                .unwrap_or(key)
        };
        let step_id = format!("{}:{logical}", self.run);
        let mut event = json!({"id":id,"turn_id":self.run,"thread_id":self.thread,"channel":"webchat",
            "root_turn_id":self.root,"step_id":step_id,"phase":phase,
            "user_id":self.user,"session_id":self.user,"agent_id":self.agent,"agent_name":self.name,
            "kind":kind,"status":status,"label":label,"detail":detail,"dedupe_key":key,"created_at":now()});
        if let Some(stored) = db.col_get("narrationEvents", &id)? {
            return narration_frame(db, &stored.doc);
        }
        {
            let previous = db.col_get(RUNS, &self.run)?;
            let counter = db.col_get("counters", REVISION)?;
            let revision = counter
                .as_ref()
                .and_then(|e| e.doc["value"].as_u64())
                .unwrap_or(0)
                .checked_add(1)
                .ok_or("Revisión de oficina agotada")?;
            let terminal = matches!(phase, "completed" | "failed" | "aborted");
            event["revision"] = json!(revision);
            let row = json!({"id":self.run,"root_id":self.root,"thread_id":self.thread,"user_id":self.user,
                "agent_id":self.agent,"agent_name":self.name,"parent_agent_id":self.parent,"task":self.task,
                "phase":phase,"status":if terminal { status } else { "running" },"label":label,
                "updated_at":now(),"revision":revision});
            db.col_batch(&[
                ColOp::Put {
                    collection: RUNS.into(),
                    id: self.run.clone(),
                    doc: row,
                    expected_version: Some(previous.map_or(0, |r| r.version)),
                },
                ColOp::Put {
                    collection: "narrationEvents".into(),
                    id: id.clone(),
                    doc: event.clone(),
                    expected_version: Some(0),
                },
                ColOp::Put {
                    collection: "counters".into(),
                    id: REVISION.into(),
                    doc: json!({"value":revision}),
                    expected_version: Some(counter.map_or(0, |r| r.version)),
                },
            ])?;
            let _ = self.app.office_events.send(snapshot(db, &self.user)?);
            let (actor, target, work_phase) =
                if phase == "thinking" && self.parent.is_some() && key == "start" {
                    (
                        self.parent.clone().unwrap_or_default(),
                        Some(self.agent.clone()),
                        "delegated",
                    )
                } else {
                    (self.agent.clone(), self.parent.clone(), phase)
                };
            let _ = self.app.office_events.send(json!({"type":"canvas:work_event","userId":self.user,"timestamp":event["created_at"],
                "data":{"eventId":id,"phase":work_phase,"actorId":actor,"targetId":target,
                    "taskRef":self.run,"taskName":self.task,"revision":revision,"detail":label,"toolName":if phase=="tool_call" {Some("task_delegate")} else {None}}}));
        }
        narration_frame(db, &event)
    }
}

pub fn narration_frame(db: &HiveDB, event: &Value) -> Result<Value> {
    let run = event["turn_id"].as_str().unwrap_or_default();
    let root = match event["root_turn_id"].as_str() {
        Some(root) => root.to_owned(),
        None => db
            .col_get(RUNS, run)?
            .and_then(|row| row.doc["root_id"].as_str().map(str::to_owned))
            .unwrap_or_else(|| run.to_owned()),
    };
    Ok(
        json!({"type":"process","id":event["id"],"stepId":event.get("step_id").unwrap_or(&event["id"]),
        "messageId":root,"threadId":event["thread_id"],"agentId":event["agent_id"],"agentName":event["agent_name"],
        "processKind":match event["phase"].as_str() {Some("writing")=>"writing",Some("tool_call")=>"tool",_=>"analysis"},
        "narrationKind":event["kind"],"processStatus":match event["status"].as_str() {Some("done")=>"done",Some("error")=>"error",_=>"thinking"},
        "label":event["label"],"detail":event["detail"],"timestamp":event["created_at"],"turnDone":false}),
    )
}

pub fn agents(db: &HiveDB, user: &str) -> Result<Vec<Value>> {
    Ok(config::docs(db,"agents")?.into_iter().filter(|a| config::agent_visible(a,user)).map(|a| json!({
        "id":a["id"],"name":a["name"],"description":a["description"],"role":a["role"],"status":a["status"],
        "enabled":a["enabled"],"providerId":a["provider_id"],"modelId":a["model_id"],
        "systemPrompt":a["system_prompt"],"toolsJson":a["tools_json"],"skillsJson":a["skills_json"],
        "maxIterations":a["max_iterations"],"workspace":a["workspace"]})).collect())
}

pub fn snapshot(db: &HiveDB, user: &str) -> Result<Value> {
    let runs: Vec<Value> = config::docs(db, RUNS)?
        .into_iter()
        .filter(|r| r["user_id"] == user)
        .collect();
    let roster = agents(db, user)?;
    let nodes: Vec<Value> = roster
        .iter()
        .map(|agent| {
            let active = runs
                .iter()
                .filter(|r| r["agent_id"] == agent["id"] && r["status"] == "running")
                .max_by_key(|r| {
                    (
                        r["phase"] == "tool_call",
                        r["revision"].as_u64().unwrap_or(0),
                    )
                });
            let latest = runs
                .iter()
                .filter(|r| r["agent_id"] == agent["id"])
                .max_by_key(|r| r["revision"].as_u64().unwrap_or(0));
            let status = active.map_or_else(
                || {
                    if latest.is_some_and(|r| r["status"] == "error") {
                        "error"
                    } else {
                        "idle"
                    }
                },
                |r| {
                    if r["phase"] == "tool_call" {
                        "tool_call"
                    } else {
                        "thinking"
                    }
                },
            );
            json!({"id":agent["id"],"type":"agent","label":agent["name"],"data":{"status":status,
            "currentTask":active.map(|r|r["task"].clone()),"taskId":active.map(|r|r["id"].clone()),
            "currentTool":active.filter(|r|r["phase"]=="tool_call").map(|_|"task_delegate"),
            "delegatedBy":active.and_then(|r|r["parent_agent_id"].as_str())}})
        })
        .collect();
    let edges: Vec<Value> = runs.iter().filter(|r| r["status"]=="running").filter_map(|r| {
        let parent = r["parent_agent_id"].as_str()?;
        Some(json!({"id":format!("{}:delegates",r["id"].as_str().unwrap_or_default()),"source":parent,"target":r["agent_id"],
            "edgeType":"delegates","data":{"taskId":r["id"],"taskName":r["task"]}}))
    }).collect();
    let revision = db
        .col_get("counters", REVISION)?
        .map_or(0, |r| r.doc["value"].as_u64().unwrap_or(0));
    Ok(
        json!({"type":"canvas:snapshot","userId":user,"timestamp":now(),"data":{"nodes":nodes,"edges":edges,"agents":roster,"revision":revision}}),
    )
}

pub async fn current(app: Arc<App>, user: String) -> Result<Value> {
    tokio::task::spawn_blocking(move || {
        let _guard = app
            .activity_lock
            .write()
            .map_err(|_| "Actividad bloqueada")?;
        snapshot(&app.db, &user)
    })
    .await?
}

/// A crashed run is not resumed or replayed by opening the office.
pub fn reconcile(db: &HiveDB) -> Result<()> {
    for mut row in db.col_scan(RUNS, &Default::default())? {
        if row.doc["status"] == "running" {
            let counter = db.col_get("counters", REVISION)?;
            let revision = counter
                .as_ref()
                .and_then(|r| r.doc["value"].as_u64())
                .unwrap_or(0)
                .checked_add(1)
                .ok_or("Revisión de oficina agotada")?;
            row.doc["status"] = json!("error");
            row.doc["phase"] = json!("failed");
            row.doc["label"] = json!("Ejecución interrumpida al reiniciar; requiere revisión");
            row.doc["revision"] = json!(revision);
            row.doc["updated_at"] = json!(now());
            db.col_batch(&[
                ColOp::Put {
                    collection: RUNS.into(),
                    id: row.id,
                    doc: row.doc,
                    expected_version: Some(row.version),
                },
                ColOp::Put {
                    collection: "counters".into(),
                    id: REVISION.into(),
                    doc: json!({"value":revision}),
                    expected_version: Some(counter.map_or(0, |r| r.version)),
                },
            ])?;
        }
    }
    Ok(())
}
