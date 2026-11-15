use hive_agent_core::{
    Error, Usage,
    objective::{Objective, ObjectivePolicy, TriggerSchedule},
    sessions::SessionStore,
};
use serde_json::json;

#[test]
fn all_sources_are_disabled_until_explicitly_enabled() {
    let objective = Objective::new(Default::default()).unwrap();
    for (id, schedule) in [
        ("event", TriggerSchedule::Event),
        ("date", TriggerSchedule::At { unix_ms: 10 }),
        (
            "interval",
            TriggerSchedule::Interval {
                every_ms: 100,
                anchor_unix_ms: Some(10),
            },
        ),
    ] {
        let registered = objective.register_trigger(id.into(), schedule).unwrap();
        assert_eq!(registered["enabled"], false);
        assert!(objective.poll_trigger_at(id, 1000).unwrap().is_none());
    }
    assert!(
        objective
            .emit_trigger("event", "e1".into(), json!({"n":1}))
            .unwrap()
            .is_none()
    );
    let snapshot = objective.snapshot().unwrap();
    assert_eq!(snapshot["tool_calls"], 0);
    assert_eq!(snapshot["model_calls"], 0);
}

#[test]
fn intervals_coalesce_missed_occurrences_without_a_catchup_burst() {
    let objective = Objective::new(Default::default()).unwrap();
    let schedule = TriggerSchedule::Interval {
        every_ms: 100,
        anchor_unix_ms: Some(1000),
    };
    objective
        .register_trigger("poll".into(), schedule.clone())
        .unwrap();
    objective.enable_trigger("poll", true).unwrap();
    let claim = objective.poll_trigger_at("poll", 2055).unwrap().unwrap();
    assert_eq!(claim.occurrence_key, "2000");
    assert_eq!(claim.input["scheduled_for_unix_ms"], 2000);
    assert_eq!(claim.input["skipped_occurrences"], 10);
    assert_eq!(
        objective.inspect_trigger("poll").unwrap()["next_due_unix_ms"],
        2100
    );
    objective
        .finish_trigger(
            "poll",
            &claim.occurrence_key,
            &claim.execution_id,
            json!("done"),
        )
        .unwrap();
    assert!(objective.poll_trigger_at("poll", 2055).unwrap().is_none());
    // Re-registration and a backwards observation do not reset the calendar.
    assert_eq!(
        objective.register_trigger("poll".into(), schedule).unwrap()["next_due_unix_ms"],
        2100
    );
    assert!(objective.poll_trigger_at("poll", 1000).unwrap().is_none());
    assert_eq!(objective.snapshot().unwrap()["tool_calls"], 1);
}

#[test]
fn event_keys_are_idempotent_and_cannot_be_rebound_to_different_inputs() {
    let objective = Objective::new(Default::default()).unwrap();
    objective
        .register_trigger("event".into(), TriggerSchedule::Event)
        .unwrap();
    objective.enable_trigger("event", true).unwrap();
    let claim = objective
        .emit_trigger("event", "e1".into(), json!({"n":1}))
        .unwrap()
        .unwrap();
    assert!(matches!(
        objective.emit_trigger("event", "e1".into(), json!({"n":1})),
        Err(Error::TriggerUncertain)
    ));
    assert!(matches!(
        objective.emit_trigger("event", "e2".into(), json!({"n":2})),
        Err(Error::TriggerUncertain)
    ));
    let receipt = objective
        .finish_trigger("event", "e1", &claim.execution_id, json!({"done":true}))
        .unwrap();
    let cached = objective
        .emit_trigger("event", "e1".into(), json!({"n":1}))
        .unwrap()
        .unwrap();
    assert_eq!(cached.result.unwrap().execution_id, receipt.execution_id);
    assert!(matches!(
        objective.emit_trigger("event", "e1".into(), json!({"n":2})),
        Err(Error::Input(_))
    ));
    assert_eq!(objective.snapshot().unwrap()["tool_calls"], 1);
}

#[test]
fn dates_fire_once_and_wrong_execution_ids_never_confirm_them() {
    let objective = Objective::new(Default::default()).unwrap();
    objective
        .register_trigger("date".into(), TriggerSchedule::At { unix_ms: 1000 })
        .unwrap();
    objective.enable_trigger("date", true).unwrap();
    assert!(objective.poll_trigger_at("date", 999).unwrap().is_none());
    let claim = objective.poll_trigger_at("date", 1000).unwrap().unwrap();
    assert!(matches!(
        objective.finish_trigger("date", &claim.occurrence_key, "stale", json!(true)),
        Err(Error::SessionConflict)
    ));
    assert!(matches!(
        objective.poll_trigger_at("date", 2000),
        Err(Error::TriggerUncertain)
    ));
    objective
        .finish_trigger(
            "date",
            &claim.occurrence_key,
            &claim.execution_id,
            json!(null),
        )
        .unwrap();
    assert!(objective.poll_trigger_at("date", 2000).unwrap().is_none());
    assert!(matches!(
        objective.finish_trigger(
            "date",
            &claim.occurrence_key,
            &claim.execution_id,
            json!(true)
        ),
        Err(Error::SessionConflict)
    ));
}

