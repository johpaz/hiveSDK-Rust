use hive_agent_core::objective::{Objective, ObjectivePolicy, SummaryInput};
use hive_agent_core::sessions::SessionStore;
use hive_agent_core::{Engine, Error, ModelReply, Usage};
use serde_json::json;

fn input(prompt: &str) -> SummaryInput {
    let mut state = Engine::new("test".into(), None, vec![], 8).unwrap();
    state.start(prompt.into()).unwrap();
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: Some("Known fact".into()),
            tool_calls: vec![],
            usage: None,
        })
        .unwrap();
    SummaryInput {
        source: state.capture_context(0, 2).unwrap(),
        messages: state.messages().to_vec(),
    }
}
fn policy() -> ObjectivePolicy {
    ObjectivePolicy {
        max_model_calls: 3,
        max_output_tokens: 10,
        max_total_output_tokens: 30,
        ..Default::default()
    }
}

#[test]
fn explicit_programmatic_summary_is_cached_and_never_calls_a_model() {
    let objective = Objective::new(policy()).unwrap();
    let original = input("R7 viernes");
    let claim = objective
        .begin_summary("s1".into(), original.clone())
        .unwrap();
    assert!(claim.result.is_none());
    let receipt = objective
        .finish_summary("s1", &claim.execution_id, "R7 viernes".into())
        .unwrap();
    assert_eq!(receipt.summary.source, original.source);
    assert_eq!(receipt.summary.id, "s1");
    assert_eq!(receipt.cost.model_calls, 0);
    assert_eq!(receipt.cost.tool_calls, 1);
    let cached = objective
        .begin_summary("s1".into(), original)
        .unwrap()
        .result
        .unwrap();
    assert_eq!(cached.execution_id, receipt.execution_id);
    assert_eq!(objective.snapshot().unwrap()["tool_calls"], 1);
    assert!(
        objective
            .begin_summary("s1".into(), input("Other fact"))
            .is_err()
    );
}

#[test]
fn scoped_cost_excludes_concurrent_summaries_and_unrelated_root_work() {
    let objective = Objective::new(policy()).unwrap();
    let a = objective.begin_summary("a".into(), input("A")).unwrap();
    let b = objective.begin_summary("b".into(), input("B")).unwrap();
    let unrelated = objective.reserve_model(0, Some(10)).unwrap();
    let am = objective.reserve_summary_model("a", 1, Some(10)).unwrap();
    let bm = objective.reserve_summary_model("b", 1, Some(10)).unwrap();
    objective.count_summary_tool("a", 1).unwrap();
    for (id, input_tokens, output_tokens) in [(unrelated.id, 100, 7), (am.id, 3, 4), (bm.id, 6, 2)]
    {
        objective
            .settle_model(
                id,
                Some(Usage {
                    input_tokens,
                    output_tokens,
                }),
            )
            .unwrap();
    }
    let a = objective
        .finish_summary("a", &a.execution_id, "A summary".into())
        .unwrap();
    let b = objective
        .finish_summary("b", &b.execution_id, "B summary".into())
        .unwrap();
    assert_eq!(
        (
            a.cost.model_calls,
            a.cost.tool_calls,
            a.cost.input_tokens,
            a.cost.output_tokens
        ),
        (1, 2, 3, 4)
    );
    assert_eq!(
        (
            b.cost.model_calls,
            b.cost.tool_calls,
            b.cost.input_tokens,
            b.cost.output_tokens
        ),
        (1, 1, 6, 2)
    );
    let root = objective.snapshot().unwrap();
    assert_eq!(root["input_tokens"], 109);
    assert_eq!(root["output_tokens"], 13);
}

