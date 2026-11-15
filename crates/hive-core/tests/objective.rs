use hive_agent_core::{
    Usage,
    objective::{Objective, ObjectivePolicy},
};
use std::sync::{Arc, Barrier};

#[test]
fn concurrent_reservations_never_multiply_capacity() {
    let objective = Arc::new(
        Objective::new(ObjectivePolicy {
            max_model_calls: 100,
            max_output_tokens: 10,
            max_total_output_tokens: 50,
            ..Default::default()
        })
        .unwrap(),
    );
    let barrier = Arc::new(Barrier::new(20));
    let handles: Vec<_> = (0..20)
        .map(|_| {
            let objective = objective.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                objective.reserve_model(0, None).ok()
            })
        })
        .collect();
    let permits: Vec<_> = handles
        .into_iter()
        .filter_map(|h| h.join().unwrap())
        .collect();
    assert_eq!(permits.len(), 5);
    assert_eq!(permits.iter().map(|p| p.max_output_tokens).sum::<u64>(), 50);
    assert_eq!(objective.snapshot().unwrap()["model_calls"], 5);
}

#[test]
fn settlement_is_single_use_and_releases_only_unused_reservation() {
    let objective = Objective::new(ObjectivePolicy {
        max_output_tokens: 10,
        max_total_output_tokens: 15,
        ..Default::default()
    })
    .unwrap();
    let first = objective.reserve_model(0, None).unwrap();
    let second = objective.reserve_model(1, None).unwrap();
    assert_eq!(second.max_output_tokens, 5);
    objective
        .settle_model(
            first.id,
            Some(Usage {
                input_tokens: 20,
                output_tokens: 3,
            }),
        )
        .unwrap();
    assert!(
        objective
            .settle_model(first.id, Some(Usage::default()))
            .is_err()
    );
    let third = objective.reserve_model(2, Some(4)).unwrap();
    assert_eq!(third.max_output_tokens, 4);
    let view = objective.snapshot().unwrap();
    assert_eq!(view["input_tokens"], 20);
    assert_eq!(view["output_tokens"], 3);
    assert_eq!(view["reservations"]["2"], 5);
}

#[test]
fn unmetered_and_over_cap_replies_fail_closed() {
    for usage in [
        None,
        Some(Usage {
            input_tokens: 1,
            output_tokens: 11,
        }),
    ] {
        let objective = Objective::new(ObjectivePolicy {
            max_output_tokens: 10,
            ..Default::default()
        })
        .unwrap();
        let permit = objective.reserve_model(0, None).unwrap();
        objective.settle_model(permit.id, usage).unwrap();
        assert!(objective.check(0).is_err());
        assert!(objective.reserve_model(0, None).is_err());
        assert!(objective.count_tool(0).is_err());
        assert!(
            objective
                .settle_model(permit.id, Some(Usage::default()))
                .is_err()
        );
    }
}

#[test]
fn depth_counts_tools_and_calls_are_shared_and_never_reset() {
    let objective = Objective::new(ObjectivePolicy {
        max_model_calls: 2,
        max_tool_calls: 1,
        max_depth: 1,
        ..Default::default()
    })
    .unwrap();
    assert!(objective.reserve_model(2, None).is_err());
    assert_eq!(objective.snapshot().unwrap()["model_calls"], 0);
    objective.count_tool(1).unwrap();
    assert!(objective.count_tool(0).is_err());
    for depth in [0, 1] {
        let permit = objective.reserve_model(depth, None).unwrap();
        objective
            .settle_model(permit.id, Some(Usage::default()))
            .unwrap();
    }
    assert!(objective.reserve_model(0, None).is_err());
}

#[test]
fn cancellation_is_terminal_but_known_late_usage_can_settle() {
    let objective = Objective::new(Default::default()).unwrap();
    let permit = objective.reserve_model(0, None).unwrap();
    objective.cancel().unwrap();
    objective
        .settle_model(
            permit.id,
            Some(Usage {
                input_tokens: 1,
                output_tokens: 2,
            }),
        )
        .unwrap();
    assert!(objective.check(0).is_err());
    let snapshot = objective.snapshot().unwrap();
    assert_eq!(snapshot["output_tokens"], 2);
    assert_eq!(snapshot["stopped"], "cancelled");
}

#[test]
fn absolute_deadline_includes_idle_time() {
    let objective = Objective::new(ObjectivePolicy {
        max_run_ms: Some(1),
        ..Default::default()
    })
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(objective.reserve_model(0, None).is_err());
    assert_eq!(objective.snapshot().unwrap()["model_calls"], 0);
    assert!(objective.count_tool(0).is_err());
}

#[test]
fn invalid_policy_and_unknown_commands_cannot_change_accounting() {
    for policy in [
        r#"{"max_model_calls":0}"#,
        r#"{"max_total_output_tokens":0}"#,
        r#"{"max_run_ms":0}"#,
    ] {
        assert!(Objective::new(serde_json::from_str(policy).unwrap()).is_err());
    }
    assert!(serde_json::from_str::<ObjectivePolicy>(r#"{"unknown":true}"#).is_err());
    let objective = Objective::new(Default::default()).unwrap();
    assert!(objective.dispatch(r#"{"op":"reset"}"#).is_err());
    assert!(
        objective
            .dispatch(r#"{"op":"model","depth":0,"cap":0}"#)
            .is_err()
    );
    assert_eq!(objective.snapshot().unwrap()["model_calls"], 0);
}
