//! Dashboard reads canonical Hive rollups; native writes share their transaction.
use crate::{
    Result,
    config::{self, App},
};
use hive_agent_core::Usage;
use hivedb_core::{ColOp, ScanOptions};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

pub struct Runtime {
    started: Instant,
    cpu: Mutex<Option<(Instant, u64)>>,
    connections: AtomicU64,
    activity_ready: Mutex<bool>,
}
impl Default for Runtime {
    fn default() -> Self {
        let started = Instant::now();
        Self {
            started,
            cpu: Mutex::new(cpu_ticks().map(|ticks| (started, ticks))),
            connections: AtomicU64::new(0),
            activity_ready: Mutex::new(false),
        }
    }
}
pub struct Connection<'a>(&'a AtomicU64);
impl Drop for Connection<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
impl Runtime {
    pub fn connect(&self) -> Connection<'_> {
        self.connections.fetch_add(1, Ordering::Relaxed);
        Connection(&self.connections)
    }
    fn stats(&self, app: &App, now: u64) -> Result<Value> {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        let cpu = cpu_ticks().and_then(|ticks| {
            let mut previous = self.cpu.lock().ok()?;
            let at = Instant::now();
            let result = previous.as_ref().and_then(|(sample, old)| {
                let elapsed = at.duration_since(*sample).as_secs_f64();
                (elapsed > 0.0).then(|| {
                    100.0 * ticks.saturating_sub(*old) as f64
                        / ticks_per_second()
                        / elapsed
                        / cores as f64
                })
            });
            *previous = Some((at, ticks));
            result.map(|cpu| cpu.clamp(0.0, 100.0))
        });
        let rss = rss_mb();
        let seconds = self.started.elapsed().as_secs();
        let recent = config::docs(&app.db, "conversations")?
            .iter()
            .filter(|row| {
                row["created_at"]
                    .as_u64()
                    .is_some_and(|at| at >= now.saturating_sub(300_000) && at <= now)
            })
            .count();
        Ok(json!({"runtime":"rust","cpu":cpu,"memory":{"rss":rss},
            "processMetricsAvailable":cpu.is_some() && rss.is_some(),
            "uptime":format!("{:02}:{:02}:{:02}",seconds/3600,(seconds/60)%60,seconds%60),
            "connections":self.connections.load(Ordering::Relaxed),"cores":cores,
            "activeRuns":app.active.lock().map_err(|_|"Turnos bloqueados")?.len(),"recentMessages":recent}))
    }
}

#[cfg(target_os = "linux")]
fn cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields: Vec<_> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    fields
        .get(11)?
        .parse::<u64>()
        .ok()?
        .checked_add(fields.get(12)?.parse().ok()?)
}
#[cfg(target_os = "linux")]
fn ticks_per_second() -> f64 {
    // SAFETY: sysconf reads a system constant and takes no pointers.
    unsafe { libc::sysconf(libc::_SC_CLK_TCK).max(1) as f64 }
}
#[cfg(target_os = "linux")]
fn rss_mb() -> Option<f64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_whitespace()
                .next()?
                .parse::<f64>()
                .ok()
                .map(|kb| kb / 1024.0)
        })
}
#[cfg(not(target_os = "linux"))]
fn cpu_ticks() -> Option<u64> {
    None
}
#[cfg(not(target_os = "linux"))]
fn ticks_per_second() -> f64 {
    1.0
}
#[cfg(not(target_os = "linux"))]
fn rss_mb() -> Option<f64> {
    None
}