#[test]
fn unknown_model_cost_remains_reserved_and_late_known_usage_updates_the_receipt() {
    let objective = Objective::new(policy()).unwrap();
    let value = input("R7");
    let claim = objective.begin_summary("s".into(), value.clone()).unwrap();
    let permit = objective.reserve_summary_model("s", 1, Some(10)).unwrap();
    assert_eq!(
        objective.snapshot().unwrap()["summary_costs"]["s"]["pending_model_calls"],
        1
    );
    objective.cancel().unwrap();
    let result = objective
        .finish_summary("s", &claim.execution_id, "Known summary".into())
        .unwrap();
    assert_eq!(result.cost.pending_model_calls, 1);
    assert_eq!(result.cost.reserved_output_tokens, 10);
    assert_eq!(result.cost.unmetered_model_calls, 0);
    objective
        .settle_model(
            permit.id,
            Some(Usage {
                input_tokens: 4,
                output_tokens: 3,
            }),
        )
        .unwrap();
    let cached = objective
        .begin_summary("s".into(), value)
        .unwrap()
        .result
        .unwrap();
    assert_eq!(cached.cost.pending_model_calls, 0);
    assert_eq!(cached.cost.reserved_output_tokens, 0);
    assert_eq!(cached.cost.input_tokens, 4);
    assert_eq!(cached.cost.output_tokens, 3);
    assert!(objective.reserve_summary_model("s", 1, Some(10)).is_err());
}

#[test]
fn missing_usage_is_explicit_and_never_refunds_reserved_auxiliary_output() {
    let objective = Objective::new(policy()).unwrap();
    let claim = objective.begin_summary("s".into(), input("R7")).unwrap();
    let model = objective.reserve_summary_model("s", 1, Some(10)).unwrap();
    objective.settle_model(model.id, None).unwrap();
    let receipt = objective
        .finish_summary("s", &claim.execution_id, "Known summary".into())
        .unwrap();
    assert_eq!(receipt.cost.unmetered_model_calls, 1);
    assert_eq!(receipt.cost.pending_model_calls, 0);
    assert_eq!(receipt.cost.reserved_output_tokens, 10);
    assert_eq!(receipt.cost.output_tokens, 0);
    assert!(matches!(
        objective.reserve_model(0, Some(10)),
        Err(Error::ObjectiveLimit(_))
    ));
}

#[test]
fn summary_claims_and_scoped_effects_share_root_quota_and_depth() {
    let objective = Objective::new(ObjectivePolicy {
        max_tool_calls: 1,
        max_model_calls: 1,
        max_depth: 1,
        ..policy()
    })
    .unwrap();
    let claim = objective.begin_summary("s".into(), input("R7")).unwrap();
    assert!(matches!(
        objective.begin_summary("another".into(), input("R8")),
        Err(Error::ObjectiveLimit(_))
    ));
    assert!(objective.reserve_summary_model("s", 0, Some(10)).is_err());
    assert!(matches!(
        objective.reserve_summary_model("s", 2, Some(10)),
        Err(Error::ObjectiveLimit(_))
    ));
    assert!(objective.count_summary_tool("s", 1).is_err());
    objective.reserve_summary_model("s", 1, Some(10)).unwrap();
    assert!(objective.reserve_summary_model("s", 1, Some(10)).is_err());
    let receipt = objective
        .finish_summary("s", &claim.execution_id, "R7".into())
        .unwrap();
    assert_eq!(receipt.cost.model_calls, 1);
    assert_eq!(receipt.cost.tool_calls, 1);
}

#[test]
fn invalid_inputs_pending_keys_and_stale_resolvers_cannot_repeat_or_overwrite_work() {
    let objective = Objective::new(policy()).unwrap();
    let mut changed = input("R7");
    changed.messages[0].content = Some("Changed".into());
    assert!(objective.begin_summary("bad".into(), changed).is_err());
    assert!(objective.begin_summary("".into(), input("R7")).is_err());
    assert_eq!(objective.snapshot().unwrap()["tool_calls"], 0);
    let claim = objective.begin_summary("s".into(), input("R7")).unwrap();
    assert!(matches!(
        objective.begin_summary("s".into(), input("R7")),
        Err(Error::SummaryUncertain)
    ));
    assert!(
        objective
            .finish_summary("s", "stale", "summary".into())
            .is_err()
    );
    assert!(
        objective
            .finish_summary("s", &claim.execution_id, " ".into())
            .is_err()
    );
    objective
        .finish_summary("s", &claim.execution_id, "summary".into())
        .unwrap();
    assert!(
        objective
            .finish_summary("s", &claim.execution_id, "Changed".into())
            .is_err()
    );
    assert!(objective.reserve_summary_model("s", 1, Some(10)).is_err());
    assert_eq!(objective.snapshot().unwrap()["tool_calls"], 1);
}

