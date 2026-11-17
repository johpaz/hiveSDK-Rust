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

// ---- cron triggers -----------------------------------------------------------------

#[cfg(feature = "scheduler")]
const DAY: u64 = 86_400_000;
#[cfg(feature = "scheduler")]
const HOUR: u64 = 3_600_000;

#[cfg(feature = "scheduler")]
fn cron(expression: &str, timezone: &str) -> TriggerSchedule {
    TriggerSchedule::Cron {
        expression: expression.into(),
        timezone: timezone.into(),
        dom_and_dow: false,
    }
}
#[cfg(feature = "scheduler")]
fn next_due(objective: &Objective, id: &str) -> u64 {
    objective.inspect_trigger(id).unwrap()["next_due_unix_ms"]
        .as_u64()
        .unwrap()
}

#[cfg(feature = "scheduler")]
#[test]
fn a_cron_trigger_fires_at_its_wall_clock_time_in_its_zone_and_advances_one_occurrence() {
    let objective = Objective::new(Default::default()).unwrap();
    // 08:00 every day in Bogota (UTC-5, no daylight saving) is 13:00 UTC.
    let registered = objective
        .register_trigger("daily".into(), cron("0 8 * * *", "America/Bogota"))
        .unwrap();
    assert_eq!(
        registered["enabled"], false,
        "like every trigger, it is off until enabled"
    );
    let first = next_due(&objective, "daily");
    assert_eq!(first % DAY, 13 * HOUR, "08:00 in Bogota");
    assert!(
        objective.poll_trigger_at("daily", first).unwrap().is_none(),
        "disabled triggers never claim"
    );
    objective.enable_trigger("daily", true).unwrap();
    assert!(
        objective
            .poll_trigger_at("daily", first - 1)
            .unwrap()
            .is_none(),
        "not due one millisecond early"
    );
    let claim = objective.poll_trigger_at("daily", first).unwrap().unwrap();
    assert_eq!(
        claim.occurrence_key,
        first.to_string(),
        "the occurrence is identified by its scheduled time"
    );
    assert_eq!(
        (
            claim.input["scheduled_for_unix_ms"].clone(),
            claim.input["skipped_occurrences"].clone()
        ),
        (json!(first), json!(0))
    );
    assert!(
        matches!(
            objective.poll_trigger_at("daily", first + 1),
            Err(Error::TriggerUncertain)
        ),
        "an occurrence in flight blocks the next claim"
    );
    objective
        .finish_trigger(
            "daily",
            &claim.occurrence_key,
            &claim.execution_id,
            json!({"ok":true}),
        )
        .unwrap();
    assert_eq!(
        next_due(&objective, "daily"),
        first + DAY,
        "exactly one occurrence later"
    );
    assert!(
        objective
            .poll_trigger_at("daily", first + DAY - 1)
            .unwrap()
            .is_none()
    );
    let snapshot = objective.snapshot().unwrap();
    assert_eq!(
        (
            snapshot["tool_calls"].clone(),
            snapshot["model_calls"].clone()
        ),
        (json!(1), json!(0)),
        "a claim is charged like any other attempt and calls no model"
    );
}