pub fn hour_bucket(now: u64) -> Result<String> {
    let date = time::OffsetDateTime::from_unix_timestamp(i64::try_from(now / 1000)?)?;
    Ok(format!("{}T{:02}", date.date(), date.hour()))
}
fn buckets(now: u64, hours: u64) -> Result<Vec<String>> {
    (0..=hours)
        .rev()
        .map(|offset| hour_bucket(now.saturating_sub(offset * 3_600_000)))
        .collect()
}
fn hours(query: &str, default: u64) -> Option<u64> {
    match query
        .split('&')
        .find_map(|part| part.strip_prefix("hours="))
    {
        None => Some(default),
        Some(value) => value.parse().ok().filter(|value| (1..=168).contains(value)),
    }
}
pub fn route(app: &App, path: &str, query: &str, now: u64) -> Result<Option<(u16, Value)>> {
    let value = match path {
        "/api/system-stats" => app.dashboard.stats(app, now)?,
        "/api/version" => {
            json!({"current":env!("CARGO_PKG_VERSION"),"latest":"","status":"local","installationType":"binary","runtime":"rust"})
        }
        "/api/agents/proposals" => {
            let users = config::docs(&app.db, "users")?;
            let user = config::string(users.first().ok_or("Sin usuario")?, "id")?;
            let agents: BTreeMap<_, _> = config::docs(&app.db, "agents")?
                .into_iter()
                .filter(|agent| config::agent_visible(agent, user))
                .map(|agent| (agent["id"].as_str().unwrap_or_default().to_owned(), agent))
                .collect();
            let mut proposals: Vec<_> = config::docs(&app.db,"agentProposals")?.into_iter()
                .filter(|row|row["status"]=="proposed" && row["type"]=="disable_agent")
                .filter_map(|row| {
                    let agent = agents.get(row["agent_id"].as_str()?)?;
                    let change: Value = serde_json::from_str(row["proposed_change_json"].as_str().unwrap_or("")).unwrap_or(Value::Null);
                    Some(json!({"id":row["id"],"agentId":row["agent_id"],"agentName":agent["name"],"reason":change["reason"],"createdAt":row["created_at"]}))
                }).collect();
            proposals.sort_by_key(|row| std::cmp::Reverse(row["createdAt"].as_u64().unwrap_or(0)));
            json!({"proposals":proposals})
        }
        "/api/activity-stats" | "/api/usage-stats" => {
            let Some(hours) = hours(
                query,
                if path == "/api/activity-stats" {
                    12
                } else {
                    24
                },
            ) else {
                return Ok(Some((
                    400,
                    json!({"error":"hours debe ser un entero entre 1 y 168"}),
                )));
            };
            let buckets = buckets(now, hours)?;
            if path == "/api/activity-stats" {
                backfill_activity(app)?;
                let mut points = Vec::new();
                for bucket in buckets {
                    let count = app
                        .db
                        .col_get("activityRollups", &bucket)?
                        .and_then(|entry| entry.doc["messageCount"].as_u64())
                        .unwrap_or(0);
                    points.push(json!({"time":format!("{} {}:00",&bucket[..10],&bucket[11..]),"count":count}));
                }
                json!(points)
            } else {
                usage_summary(app, &buckets)?
            }
        }
        _ => return Ok(None),
    };
    Ok(Some((200, value)))
}

