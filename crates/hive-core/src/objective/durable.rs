//! Root accounting documents and CAS writes shared with session transactions.
use super::{Ledger, Objective, ObjectivePolicy};
use crate::{
    Error, Result,
    sessions::{SessionStore, storage},
};
use hivedb_core::ColOp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Mutex;

const COLLECTION: &str = "hive_sdk_rust_objectives_v1";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    tenant_id: String,
    id: String,
    policy: ObjectivePolicy,
    deadline: Option<u64>,
    ledger: Ledger,
}

pub(crate) struct Prepared {
    pub write: Option<ColOp>,
    pub result: Result<Value>,
}

fn key(store: &SessionStore, id: &str) -> Result<String> {
    if id.trim().is_empty() {
        return Err(Error::Input("objective id must not be empty".into()));
    }
    Ok(serde_json::to_string(&(&store.tenant_id, id))?)
}

impl SessionStore {
    /// Create once, or reopen with the original policy; never refreshes deadline or balances.
    pub fn open_objective(&self, id: &str, policy: ObjectivePolicy) -> Result<Value> {
        let _guard = self
            .objective_lock
            .lock()
            .map_err(|_| Error::Storage("poisoned objective transaction guard".into()))?;
        let key = key(self, id)?;
        if let Some(doc) = self.db.col_get(COLLECTION, &key).map_err(storage)? {
            let record = self.objective_record(id, doc.doc)?;
            if record.policy != policy {
                return Err(Error::SessionConfigMismatch);
            }
            return self.inspect_objective(id);
        }
        let objective = Objective::new(policy)?;
        let record = Record {
            version: 4,
            tenant_id: self.tenant_id.clone(),
            id: id.into(),
            policy: objective.policy,
            deadline: objective.deadline,
            ledger: Ledger::default(),
        };
        self.db
            .col_batch(&[ColOp::Put {
                collection: COLLECTION.into(),
                id: key,
                doc: serde_json::to_value(record)?,
                expected_version: Some(0),
            }])
            .map_err(storage)?;
        self.inspect_objective(id)
    }

    fn objective_record(&self, id: &str, value: Value) -> Result<Record> {
        let record: Record = serde_json::from_value(value)?;
        // Validate stored policy without recomputing the persisted clock.
        Objective::new(record.policy.clone())?;
        for (key, entry) in &record.ledger.verifications {
            entry.validate(key)?;
        }
        for (id, entry) in &record.ledger.triggers {
            entry.validate(id)?;
        }
        let mut summary_models = std::collections::BTreeSet::new();
        let mut summary_input = 0u64;
        let mut summary_output = 0u64;
        for (key, entry) in &record.ledger.summaries {
            entry.validate(key)?;
            for (id, usage) in &entry.models {
                if let Some(usage) = usage {
                    summary_input = summary_input.saturating_add(usage.input_tokens);
                    summary_output = summary_output.saturating_add(usage.output_tokens);
                }
                if *id == 0
                    || *id > record.ledger.model_calls
                    || !summary_models.insert(*id)
                    || (usage.is_some()
                        && (!record.ledger.settled.contains(id)
                            || record.ledger.reservations.contains_key(id)))
                    || (usage.is_none() && !record.ledger.reservations.contains_key(id))
                {
                    return Err(Error::Storage("invalid summary model accounting".into()));
                }
            }
        }
        let claimed = record.ledger.verifications.len() as u64
            + record
                .ledger
                .triggers
                .values()
                .map(|entry| entry.execution_count() as u64)
                .sum::<u64>()
            + record
                .ledger
                .summaries
                .values()
                .map(|entry| u64::from(entry.tool_calls))
                .sum::<u64>();
        if !matches!(record.version, 1..=4)
            || (record.version < 4 && !record.ledger.summaries.is_empty())
            || record.id != id
            || record.tenant_id != self.tenant_id
            || record.deadline.is_some() != record.policy.max_run_ms.is_some()
            || record.deadline.is_some_and(|n| n > 9_007_199_254_740_991)
            || record.ledger.model_calls > record.policy.max_model_calls
            || record.ledger.tool_calls > record.policy.max_tool_calls
            || claimed > u64::from(record.ledger.tool_calls)
            || record.ledger.triggers.len() > 64
            || record.ledger.summaries.len() > 64
            || summary_input > record.ledger.input_tokens
            || summary_output > record.ledger.output_tokens
            || record
                .ledger
                .reservations
                .keys()
                .chain(record.ledger.settled.iter())
                .any(|id| *id == 0 || *id > record.ledger.model_calls)
            || record
                .ledger
                .reservations
                .values()
                .any(|cap| *cap == 0 || *cap > record.policy.max_output_tokens)
            || record
                .ledger
                .reservations
                .values()
                .try_fold(0_u64, |sum, cap| sum.checked_add(*cap))
                .is_none_or(|sum| sum > record.policy.max_total_output_tokens)
        {
            return Err(Error::Storage("invalid objective record".into()));
        }
        Ok(record)
    }

    pub fn inspect_objective(&self, id: &str) -> Result<Value> {
        let doc = self
            .db
            .col_get(COLLECTION, &key(self, id)?)
            .map_err(storage)?
            .ok_or_else(|| Error::Input("objective does not exist".into()))?;
        let record = self.objective_record(id, doc.doc)?;
        let objective = Objective {
            policy: record.policy,
            deadline: record.deadline,
            ledger: Mutex::new(record.ledger),
        };
        let mut snapshot = objective.snapshot()?;
        snapshot["id"] = json!(id);
        snapshot["revision"] = json!(doc.version);
        Ok(snapshot)
    }