#[cfg(feature = "scheduler")]
#[test]
fn missed_cron_occurrences_are_coalesced_into_one_claim_with_a_count_never_a_burst() {
    let objective = Objective::new(Default::default()).unwrap();
    objective
        .register_trigger("daily".into(), cron("0 8 * * *", "UTC"))
        .unwrap();
    objective.enable_trigger("daily", true).unwrap();
    let first = next_due(&objective, "daily");
    // Off for three and a bit days: the latest due occurrence is claimed, three earlier ones were skipped.
    let observed = first + 3 * DAY + HOUR;
    let claim = objective
        .poll_trigger_at("daily", observed)
        .unwrap()
        .unwrap();
    assert_eq!(claim.input["scheduled_for_unix_ms"], first + 3 * DAY);
    assert_eq!(
        (
            claim.input["skipped_occurrences"].clone(),
            claim.input["observed_unix_ms"].clone()
        ),
        (json!(3), json!(observed))
    );
    assert_eq!(
        next_due(&objective, "daily"),
        first + 4 * DAY,
        "the next one is in the future"
    );
    objective
        .finish_trigger(
            "daily",
            &claim.occurrence_key,
            &claim.execution_id,
            json!(null),
        )
        .unwrap();
    assert!(
        objective
            .poll_trigger_at("daily", observed)
            .unwrap()
            .is_none(),
        "no catch-up claims follow"
    );
    // A trigger every minute that was off for two days still yields one claim.
    objective
        .register_trigger("minutely".into(), cron("* * * * *", "UTC"))
        .unwrap();
    objective.enable_trigger("minutely", true).unwrap();
    let start = next_due(&objective, "minutely");
    let claim = objective
        .poll_trigger_at("minutely", start + 2 * DAY + 30_000)
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.input["scheduled_for_unix_ms"],
        start + 2 * DAY,
        "the latest due occurrence, found without visiting all 2880"
    );
    assert_eq!(
        (
            claim.input["skipped_occurrences"].clone(),
            claim.input["skipped_is_lower_bound"].clone()
        ),
        (json!(100), json!(true)),
        "the count is exact up to 100, then a lower bound"
    );
    objective
        .finish_trigger(
            "minutely",
            &claim.occurrence_key,
            &claim.execution_id,
            json!(1),
        )
        .unwrap();
    assert_eq!(next_due(&objective, "minutely"), start + 2 * DAY + 60_000);
    // The work is bounded however long the outage: a per-second trigger off for a day (86400
    // occurrences) is still one claim, found and counted in a few hundred steps.
    objective
        .register_trigger("secondly".into(), cron("* * * * * *", "UTC"))
        .unwrap();
    objective.enable_trigger("secondly", true).unwrap();
    let begin = next_due(&objective, "secondly");
    let observed = begin + DAY;
    let claim = objective
        .poll_trigger_at("secondly", observed)
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.input["scheduled_for_unix_ms"],
        begin + DAY,
        "still the latest occurrence that is due"
    );
    assert_eq!(
        (
            claim.input["skipped_occurrences"].clone(),
            claim.input["skipped_is_lower_bound"].clone()
        ),
        (json!(100), json!(true))
    );
    assert!(
        next_due(&objective, "secondly") > observed,
        "and the next occurrence is still in the future"
    );
}

#[cfg(feature = "scheduler")]
#[test]
fn cron_expressions_use_the_shared_calendar_including_weekdays_and_invalid_ones_fail_before_any_state()
 {
    let objective = Objective::new(Default::default()).unwrap();
    objective
        .register_trigger("weekdays".into(), cron("0 9 * * MON-FRI", "UTC"))
        .unwrap();
    for _ in 0..3 {
        let due = next_due(&objective, "weekdays");
        let weekday = ((due / DAY) + 4) % 7; // 1970-01-01 was a Thursday; 0 is Sunday
        assert!((1..=5).contains(&weekday), "{weekday}");
        assert_eq!(due % DAY, 9 * HOUR);
        objective.enable_trigger("weekdays", true).unwrap();
        let claim = objective.poll_trigger_at("weekdays", due).unwrap().unwrap();
        objective
            .finish_trigger(
                "weekdays",
                &claim.occurrence_key,
                &claim.execution_id,
                json!(0),
            )
            .unwrap();
    }
    for (expression, timezone) in [
        ("80 * * * *", "UTC"),
        ("*/0 * * * *", "UTC"),
        ("", "UTC"),
        ("* * * * *", "Mars/Base"),
        ("* * * * *", ""),
        ("0 0 30 2 *", "UTC"),
        ("not a cron", "UTC"),
    ] {
        let error = objective
            .register_trigger("bad".into(), cron(expression, timezone))
            .unwrap_err();
        assert!(
            matches!(error, Error::Input(_)),
            "{expression} {timezone}: {error:?}"
        );
        assert!(
            objective.inspect_trigger("bad").is_err(),
            "nothing was registered for {expression}"
        );
    }
    // Registering the same definition again is idempotent; another one under the same id is refused.
    let first = objective
        .register_trigger("weekdays".into(), cron("0 9 * * MON-FRI", "UTC"))
        .unwrap();
    assert_eq!(first["schedule"]["expression"], "0 9 * * MON-FRI");
    assert!(
        objective
            .register_trigger("weekdays".into(), cron("0 10 * * MON-FRI", "UTC"))
            .is_err()
    );
    // A six-field expression carries seconds; dom_and_dow is part of the definition.
    let and = TriggerSchedule::Cron {
        expression: "0 0 13 * FRI".into(),
        timezone: "UTC".into(),
        dom_and_dow: true,
    };
    assert!(
        objective
            .register_trigger("friday13".into(), and.clone())
            .is_ok()
    );
    assert!(
        objective
            .register_trigger("friday13".into(), cron("0 0 13 * FRI", "UTC"))
            .is_err(),
        "OR and AND are different schedules"
    );
}

