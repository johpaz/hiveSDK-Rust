//! Schedule advancement and job publication are one queue transaction.
use super::*;
use crate::scheduler::Schedule;
const SCHEDULES: &str = "hive_sdk_rust_schedules_v1";
fn grace() -> u64 {
    60_000
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MisfirePolicy {
    #[default]
    Skip,
    FireOnce,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSpec {
    pub schedule: Schedule,
    pub job: JobInput,
    #[serde(default)]
    pub misfire_policy: MisfirePolicy,
    #[serde(default = "grace")]
    pub misfire_grace_ms: u64,
    #[serde(default)]
    pub start_at: Option<u64>,
    #[serde(default)]
    pub stop_at: Option<u64>,
    #[serde(default)]
    pub max_runs: Option<u32>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    version: u32,
    tenant: String,
    queue: String,
    id: String,
    spec: ScheduleSpec,
    enabled: bool,
    created_at: u64,
    next_due: Option<u64>,
    last_tick: Option<u64>,
    published: u32,
    active_job: Option<String>,
}
impl DurableQueue {
    fn read_schedule(&self, id: &str) -> Result<Option<(u64, Entry)>> {
        let Some(row) = self
            .store
            .db
            .col_get(SCHEDULES, &self.job_key(id)?)
            .map_err(storage)?
        else {
            return Ok(None);
        };
        let entry: Entry = serde_json::from_value(row.doc)?;
        if entry.version != 1
            || entry.tenant != self.store.tenant_id
            || entry.queue != self.id
            || entry.id != id
        {
            return Err(state("invalid schedule scope or version"));
        }
        Ok(Some((row.version, entry)))
    }
    fn schedule_write(&self, entry: &Entry, revision: u64, writes: &mut Vec<ColOp>) -> Result<()> {
        writes.push(ColOp::Put {
            collection: SCHEDULES.into(),
            id: self.job_key(&entry.id)?,
            doc: serde_json::to_value(entry)?,
            expected_version: Some(revision),
        });
        Ok(())
    }
    fn schedule_report(&self, entry: &Entry) -> Result<Value> {
        let active = entry
            .active_job
            .as_ref()
            .map(|id| self.get(id))
            .transpose()?
            .flatten();
        if entry.active_job.is_some() && active.is_none() {
            return Err(state("missing scheduled job"));
        }
        let blocked = active.as_ref().is_some_and(|job| !job.status.terminal());
        let status = if blocked {
            "blocked"
        } else if entry.next_due.is_none() {
            "completed"
        } else if entry.enabled {
            "active"
        } else {
            "paused"
        };
        Ok(
            json!({"id":entry.id,"spec":entry.spec,"enabled":entry.enabled,"created_at":entry.created_at,
            "next_due":entry.next_due,"last_tick":entry.last_tick,"published":entry.published,"active_job":active,"status":status}),
        )
    }
    pub fn create_schedule(&self, id: &str, spec: ScheduleSpec) -> Result<Value> {
        valid_id(id)?;
        spec.schedule.validate()?;
        if spec.misfire_grace_ms > 86_400_000
            || spec.start_at.is_some_and(|t| t > PORTABLE)
            || spec.stop_at.is_some_and(|t| t > PORTABLE)
            || spec.start_at.zip(spec.stop_at).is_some_and(|(a, b)| a >= b)
            || spec.max_runs.is_some_and(|v| v == 0 || v > 10_000)
            || spec.job.not_before.is_some()
            || spec.job.idempotency_key.is_some()
            || spec.job.payload.get("schedule").is_some()
        {
            return Err(invalid("invalid schedule bounds or reserved job fields"));
        }
        self.transact(|meta, now| {
            if let Some((_, old)) = self.read_schedule(id)? {
                if old.spec != spec {
                    return Err(state("SCHEDULE_IDEMPOTENCY_CONFLICT"));
                }
                return Change::value(self.schedule_report(&old)?);
            }
            self.check_input(&spec.job, &meta.policy)?;
            if let Some(root) = &spec.job.objective_id {
                self.store
                    .prepare_objective(
                        root,
                        &json!({"op":"check","depth":spec.job.depth}).to_string(),
                    )?
                    .result?;
            }
            let next = match &spec.schedule {
                Schedule::At { unix_ms } => {
                    Some(*unix_ms).filter(|v| spec.start_at.is_none_or(|start| *v >= start))
                }
                Schedule::Interval {
                    every_ms,
                    anchor_unix_ms,
                } => {
                    let first = match anchor_unix_ms {
                        Some(at) => Some(*at),
                        None => now.checked_add(*every_ms).filter(|v| *v <= PORTABLE),
                    };
                    first
                        .map(|at| {
                            Schedule::Interval {
                                every_ms: *every_ms,
                                anchor_unix_ms: Some(at),
                            }
                            .next_at_or_after(spec.start_at.unwrap_or(at))
                        })
                        .transpose()?
                        .flatten()
                }
                Schedule::Cron { .. } => match spec.start_at {
                    Some(start) => spec.schedule.next_at_or_after(start)?,
                    None => spec.schedule.next_after(now)?,
                },
            }
            .filter(|v| spec.stop_at.is_none_or(|stop| *v < stop));
            let entry = Entry {
                version: 1,
                tenant: self.store.tenant_id.clone(),
                queue: self.id.clone(),
                id: id.into(),
                spec: spec.clone(),
                enabled: false,
                created_at: now,
                next_due: next,
                last_tick: None,
                published: 0,
                active_job: None,
            };
            let mut writes = vec![];
            self.schedule_write(&entry, 0, &mut writes)?;
            Ok(Change {
                writes,
                result: Ok(self.schedule_report(&entry)?),
            })
        })
    }
    pub fn get_schedule(&self, id: &str) -> Result<Option<Value>> {
        let value = self.transact(|_, _| {
            Change::value(
                self.read_schedule(id)?
                    .map(|(_, entry)| self.schedule_report(&entry))
                    .transpose()?,
            )
        })?;
        Ok((!value.is_null()).then_some(value))
    }
    pub fn enable_schedule(&self, id: &str, enabled: bool) -> Result<Value> {
        self.transact(|_, _| {
            let (revision, mut entry) = self
                .read_schedule(id)?
                .ok_or_else(|| state("SCHEDULE_NOT_FOUND"))?;
            if entry.enabled == enabled {
                return Change::value(self.schedule_report(&entry)?);
            }
            entry.enabled = enabled;
            let mut writes = vec![];
            self.schedule_write(&entry, revision, &mut writes)?;
            Ok(Change {
                writes,
                result: Ok(self.schedule_report(&entry)?),
            })
        })
    }
    pub fn tick_schedule(&self, id: &str, observed: Option<u64>) -> Result<Value> {
        if observed.is_some_and(|v| v > PORTABLE) {
            return Err(invalid("schedule observation exceeds portable range"));
        }
        self.transact(|meta,clock| {
            let now=observed.unwrap_or(clock);
            let (revision,mut entry)=self.read_schedule(id)?.ok_or_else(||state("SCHEDULE_NOT_FOUND"))?;
            if entry.last_tick.is_some_and(|last|now<last) {return Err(invalid("schedule clock moved backwards"));}
            let report=self.schedule_report(&entry)?;
            if !entry.enabled || entry.next_due.is_none_or(|due|due>now) {return Change::value(json!({"outcome":"idle","job":null}));}
            if report["status"]=="blocked" {return Change::value(json!({"outcome":"blocked","job":null}));}
            let due=entry.next_due.expect("checked due");
            let expired=entry.spec.stop_at.is_some_and(|stop|now>=stop);
            let limit=entry.spec.max_runs.is_some_and(|limit|entry.published>=limit);
            let skipped=entry.spec.misfire_policy==MisfirePolicy::Skip && now-due>entry.spec.misfire_grace_ms;
            let mut writes=vec![];let mut published=None;
            if !expired && !limit && !skipped {
                let mut input=entry.spec.job.clone();
                input.not_before=Some(due);
                input.payload["schedule"]=json!({"id":id,"scheduled_for":due,"observed_at":now});
                let encoded=serde_json::to_vec(&json!([self.store.tenant_id,self.id,id,due]))?;
                let hash=ring::digest::digest(&ring::digest::SHA256,&encoded).as_ref().iter().map(|n|format!("{n:02x}")).collect::<String>();
                input.idempotency_key=Some(format!("schedule:{hash}"));
                let mut change=self.prepare_enqueue(meta,&input,clock)?;
                let job:Job=serde_json::from_value(change.result?)?;
                entry.published+=1;entry.active_job=Some(job.id.clone());published=Some(job);
                writes.append(&mut change.writes);
            } else {entry.active_job=None;}
            entry.last_tick=Some(now);
            entry.next_due=if expired || limit || entry.spec.max_runs.is_some_and(|limit|entry.published>=limit) {None} else {
                match &entry.spec.schedule {
                    Schedule::Interval {every_ms,..}=>((now-due)/every_ms).checked_add(1).and_then(|steps|steps.checked_mul(*every_ms)).and_then(|delta|due.checked_add(delta)).filter(|v|*v<=PORTABLE),
                    _=>entry.spec.schedule.next_after(now)?,
                }.filter(|v|entry.spec.stop_at.is_none_or(|stop|*v<stop))
            };
            self.schedule_write(&entry,revision,&mut writes)?;
            // Return the committed job receipt; schedule inspection is a separate read.
            Ok(Change {writes,result:Ok(json!({"outcome":if published.is_some(){"published"}else{"skipped"},"job":published}))})
        })
    }
    /// Explicit producer loop; workers/executors are driven separately.
    pub async fn run_scheduler(
        &self,
        poll_ms: u64,
        cancel: crate::provider::CancellationToken,
    ) -> Result<()> {
        if !(10..=60_000).contains(&poll_ms) {
            return Err(invalid("scheduler poll interval must be 10..60000 ms"));
        }
        loop {
            let mut cursor = None;
            loop {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                let queue = self.clone();
                let page = tokio::task::spawn_blocking(move || {
                    queue.list_schedules(50, cursor.as_deref())
                })
                .await
                .map_err(|_| state("SCHEDULER_WORKER_PANICKED"))??;
                for entry in page["items"]
                    .as_array()
                    .ok_or_else(|| state("invalid schedule page"))?
                {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    let id = entry["id"]
                        .as_str()
                        .ok_or_else(|| state("missing schedule id"))?
                        .to_owned();
                    let queue = self.clone();
                    tokio::task::spawn_blocking(move || queue.tick_schedule(&id, None))
                        .await
                        .map_err(|_| state("SCHEDULER_WORKER_PANICKED"))??;
                }
                cursor = page["next_cursor"].as_str().map(str::to_owned);
                if cursor.is_none() {
                    break;
                }
            }
            tokio::select! { _ = cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {} }
        }
    }
    pub fn list_schedules(&self, limit: usize, cursor: Option<&str>) -> Result<Value> {
        if !(1..=200).contains(&limit) {
            return Err(invalid("schedule page limit must be 1..200"));
        }
        let prefix = self.prefix();
        if cursor.is_some_and(|c| !c.starts_with(&prefix)) {
            return Err(invalid("schedule cursor belongs to another scope"));
        }
        let rows = self
            .store
            .db
            .col_scan(
                SCHEDULES,
                &ScanOptions {
                    prefix: Some(prefix),
                    start: cursor.map(str::to_owned),
                    limit: limit + 2,
                    ..Default::default()
                },
            )
            .map_err(storage)?;
        let mut items = vec![];
        let mut last = None;
        for row in rows {
            if Some(row.id.as_str()) == cursor {
                continue;
            }
            if items.len() == limit {
                return Ok(json!({"items":items,"next_cursor":last}));
            }
            let entry: Entry = serde_json::from_value(row.doc)?;
            // Reuse scoped/versioned reads rather than trusting a decoded row alone.
            let (_, checked) = self
                .read_schedule(&entry.id)?
                .ok_or_else(|| state("missing schedule"))?;
            items.push(self.schedule_report(&checked)?);
            last = Some(row.id);
        }
        Ok(json!({"items":items,"next_cursor":null}))
    }
}