    pub(crate) fn prepare_objective(&self, id: &str, command: &str) -> Result<Prepared> {
        let key = key(self, id)?;
        let doc = self
            .db
            .col_get(COLLECTION, &key)
            .map_err(storage)?
            .ok_or_else(|| Error::Input("objective does not exist".into()))?;
        let original = doc.doc;
        let record = self.objective_record(id, original.clone())?;
        let objective = Objective {
            policy: record.policy,
            deadline: record.deadline,
            ledger: Mutex::new(record.ledger),
        };
        let result = objective
            .dispatch(command)
            .and_then(|value| Ok(serde_json::from_str(&value)?));
        let updated = serde_json::to_value(Record {
            version: 4,
            tenant_id: self.tenant_id.clone(),
            id: id.into(),
            policy: objective.policy.clone(),
            deadline: objective.deadline,
            ledger: objective.lock()?.clone(),
        })?;
        let write = (updated != original).then_some(ColOp::Put {
            collection: COLLECTION.into(),
            id: key,
            doc: updated,
            expected_version: Some(doc.version),
        });
        Ok(Prepared { write, result })
    }

    /// Standalone durable operations reserve before dispatch. Session operations use prepare + col_batch.
    pub fn objective_command(&self, id: &str, command: &str) -> Result<Value> {
        let _guard = self
            .objective_lock
            .lock()
            .map_err(|_| Error::Storage("poisoned objective transaction guard".into()))?;
        if serde_json::from_str::<Value>(command)?.get("op") == Some(&json!("inspect")) {
            // Still parse the full command so unknown fields cannot bypass validation.
            let prepared = self.prepare_objective(id, command)?;
            prepared.result?;
            return self.inspect_objective(id);
        }
        let prepared = self.prepare_objective(id, command)?;
        if let Some(write) = prepared.write {
            self.db.col_batch(&[write]).map_err(storage)?;
        }
        prepared.result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hivedb_core::PutOptions;

    #[test]
    fn failed_session_cas_rolls_back_the_root_write_in_the_same_batch() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
        store
            .open_objective("root", ObjectivePolicy::default())
            .unwrap();
        store
            .db
            .col_put(
                "transaction_probe",
                "session",
                &json!({"n":1}),
                PutOptions {
                    expected_version: Some(0),
                },
            )
            .unwrap();
        let prepared = store
            .prepare_objective("root", r#"{"op":"model","depth":0,"cap":10}"#)
            .unwrap();
        prepared.result.unwrap();
        let writes = [
            prepared.write.unwrap(),
            ColOp::Put {
                collection: "transaction_probe".into(),
                id: "session".into(),
                doc: json!({"n":2}),
                expected_version: Some(0),
            },
        ];
        assert!(store.db.col_batch(&writes).is_err());
        let snapshot = store.inspect_objective("root").unwrap();
        assert_eq!(snapshot["model_calls"], 0);
        assert_eq!(snapshot["reservations"], json!({}));
        assert_eq!(
            store
                .db
                .col_get("transaction_probe", "session")
                .unwrap()
                .unwrap()
                .doc["n"],
            1
        );
    }

    #[test]
    fn legacy_root_upgrades_without_refunding_an_outstanding_permit() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
        store
            .open_objective("root", ObjectivePolicy::default())
            .unwrap();
        store
            .objective_command("root", r#"{"op":"model","depth":0,"cap":10}"#)
            .unwrap();
        let key = key(&store, "root").unwrap();
        let doc = store.db.col_get(COLLECTION, &key).unwrap().unwrap();
        let mut legacy = doc.doc;
        legacy["version"] = json!(1);
        legacy["ledger"]
            .as_object_mut()
            .unwrap()
            .remove("verifications");
        legacy["ledger"].as_object_mut().unwrap().remove("triggers");
        legacy["ledger"]
            .as_object_mut()
            .unwrap()
            .remove("summaries");
        store
            .db
            .col_put(
                COLLECTION,
                &key,
                &legacy,
                PutOptions {
                    expected_version: Some(doc.version),
                },
            )
            .unwrap();
        assert_eq!(
            store.inspect_objective("root").unwrap()["reservations"]["1"],
            10
        );
        store
            .objective_command("root", r#"{"op":"check","depth":0}"#)
            .unwrap();
        let upgraded = store.db.col_get(COLLECTION, &key).unwrap().unwrap().doc;
        assert_eq!(upgraded["version"], 4);
        assert_eq!(upgraded["ledger"]["model_calls"], 1);
        assert_eq!(upgraded["ledger"]["reservations"]["1"], 10);
    }

    #[test]
    fn bound_sessions_use_a_format_that_older_unbound_readers_reject() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
        store
            .open_objective("root", ObjectivePolicy::default())
            .unwrap();
        let mut engine = crate::Engine::new("test".into(), None, vec![], 8).unwrap();
        engine.bind_objective("root".into()).unwrap();
        store
            .begin_run("a", "s", 0, engine.clone(), "Work".into())
            .unwrap();
        let id = serde_json::to_string(&("tenant", "a", "s")).unwrap();
        let record = store
            .db
            .col_get("hive_sdk_rust_sessions_v1", &id)
            .unwrap()
            .unwrap()
            .doc;
        assert_eq!(record["schema_version"], 3);
        assert_eq!(record["state"]["objective_id"], "root");
        assert!(store.load_run("a", "s", &engine).is_ok());
    }
}
