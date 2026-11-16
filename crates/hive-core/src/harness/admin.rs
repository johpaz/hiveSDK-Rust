//! Queue administration: filtered listing, explicit retry of failed jobs, bulk
//! cancellation, and the durable event log. Nothing here runs or replays work on its
//! own; each operation is something an operator or host asks for.
use super::*;

const SCHEDULE_ROWS: &str = "hive_sdk_rust_schedules_v1";
const MAX_EXAMINED: usize = 5_000;
const MAX_BULK: usize = 1_000;

/// Which jobs a listing or bulk operation applies to. Empty fields match everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobFilter {
    pub status: Vec<JobStatus>,
    pub kind: Option<String>,
    pub kind_prefix: Option<String>,
    pub lane: Option<String>,
    pub objective_id: Option<String>,
}

impl JobFilter {
    fn matches(&self, job: &Job) -> bool {
        (self.status.is_empty() || self.status.contains(&job.status))
            && self
                .kind
                .as_ref()
                .is_none_or(|kind| *kind == job.input.kind)
            && self
                .kind_prefix
                .as_ref()
                .is_none_or(|prefix| job.input.kind.starts_with(prefix))
            && self
                .lane
                .as_ref()
                .is_none_or(|lane| *lane == job.input.lane)
            && self
                .objective_id
                .as_ref()
                .is_none_or(|id| job.input.objective_id.as_ref() == Some(id))
    }
}

impl DurableQueue {
    /// Append an event to the batch of the change it describes. The sequence is
    /// monotonic per queue and committed with the same CAS as the change.
    pub(super) fn emit(
        &self,
        meta: &mut Meta,
        writes: &mut Vec<ColOp>,
        kind: &str,
        subject: Value,
    ) -> Result<()> {
        meta.events = meta
            .events
            .checked_add(1)
            .filter(|n| *n <= PORTABLE)
            .ok_or_else(|| state("EVENT_SEQUENCE_EXHAUSTED"))?;
        let at = (self.clock)()?;
        writes.push(ColOp::Put {
            collection: EVENTS.into(),
            id: self.event_key(meta.events),
            doc: json!({"version":1,"tenant":self.store.tenant_id,"queue":self.id,"seq":meta.events,"at":at,"type":kind,"subject":subject}),
            expected_version: Some(0),
        });
        Ok(())
    }
    fn event_key(&self, seq: u64) -> String {
        json!([self.store.tenant_id, self.id, format!("{seq:020}")]).to_string()
    }

    /// Events with a sequence greater than `after`, oldest first. A consumer stores
    /// `next_after` and resumes from it; a gap can only come from `prune_events`.
    pub fn events(&self, after: u64, limit: usize) -> Result<Value> {
        if !(1..=1000).contains(&limit) {
            return Err(invalid("event limit must be 1..1000"));
        }
        let rows = self
            .store
            .db
            .col_scan(
                EVENTS,
                &ScanOptions {
                    prefix: Some(self.prefix()),
                    start: Some(self.event_key(after.saturating_add(1))),
                    limit,
                    ..Default::default()
                },
            )
            .map_err(storage)?;
        let mut items = vec![];
        for row in rows {
            if row.doc["tenant"] != self.store.tenant_id.as_str()
                || row.doc["queue"] != self.id.as_str()
            {
                return Err(state("EVENT_SCOPE_MISMATCH"));
            }
            items.push(row.doc);
        }
        let next_after = items
            .last()
            .and_then(|event| event["seq"].as_u64())
            .unwrap_or(after);
        let latest = self
            .read_meta()?
            .ok_or_else(|| state("QUEUE_NOT_OPEN"))?
            .1
            .events;
        Ok(json!({"items":items,"next_after":next_after,"latest":latest}))
    }

