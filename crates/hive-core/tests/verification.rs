use hive_agent_core::{
    Error,
    objective::{Objective, ObjectivePolicy, ReviewEvidence, VerificationVerdict},
    sessions::SessionStore,
};
use serde_json::json;

fn verdict(passed: bool) -> VerificationVerdict {
    VerificationVerdict {
        passed,
        reason: Some("checked receipt".into()),
        evidence: Some(json!({"receipt":"r1"})),
    }
}

#[test]
fn explicit_verification_has_a_single_claim_and_cached_false_is_a_known_result() {
    let objective = Objective::new(Default::default()).unwrap();
    let claim = objective
        .begin_verification("receipt-v1".into(), json!({"n":1}), None)
        .unwrap();
    assert!(claim.result.is_none());
    assert!(matches!(
        objective.begin_verification("receipt-v1".into(), json!({"n":1}), None),
        Err(Error::VerificationUncertain)
    ));
    let result = objective
        .finish_verification("receipt-v1", &claim.execution_id, verdict(false))
        .unwrap();
    let cached = objective
        .begin_verification("receipt-v1".into(), json!({"n":1}), None)
        .unwrap();
    assert_eq!(cached.result.unwrap().verdict, result.verdict);
    assert_eq!(cached.execution_id, claim.execution_id);
    let snapshot = objective.snapshot().unwrap();
    assert_eq!(snapshot["tool_calls"], 1);
    assert_eq!(snapshot["model_calls"], 0);
    assert!(snapshot["stopped"].is_null());
}

#[test]
fn duplicate_keys_cannot_change_inputs_or_review_evidence() {
    let objective = Objective::new(Default::default()).unwrap();
    let evidence = ReviewEvidence {
        evaluation_id: "eval-v1".into(),
        justification: "contract test".into(),
    };
    let claim = objective
        .begin_verification("check".into(), json!(1), Some(evidence.clone()))
        .unwrap();
    objective
        .finish_verification("check", &claim.execution_id, verdict(true))
        .unwrap();
    for (input, review) in [(json!(2), Some(evidence)), (json!(1), None)] {
        assert!(matches!(
            objective.begin_verification("check".into(), input, review),
            Err(Error::Input(_))
        ));
    }
    assert_eq!(objective.snapshot().unwrap()["tool_calls"], 1);
}

#[test]
fn claim_consumes_the_same_root_attempt_allowance_as_tools() {
    let objective = Objective::new(ObjectivePolicy {
        max_tool_calls: 1,
        ..Default::default()
    })
    .unwrap();
    objective
        .begin_verification("check".into(), json!(null), None)
        .unwrap();
    assert!(matches!(
        objective.count_tool(0),
        Err(Error::ObjectiveLimit(_))
    ));
    assert!(matches!(
        objective.begin_verification("another".into(), json!(null), None),
        Err(Error::ObjectiveLimit(_))
    ));
    assert_eq!(objective.snapshot().unwrap()["tool_calls"], 1);
}

#[test]
fn input_and_evidence_are_validated_before_claim_without_coercion() {
    let objective = Objective::new(Default::default()).unwrap();
    assert!(
        objective
            .begin_verification(" ".into(), json!(null), None)
            .is_err()
    );
    assert!(
        objective
            .begin_verification("check".into(), json!("x".repeat(65537)), None)
            .is_err()
    );
    assert!(
        objective
            .begin_verification(
                "check".into(),
                json!(1),
                Some(ReviewEvidence {
                    evaluation_id: "".into(),
                    justification: "why".into()
                })
            )
            .is_err()
    );
    assert!(serde_json::from_value::<VerificationVerdict>(json!({"passed":"true"})).is_err());
    assert!(
        serde_json::from_value::<VerificationVerdict>(json!({"passed":true,"extra":1})).is_err()
    );
    assert_eq!(objective.snapshot().unwrap()["tool_calls"], 0);
}

#[test]
fn malformed_or_stale_completion_never_confirms_or_overwrites_a_verdict() {
    let objective = Objective::new(Default::default()).unwrap();
    let claim = objective
        .begin_verification("check".into(), json!(1), None)
        .unwrap();
    assert!(matches!(
        objective.finish_verification("check", "wrong-id", verdict(true)),
        Err(Error::SessionConflict)
    ));
    assert!(
        objective
            .finish_verification(
                "check",
                &claim.execution_id,
                VerificationVerdict {
                    passed: true,
                    reason: Some("x".repeat(4097)),
                    evidence: None
                }
            )
            .is_err()
    );
    assert!(matches!(
        objective.begin_verification("check".into(), json!(1), None),
        Err(Error::VerificationUncertain)
    ));
    objective
        .finish_verification("check", &claim.execution_id, verdict(false))
        .unwrap();
    assert!(matches!(
        objective.finish_verification("check", &claim.execution_id, verdict(true)),
        Err(Error::SessionConflict)
    ));
    assert_eq!(
        objective.snapshot().unwrap()["verifications"]["check"]["verdict"]["passed"],
        false
    );
}

#[test]
fn cancellation_and_expiration_keep_known_receipts_without_more_effects() {
    for deadline in [false, true] {
        let objective = Objective::new(ObjectivePolicy {
            max_run_ms: deadline.then_some(50),
            ..Default::default()
        })
        .unwrap();
        let claim = objective
            .begin_verification("check".into(), json!(1), None)
            .unwrap();
        if deadline {
            std::thread::sleep(std::time::Duration::from_millis(60));
        } else {
            objective.cancel().unwrap();
        }
        objective
            .finish_verification("check", &claim.execution_id, verdict(true))
            .unwrap();
        assert!(
            objective
                .begin_verification("check".into(), json!(1), None)
                .unwrap()
                .result
                .is_some()
        );
        assert!(matches!(
            objective.begin_verification("new".into(), json!(1), None),
            Err(Error::ObjectiveLimit(_))
        ));
        assert_eq!(
            objective.snapshot().unwrap()["stopped"],
            if deadline {
                "deadline_exceeded"
            } else {
                "cancelled"
            }
        );
    }
}

#[test]
fn durable_claim_and_resolution_reopen_without_repeating_the_callback() {
    let directory = tempfile::tempdir().unwrap();
    let execution_id;
    {
        let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
        store
            .open_objective("root", ObjectivePolicy::default())
            .unwrap();
        let claim = store
            .objective_command(
                "root",
                r#"{"op":"verify_begin","key":"check","input":{"n":1}}"#,
            )
            .unwrap();
        execution_id = claim["execution_id"].as_str().unwrap().to_owned();
    }
    let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    assert!(matches!(
        store.objective_command(
            "root",
            r#"{"op":"verify_begin","key":"check","input":{"n":1}}"#
        ),
        Err(Error::VerificationUncertain)
    ));
    let resolution = json!({"op":"verify_finish","key":"check","execution_id":execution_id,"verdict":{"passed":true,"evidence":{"receipt":"r1"}}});
    store
        .objective_command("root", &resolution.to_string())
        .unwrap();
    assert!(
        store
            .objective_command("root", &resolution.to_string())
            .is_err()
    );
    let cached = store
        .objective_command(
            "root",
            r#"{"op":"verify_begin","key":"check","input":{"n":1}}"#,
        )
        .unwrap();
    assert_eq!(cached["result"]["verdict"]["passed"], true);
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 1);
}