#[cfg(feature = "scheduler")]
#[test]
fn a_cron_trigger_survives_a_reopen_and_a_pending_occurrence_is_never_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let (key, execution_id);
    {
        let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
        store
            .open_objective("root", ObjectivePolicy::default())
            .unwrap();
        // Every second, so the real clock reaches an occurrence quickly.
        store
            .objective_command("root", r#"{"op":"trigger_register","id":"fast","schedule":{"kind":"cron","expression":"* * * * * *","timezone":"America/Bogota"}}"#)
            .unwrap();
        store
            .objective_command(
                "root",
                r#"{"op":"trigger_enable","id":"fast","enabled":true}"#,
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1300));
        let claim = store
            .objective_command("root", r#"{"op":"trigger_tick","id":"fast"}"#)
            .unwrap();
        key = claim["occurrence_key"].as_str().unwrap().to_owned();
        execution_id = claim["execution_id"].as_str().unwrap().to_owned();
        assert_eq!(claim["input"]["scheduled_for_unix_ms"].to_string(), key);
    }
    // The process died with the occurrence in flight: reopening does not run it again.
    let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    assert!(matches!(
        store.objective_command("root", r#"{"op":"trigger_tick","id":"fast"}"#),
        Err(Error::TriggerUncertain)
    ));
    let snapshot = store.inspect_objective("root").unwrap();
    assert_eq!(
        snapshot["triggers"]["fast"]["schedule"]["timezone"], "America/Bogota",
        "the definition was stored"
    );
    store
        .objective_command("root", &json!({"op":"trigger_finish","id":"fast","key":key,"execution_id":execution_id,"result":{"done":true}}).to_string())
        .unwrap();
    let snapshot = store.inspect_objective("root").unwrap();
    let scheduled: u64 = key.parse().unwrap();
    assert!(
        snapshot["triggers"]["fast"]["next_due_unix_ms"]
            .as_u64()
            .unwrap()
            > scheduled,
        "it moves on to a later occurrence"
    );
    assert_eq!(
        snapshot["tool_calls"], 1,
        "the claim was charged once and never again"
    );
}

#[cfg(not(feature = "scheduler"))]
#[test]
fn without_the_scheduler_feature_cron_triggers_are_refused_with_a_clear_reason() {
    let objective = Objective::new(Default::default()).unwrap();
    let error = objective
        .register_trigger(
            "daily".into(),
            TriggerSchedule::Cron {
                expression: "0 8 * * *".into(),
                timezone: "UTC".into(),
                dom_and_dow: false,
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("scheduler"), "{error}");
}