    /// Delete events up to and including `up_to`. Explicit: the log is never purged on its own.
    pub fn prune_events(&self, up_to: u64) -> Result<Value> {
        let mut removed = 0u64;
        loop {
            let rows = self
                .store
                .db
                .col_scan(
                    EVENTS,
                    &ScanOptions {
                        prefix: Some(self.prefix()),
                        limit: 200,
                        ..Default::default()
                    },
                )
                .map_err(storage)?;
            let doomed: Vec<_> = rows
                .into_iter()
                .filter(|row| row.doc["seq"].as_u64().is_some_and(|seq| seq <= up_to))
                .collect();
            if doomed.is_empty() {
                return Ok(json!({"removed":removed}));
            }
            let writes: Vec<_> = doomed
                .iter()
                .map(|row| ColOp::Delete {
                    collection: EVENTS.into(),
                    id: row.id.clone(),
                })
                .collect();
            self.store.db.col_batch(&writes).map_err(storage)?;
            removed += doomed.len() as u64;
        }
    }

    /// Jobs matching `filter`, oldest first. At most 5000 jobs are examined per call;
    /// `next_cursor` resumes from the last one examined.
    pub fn list_where(
        &self,
        filter: &JobFilter,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Value> {
        if !(1..=200).contains(&limit) {
            return Err(invalid("job list limit must be 1..200"));
        }
        let mut items: Vec<Job> = vec![];
        let mut cursor = cursor.map(str::to_owned);
        let mut examined = 0;
        loop {
            let page = self.list(200, cursor.as_deref())?;
            let jobs: Vec<Job> = serde_json::from_value(page["items"].clone())?;
            let more = page["next_cursor"].as_str().map(str::to_owned);
            for job in jobs {
                examined += 1;
                let key = self.job_key(&job.id)?;
                if filter.matches(&job) {
                    items.push(job);
                }
                if items.len() == limit || examined >= MAX_EXAMINED {
                    // Anything after this job (in this page or later pages) is still unexamined.
                    return Ok(json!({"items":items,"next_cursor":Some(key),"examined":examined}));
                }
            }
            match more {
                Some(next) => cursor = Some(next),
                None => return Ok(json!({"items":items,"next_cursor":null,"examined":examined})),
            }
        }
    }

    /// Put a `failed` job back in the queue. It must still have attempts left, unless
    /// `force` is set. Either way the root objective's limits are checked again and
    /// nothing is refunded: the next run is charged like any other.
    pub fn retry_failed(&self, id: &str, force: bool) -> Result<Job> {
        let value = self.transact(|meta, now| {
            let mut job = self.get(id)?.ok_or_else(|| state("JOB_NOT_FOUND"))?;
            if job.status != JobStatus::Failed {
                return Err(state("JOB_NOT_RETRYABLE"));
            }
            if !force && job.attempts >= job.input.max_attempts {
                return Err(state("JOB_RETRY_NOT_ALLOWED"));
            }
            if let Some(objective) = &job.input.objective_id {
                self.store
                    .prepare_objective(
                        objective,
                        &json!({"op":"check","depth":job.input.depth}).to_string(),
                    )?
                    .result?;
            }
            job.status = JobStatus::Pending;
            job.error = None;
            job.result = None;
            job.finished_at = None;
            job.worker_id = None;
            job.execution_id = None;
            job.lease_expires_at = None;
            job.cancellation_requested = false;
            job.not_before = now;
            let mut writes = vec![];
            self.save(meta, &mut job, &mut writes)?;
            self.emit(
                meta,
                &mut writes,
                "job.retried",
                json!({"job_id":job.id,"forced":force,"attempts":job.attempts}),
            )?;
            Ok(Change {
                writes,
                result: Ok(serde_json::to_value(job)?),
            })
        })?;
        Ok(serde_json::from_value(value)?)
    }

    /// Cancel every non-terminal job matching `filter` (at most 1000 per call). Each is
    /// an ordinary `cancel`: pending jobs end, running ones are asked to stop.
    pub fn cancel_where(&self, filter: &JobFilter) -> Result<Value> {
        let mut narrowed = filter.clone();
        if narrowed.status.is_empty() {
            narrowed.status = vec![JobStatus::Pending, JobStatus::Running];
        }
        narrowed.status.retain(|status| !status.terminal());
        // An empty status list means "any", so a filter that named only finished
        // states must stop here instead of widening into everything.
        if narrowed.status.is_empty() {
            return Ok(json!({"cancelled":[],"truncated":false}));
        }
        let found = self.list_where(&narrowed, 200, None)?;
        let mut cancelled = vec![];
        let mut cursor = found["next_cursor"].clone();
        let mut batch: Vec<Job> = serde_json::from_value(found["items"].clone())?;
        loop {
            for job in batch {
                if cancelled.len() >= MAX_BULK {
                    return Ok(json!({"cancelled":cancelled,"truncated":true}));
                }
                let job = self.cancel(&job.id)?;
                cancelled.push(json!({"id":job.id,"status":job.status}));
            }
            let Some(next) = cursor.as_str().map(str::to_owned) else {
                break;
            };
            let page = self.list_where(&narrowed, 200, Some(&next))?;
            batch = serde_json::from_value(page["items"].clone())?;
            cursor = page["next_cursor"].clone();
            if batch.is_empty() && cursor.is_null() {
                break;
            }
        }
        Ok(json!({"cancelled":cancelled,"truncated":false}))
    }

    /// Permanently delete finished jobs of one explicit kind that ended before
    /// `finished_before`, so a long-lived queue (a recurring schedule, say) never hits
    /// `max_jobs`. The caller must name a `kind` or `kind_prefix`; kinds starting with
    /// `hive.` (delegation groups and goals, which refer to their jobs by id) are
    /// refused, and the job a schedule is currently waiting on is always kept.
    pub fn purge_jobs(
        &self,
        filter: &JobFilter,
        finished_before: u64,
        limit: usize,
    ) -> Result<Value> {
        let scoped = filter
            .kind
            .as_deref()
            .or(filter.kind_prefix.as_deref())
            .filter(|kind| !kind.is_empty());
        let Some(scope) = scoped else {
            return Err(invalid("purging requires an explicit kind or kind_prefix"));
        };
        if scope.starts_with("hive.") || "hive.".starts_with(scope) {
            return Err(invalid(
                "jobs of delegation groups and goals cannot be purged",
            ));
        }
        if !(1..=200).contains(&limit) {
            return Err(invalid("purge limit must be 1..200"));
        }
        let mut narrowed = filter.clone();
        if narrowed.status.is_empty() {
            narrowed.status = vec![
                JobStatus::Completed,
                JobStatus::Failed,
                JobStatus::Cancelled,
            ];
        }
        narrowed.status.retain(|status| status.terminal());
        if narrowed.status.is_empty() {
            return Ok(json!({"purged":0}));
        }
        let mut protected = BTreeSet::new();
        for row in self
            .store
            .db
            .col_scan(
                SCHEDULE_ROWS,
                &ScanOptions {
                    prefix: Some(self.prefix()),
                    ..Default::default()
                },
            )
            .map_err(storage)?
        {
            if let Some(id) = row.doc["active_job"].as_str() {
                protected.insert(id.to_owned());
            }
        }
        let mut candidates: Vec<String> = vec![];
        let mut cursor = None::<String>;
        'scan: loop {
            let page = self.list_where(&narrowed, 200, cursor.as_deref())?;
            for job in serde_json::from_value::<Vec<Job>>(page["items"].clone())? {
                if job.finished_at.is_some_and(|at| at < finished_before)
                    && !protected.contains(&job.id)
                {
                    candidates.push(job.id);
                    if candidates.len() == limit {
                        break 'scan;
                    }
                }
            }
            match page["next_cursor"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
        }
        let value = self.transact(|meta, _| {
            let mut writes = vec![];
            let mut removed = 0u64;
            for id in &candidates {
                // Re-read inside the transaction: an operator may have retried it meanwhile.
                let Some(job) = self.get(id)? else { continue };
                if !job.status.terminal() || !narrowed.matches(&job) {
                    continue;
                }
                writes.push(ColOp::Delete {
                    collection: JOBS.into(),
                    id: self.job_key(id)?,
                });
                if let Some(key) = &job.input.idempotency_key {
                    writes.push(ColOp::Delete {
                        collection: KEYS.into(),
                        id: json!([self.store.tenant_id, self.id, key]).to_string(),
                    });
                }
                removed += 1;
            }
            if removed == 0 {
                return Change::value(json!({"purged":0}));
            }
            meta.purged += removed;
            self.emit(
                meta,
                &mut writes,
                "jobs.purged",
                json!({"count":removed,"scope":scope,"finished_before":finished_before}),
            )?;
            Ok(Change {
                writes,
                result: Ok(json!({"purged":removed})),
            })
        })?;
        Ok(value)
    }
}