#[test]
fn trigger_claims_share_attempts_and_never_reset_model_allowances() {
    let objective = Objective::new(ObjectivePolicy {
        max_tool_calls: 1,
        max_model_calls: 1,
        ..Default::default()
    })
    .unwrap();
    objective
        .register_trigger("event".into(), TriggerSchedule::Event)
        .unwrap();
    objective.enable_trigger("event", true).unwrap();
    let claim = objective
        .emit_trigger("event", "e1".into(), json!(null))
        .unwrap()
        .unwrap();
    let permit = objective.reserve_model(1, Some(10)).unwrap();
    objective
        .settle_model(
            permit.id,
            Some(Usage {
                input_tokens: 2,
                output_tokens: 3,
            }),
        )
        .unwrap();
    objective
        .finish_trigger("event", "e1", &claim.execution_id, json!(true))
        .unwrap();
    assert!(matches!(
        objective.emit_trigger("event", "e2".into(), json!(null)),
        Err(Error::ObjectiveLimit(_))
    ));
    assert!(matches!(
        objective.count_tool(0),
        Err(Error::ObjectiveLimit(_))
    ));
    assert!(matches!(
        objective.reserve_model(0, None),
        Err(Error::ObjectiveLimit(_))
    ));
    assert!(
        objective
            .emit_trigger("event", "e1".into(), json!(null))
            .unwrap()
            .unwrap()
            .result
            .is_some()
    );
    let snapshot = objective.snapshot().unwrap();
    assert_eq!(snapshot["model_calls"], 1);
    assert_eq!(snapshot["output_tokens"], 3);
}

#[test]
fn invalid_definitions_and_inputs_fail_before_any_attempt() {
    let objective = Objective::new(Default::default()).unwrap();
    for schedule in [
        TriggerSchedule::Interval {
            every_ms: 0,
            anchor_unix_ms: None,
        },
        TriggerSchedule::At { unix_ms: u64::MAX },
    ] {
        assert!(
            objective
                .register_trigger("invalid".into(), schedule)
                .is_err()
        );
    }
    objective
        .register_trigger("event".into(), TriggerSchedule::Event)
        .unwrap();
    assert!(
        objective
            .register_trigger("event".into(), TriggerSchedule::At { unix_ms: 1000 })
            .is_err()
    );
    objective.enable_trigger("event", true).unwrap();
    assert!(
        objective
            .emit_trigger("event", "".into(), json!(1))
            .is_err()
    );
    assert!(
        objective
            .emit_trigger("event", "large".into(), json!("x".repeat(65537)))
            .is_err()
    );
    assert!(objective.poll_trigger_at("event", 1000).is_err());
    assert_eq!(objective.snapshot().unwrap()["tool_calls"], 0);
}

#[test]
fn disabling_retains_uncertainty_and_cancellation_accepts_only_known_results() {
    let objective = Objective::new(Default::default()).unwrap();
    objective
        .register_trigger("event".into(), TriggerSchedule::Event)
        .unwrap();
    objective.enable_trigger("event", true).unwrap();
    let claim = objective
        .emit_trigger("event", "e1".into(), json!(1))
        .unwrap()
        .unwrap();
    objective.enable_trigger("event", false).unwrap();
    assert!(
        objective
            .emit_trigger("event", "e2".into(), json!(2))
            .unwrap()
            .is_none()
    );
    objective.enable_trigger("event", true).unwrap();
    assert!(matches!(
        objective.emit_trigger("event", "e2".into(), json!(2)),
        Err(Error::TriggerUncertain)
    ));
    objective.cancel().unwrap();
    objective
        .finish_trigger(
            "event",
            "e1",
            &claim.execution_id,
            json!("known late result"),
        )
        .unwrap();
    assert!(
        objective
            .emit_trigger("event", "e1".into(), json!(1))
            .unwrap()
            .unwrap()
            .result
            .is_some()
    );
    assert!(matches!(
        objective.emit_trigger("event", "e2".into(), json!(2)),
        Err(Error::ObjectiveLimit(_))
    ));
}

#[test]
fn durable_pending_occurrences_reopen_without_replay_and_null_is_a_known_result() {
    let directory = tempfile::tempdir().unwrap();
    let execution_id;
    {
        let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
        store
            .open_objective("root", ObjectivePolicy::default())
            .unwrap();
        store
            .objective_command(
                "root",
                r#"{"op":"trigger_register","id":"event","schedule":{"kind":"event"}}"#,
            )
            .unwrap();
        store
            .objective_command(
                "root",
                r#"{"op":"trigger_enable","id":"event","enabled":true}"#,
            )
            .unwrap();
        execution_id = store
            .objective_command(
                "root",
                r#"{"op":"trigger_emit","id":"event","key":"e1","input":{"n":1}}"#,
            )
            .unwrap()["execution_id"]
            .as_str()
            .unwrap()
            .to_owned();
    }
    let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    assert!(matches!(
        store.objective_command(
            "root",
            r#"{"op":"trigger_emit","id":"event","key":"e1","input":{"n":1}}"#
        ),
        Err(Error::TriggerUncertain)
    ));
    let resolution = json!({"op":"trigger_finish","id":"event","key":"e1","execution_id":execution_id,"result":null});
    store
        .objective_command("root", &resolution.to_string())
        .unwrap();
    let cached = store
        .objective_command(
            "root",
            r#"{"op":"trigger_emit","id":"event","key":"e1","input":{"n":1}}"#,
        )
        .unwrap();
    assert_eq!(cached["result"]["execution_id"], execution_id);
    assert!(cached["result"]["result"].is_null());
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 1);
}