#[test]
fn durable_summary_reopens_without_replay_and_keeps_attributable_model_usage() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    store.open_objective("root", policy()).unwrap();
    let value = input("R7");
    let command = json!({"op":"summary_begin","key":"s","input":value});
    let claim = store
        .objective_command("root", &command.to_string())
        .unwrap();
    let model = store
        .objective_command(
            "root",
            &json!({"op":"summary_model","key":"s","depth":1,"cap":10}).to_string(),
        )
        .unwrap();
    store
        .objective_command(
            "root",
            &json!({"op":"summary_tool","key":"s","depth":1}).to_string(),
        )
        .unwrap();
    drop(store);
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    store.open_objective("root", policy()).unwrap();
    assert!(matches!(
        store.objective_command("root", &command.to_string()),
        Err(Error::SummaryUncertain)
    ));
    let result=store.objective_command("root",&json!({"op":"summary_finish","key":"s","execution_id":claim["execution_id"],"content":"R7 summary"}).to_string()).unwrap();
    assert_eq!(result["cost"]["pending_model_calls"], 1);
    assert_eq!(result["cost"]["tool_calls"], 2);
    store
        .objective_command(
            "root",
            &json!({"op":"settle","id":model["id"],"usage":{"input_tokens":5,"output_tokens":3}})
                .to_string(),
        )
        .unwrap();
    let cached = store
        .objective_command("root", &command.to_string())
        .unwrap();
    assert_eq!(cached["result"]["cost"]["input_tokens"], 5);
    assert_eq!(cached["result"]["cost"]["output_tokens"], 3);
    assert_eq!(cached["result"]["cost"]["pending_model_calls"], 0);
    assert_eq!(store.inspect_objective("root").unwrap()["model_calls"], 1);
}

#[test]
fn scoped_session_claims_and_clean_resume_preserve_attribution_and_remaining_budget() {
    use hive_agent_core::sessions::OperationResult;
    use hive_agent_core::{Action, Tool, ToolCall};
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    store.open_objective("root", policy()).unwrap();
    let claim = store
        .objective_command(
            "root",
            &json!({"op":"summary_begin","key":"s","input":input("R7")}).to_string(),
        )
        .unwrap();
    let mut initial = Engine::new(
        "test".into(),
        None,
        vec![Tool {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type":"object"}),
            allow_repeated_results: false,
        }],
        8,
    )
    .unwrap();
    initial.bind_objective("root".into()).unwrap();
    let start = store
        .begin_run_scoped(
            "a",
            "s",
            0,
            initial.clone(),
            "Summarize".into(),
            1,
            Some("s".into()),
        )
        .unwrap();
    let model = store.claim("a", "s", start.revision).unwrap();
    let tool = store
        .advance(
            "a",
            "s",
            model.revision,
            &model.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: ModelReply {
                    provider_state: None,
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "c".into(),
                        name: "read".into(),
                        arguments: json!({}),
                    }],
                    usage: Some(Usage {
                        input_tokens: 3,
                        output_tokens: 1,
                    }),
                },
            },
        )
        .unwrap();
    let tool = store.claim("a", "s", tool.revision).unwrap();
    let next = store
        .advance(
            "a",
            "s",
            tool.revision,
            &tool.operation.unwrap().execution_id,
            OperationResult::Tool { result: json!(5) },
        )
        .unwrap();
    drop(store);
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    let loaded = store.load_run("a", "s", &initial).unwrap();
    assert_eq!(loaded.summary_key.as_deref(), Some("s"));
    assert_eq!(loaded.objective_depth, 1);
    let resumed = store.resume_run("a", "s", next.revision).unwrap();
    let model = store.claim("a", "s", resumed.revision).unwrap();
    let done = store
        .advance(
            "a",
            "s",
            model.revision,
            &model.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: ModelReply {
                    provider_state: None,
                    content: Some("R7 summary".into()),
                    tool_calls: vec![],
                    usage: Some(Usage {
                        input_tokens: 2,
                        output_tokens: 2,
                    }),
                },
            },
        )
        .unwrap();
    let receipt=store.objective_command("root",&json!({"op":"summary_finish","key":"s","execution_id":claim["execution_id"],"content":"R7 summary"}).to_string()).unwrap();
    assert_eq!(receipt["cost"]["model_calls"], 2);
    assert_eq!(receipt["cost"]["tool_calls"], 2);
    assert_eq!(receipt["cost"]["input_tokens"], 5);
    assert_eq!(receipt["cost"]["output_tokens"], 3);
    let resumed = store.resume_run("a", "s", done.revision).unwrap();
    assert!(matches!(resumed.action, Action::Complete { .. }));
    store
        .commit("a", "s", resumed.revision, resumed.state)
        .unwrap();
    assert!(
        store
            .begin_run_scoped(
                "b",
                "s",
                0,
                initial,
                "Late work".into(),
                1,
                Some("s".into())
            )
            .is_err()
    );
    assert_eq!(store.inspect_objective("root").unwrap()["model_calls"], 2);
}