fn usage_summary(app: &App, buckets: &[String]) -> Result<Value> {
    let mut result = json!({"totalInputTokens":0,"totalOutputTokens":0,"totalCostUsd":0.0,"toonSavedTokens":0,
        "costIncomplete":false,"byProvider":{},"byModel":{}});
    for bucket in buckets {
        let Some(row) = app.db.col_get("usageRollups", bucket)? else {
            continue;
        };
        let row = row.doc;
        for (source, target) in [
            ("inputTokens", "totalInputTokens"),
            ("outputTokens", "totalOutputTokens"),
            ("toonSavedTokens", "toonSavedTokens"),
        ] {
            increment(&mut result, target, row[source].as_u64().unwrap_or(0));
        }
        result["totalCostUsd"] = json!(
            result["totalCostUsd"].as_f64().unwrap_or(0.0) + row["costUsd"].as_f64().unwrap_or(0.0)
        );
        result["costIncomplete"] =
            json!(result["costIncomplete"] == true || row["costIncomplete"] == true);
        for dimension in ["byProvider", "byModel"] {
            if let Some(groups) = row[dimension].as_object() {
                for (name, usage) in groups {
                    let entry = result[dimension]
                        .as_object_mut()
                        .unwrap()
                        .entry(name)
                        .or_insert_with(|| json!({"inputTokens":0,"outputTokens":0,"costUsd":0.0}));
                    add_usage(
                        entry,
                        usage["inputTokens"].as_u64().unwrap_or(0),
                        usage["outputTokens"].as_u64().unwrap_or(0),
                        usage["costUsd"].as_f64().unwrap_or(0.0),
                    );
                    entry["tokens"] = json!(
                        entry["inputTokens"]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_add(entry["outputTokens"].as_u64().unwrap_or(0))
                    );
                }
            }
        }
    }
    result["totalTokens"] = json!(
        result["totalInputTokens"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(result["totalOutputTokens"].as_u64().unwrap_or(0))
    );
    if let Some(models) = result["byModel"].as_object() {
        let mut missing_price = false;
        for model in models.keys() {
            let row = app.db.col_get("models", model)?;
            missing_price |= row.as_ref().is_none_or(|row| {
                row.doc["input_per_1m"].as_f64().is_none()
                    && row.doc["output_per_1m"].as_f64().is_none()
            });
        }
        if missing_price {
            result["costIncomplete"] = json!(true);
        }
    }
    Ok(result)
}
fn increment(value: &mut Value, key: &str, amount: u64) {
    value[key] = json!(value[key].as_u64().unwrap_or(0).saturating_add(amount));
}
fn add_usage(value: &mut Value, input: u64, output: u64, cost: f64) {
    increment(value, "inputTokens", input);
    increment(value, "outputTokens", output);
    value["costUsd"] = json!(value["costUsd"].as_f64().unwrap_or(0.0) + cost);
}

/// Added to the same batch as the mirrored message: no double counting on replay.
pub fn message_activity(app: &App, now: u64) -> Result<ColOp> {
    let id = hour_bucket(now)?;
    let previous = app.db.col_get("activityRollups", &id)?;
    let mut doc = previous
        .as_ref()
        .map_or_else(|| json!({}), |entry| entry.doc.clone());
    increment(&mut doc, "messageCount", 1);
    Ok(ColOp::Put {
        collection: "activityRollups".into(),
        id,
        doc,
        expected_version: Some(previous.map_or(0, |entry| entry.version)),
    })
}

// Earlier Rust builds mirrored messages but did not update Hive's activity rollups.
// Only migrate their explicit links, leaving legacy TypeScript rollups untouched.
fn backfill_activity(app: &App) -> Result<()> {
    let mut ready = app
        .dashboard
        .activity_ready
        .lock()
        .map_err(|_| "Actividad bloqueada")?;
    if *ready {
        return Ok(());
    }
    let _guard = app
        .activity_lock
        .write()
        .map_err(|_| "Actividad bloqueada")?;
    for mut link in app
        .db
        .col_scan("hive_sdk_rust_message_links_v1", &ScanOptions::default())?
    {
        if link.doc["activity_recorded"] == true {
            continue;
        }
        let Some(message_id) = link.doc["message_id"].as_str() else {
            continue;
        };
        let Some(message) = app.db.col_get("conversations", message_id)? else {
            continue;
        };
        let Some(at) = message.doc["created_at"].as_u64() else {
            continue;
        };
        link.doc["activity_recorded"] = json!(true);
        app.db.col_batch(&[
            message_activity(app, at)?,
            ColOp::Put {
                collection: "hive_sdk_rust_message_links_v1".into(),
                id: link.id,
                doc: link.doc,
                expected_version: Some(link.version),
            },
        ])?;
    }
    *ready = true;
    Ok(())
}

pub fn record_usage(
    app: &App,
    execution: &str,
    provider: &str,
    model: &str,
    usage: Option<&Usage>,
    now: u64,
) -> Result<()> {
    let _guard = app
        .activity_lock
        .write()
        .map_err(|_| "Actividad bloqueada")?;
    let id = format!("rust:{execution}");
    if app.db.col_get("usageRecords", &id)?.is_some() {
        return Ok(());
    }
    let pricing = app.db.col_get("models", model)?;
    let input_price = pricing
        .as_ref()
        .and_then(|row| row.doc["input_per_1m"].as_f64());
    let output_price = pricing
        .as_ref()
        .and_then(|row| row.doc["output_per_1m"].as_f64());
    let input = usage.map_or(0, |usage| usage.input_tokens);
    let output = usage.map_or(0, |usage| usage.output_tokens);
    let known = usage.is_some() && (input_price.is_some() || output_price.is_some());
    let cost = (input as f64 * input_price.unwrap_or(0.0)
        + output as f64 * output_price.unwrap_or(0.0))
        / 1_000_000.0;
    let bucket = hour_bucket(now)?;
    let previous = app.db.col_get("usageRollups", &bucket)?;
    let mut doc = previous.as_ref().map_or_else(
        || json!({"byProvider":{},"byModel":{}}),
        |row| row.doc.clone(),
    );
    add_usage(&mut doc, input, output, cost);
    doc["costIncomplete"] = json!(doc["costIncomplete"] == true || !known);
    for (dimension, name) in [("byProvider", provider), ("byModel", model)] {
        if doc.get(dimension).is_none() {
            doc[dimension] = json!({});
        }
        let entry = doc[dimension]
            .as_object_mut()
            .ok_or("Rollup de uso inválido")?
            .entry(name)
            .or_insert_with(|| json!({}));
        add_usage(entry, input, output, cost);
    }
    app.db.col_batch(&[
        ColOp::Put {collection:"usageRecords".into(),id:id.clone(),expected_version:Some(0),doc:json!({"id":id,"provider":provider,"model":model,
            "input_tokens":input,"output_tokens":output,"cost_usd":cost,"created_at":now,"usage_known":usage.is_some(),"cost_known":known,
            "latency_ms":null,"toon_saved_tokens":0,"toon_saved_cost":0.0,"toon_json_bytes":0,"toon_toon_bytes":0,"toon_saved_bytes":0,
            "toon_saved_percent":0,"toon_json_tokens":0,"toon_toon_tokens":0,"toon_saved_tokens_pct":0})},
        ColOp::Put {collection:"usageRollups".into(),id:bucket,doc,expected_version:Some(previous.map_or(0,|row|row.version))},
    ])?;
    Ok(())
}