#[test]
fn scoped_checkpoints_are_fenced_from_readers_that_ignore_attribution() {
    use hivedb_core::{HiveDB, PutOptions};
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    store.open_objective("root", policy()).unwrap();
    store
        .objective_command(
            "root",
            &json!({"op":"summary_begin","key":"s","input":input("R7")}).to_string(),
        )
        .unwrap();
    let mut initial = Engine::new("test".into(), None, vec![], 8).unwrap();
    initial.bind_objective("root".into()).unwrap();
    store
        .begin_run_scoped(
            "a",
            "s",
            0,
            initial.clone(),
            "Work".into(),
            1,
            Some("s".into()),
        )
        .unwrap();
    drop(store);
    let db = HiveDB::open(directory.path()).unwrap();
    let key = serde_json::to_string(&("t", "a", "s")).unwrap();
    let mut record = db
        .col_get("hive_sdk_rust_sessions_v1", &key)
        .unwrap()
        .unwrap()
        .doc;
    assert_eq!(record["schema_version"], 5);
    record["schema_version"] = json!(4);
    db.col_put(
        "hive_sdk_rust_sessions_v1",
        &key,
        &record,
        PutOptions::default(),
    )
    .unwrap();
    drop(db);
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    assert!(store.load_run("a", "s", &initial).is_err());
    assert_eq!(store.inspect_objective("root").unwrap()["model_calls"], 0);
}

#[test]
fn schema_rejections_in_summary_children_consume_the_same_scoped_tool_allowance() {
    use hive_agent_core::sessions::OperationResult;
    use hive_agent_core::{Tool, ToolCall};
    for quota in [1, 2] {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path(), "t".into()).unwrap();
        store
            .open_objective(
                "root",
                ObjectivePolicy {
                    max_model_calls: 1,
                    max_tool_calls: quota,
                    ..policy()
                },
            )
            .unwrap();
        store
            .objective_command(
                "root",
                &json!({"op":"summary_begin","key":"s","input":input("R7")}).to_string(),
            )
            .unwrap();
        let mut initial=Engine::new("test".into(),None,vec![Tool {name:"read".into(),description:"Read".into(),parameters:json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),allow_repeated_results:false}],8).unwrap();
        initial.bind_objective("root".into()).unwrap();
        let frame = store
            .begin_run_scoped("a", "s", 0, initial, "Work".into(), 1, Some("s".into()))
            .unwrap();
        let model = store.claim("a", "s", frame.revision).unwrap();
        let rejected = store
            .advance(
                "a",
                "s",
                model.revision,
                &model.operation.unwrap().execution_id,
                OperationResult::Model {
                    reply: ModelReply {
                        provider_state: None,
                        content: None,
                        tool_calls: vec![ToolCall {
                            id: "bad".into(),
                            name: "read".into(),
                            arguments: json!({}),
                        }],
                        usage: Some(Usage {
                            input_tokens: 1,
                            output_tokens: 1,
                        }),
                    },
                },
            )
            .unwrap();
        let result = store.reject("a", "s", rejected.revision);
        if quota == 1 {
            assert!(matches!(result, Err(Error::ObjectiveLimit(_))));
        } else {
            let next = result.unwrap();
            assert!(matches!(
                store.claim("a", "s", next.revision),
                Err(Error::ObjectiveLimit(_))
            ));
            assert_eq!(
                store.inspect("a", "s", None).unwrap()["run"]["tools"][0]["status"],
                "rejected"
            );
        }
        let snapshot = store.inspect_objective("root").unwrap();
        assert_eq!(snapshot["model_calls"], 1);
        assert_eq!(snapshot["tool_calls"], quota);
        assert_eq!(snapshot["summary_costs"]["s"]["tool_calls"], quota);
    }
}
