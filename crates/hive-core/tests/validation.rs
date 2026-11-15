use hive_agent_core::sessions::{OperationResult, SessionStore};
use hive_agent_core::{Action, Engine, LoopPolicy, ModelReply, StopReason, Tool, ToolCall};
use serde_json::{Value, json};

fn tool(schema: Value) -> Tool {
    Tool {
        name: "save".into(),
        description: "Save a record".into(),
        parameters: schema,
        allow_repeated_results: false,
    }
}
fn schema() -> Value {
    json!({"type":"object","properties":{"age":{"type":"integer","minimum":18}},
        "required":["age"],"additionalProperties":false})
}
fn initial(schema: Value) -> Engine {
    Engine::new("test".into(), None, vec![tool(schema)], 8).unwrap()
}
fn reply(id: &str, arguments: Value) -> ModelReply {
    ModelReply {
        provider_state: None,
        content: None,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "save".into(),
            arguments,
        }],
        usage: None,
    }
}
fn state(schema: Value, arguments: Value) -> Engine {
    let mut state = initial(schema);
    state.start("Save".into()).unwrap();
    state.accept_reply(reply("c", arguments)).unwrap();
    state
}

#[test]
fn schema_is_compiled_and_checked_at_registration() {
    for invalid in [
        json!({"type":"object","required":true}),
        json!({"type":"object","properties":{"age":{"type":"invalid-type"}}}),
        json!({"type":"object","properties":{"age":{"pattern":"["}}}),
    ] {
        assert!(Engine::new("test".into(), None, vec![tool(invalid)], 8).is_err());
    }
}

#[test]
fn remote_and_file_references_are_not_retrieved() {
    for reference in [
        "https://example.invalid/schema",
        "file:///tmp/hive-schema-not-allowed.json",
    ] {
        assert!(
            Engine::new(
                "test".into(),
                None,
                vec![tool(json!({"type":"object","$ref":reference}))],
                8
            )
            .is_err()
        );
    }
}

#[test]
fn invalid_arguments_produce_feedback_and_cannot_be_executed() {
    for input in [
        json!({}),
        json!({"age":"18"}),
        json!({"age":17}),
        json!({"age":20,"extra":true}),
        json!([]),
    ] {
        let mut state = state(schema(), input);
        let Action::ToolRejected { result, .. } = state.action() else {
            panic!("must reject");
        };
        assert_eq!(result["error"]["code"], "INVALID_TOOL_ARGUMENTS");
        assert!(!result["error"]["issues"].as_array().unwrap().is_empty());
        assert!(state.accept_tool("c", json!("bypass")).is_err());
        state.accept_rejection().unwrap();
        let Action::Model { request } = state.action() else {
            panic!("must let the model repair");
        };
        let feedback: Value =
            serde_json::from_str(request.messages.last().unwrap().content.as_ref().unwrap())
                .unwrap();
        assert_eq!(feedback["error"]["code"], "INVALID_TOOL_ARGUMENTS");
    }
}

#[test]
fn constraints_refs_composition_conditionals_and_formats_are_validated() {
    let cases = [
        (
            json!({"type":"object","properties":{"email":{"type":"string","format":"email"}},"required":["email"]}),
            json!({"email":"a@example.com"}),
            json!({"email":"not-an-email"}),
        ),
        (
            json!({"type":"object","properties":{"items":{"type":"array","items":{"enum":["a","b"]},"minItems":1,"uniqueItems":true}}}),
            json!({"items":["a","b"]}),
            json!({"items":["a","a"]}),
        ),
        (
            json!({"type":"object","properties":{"code":{"type":"string","pattern":"^[A-Z]+$","minLength":2,"maxLength":4}}}),
            json!({"code":"AB"}),
            json!({"code":"abcde"}),
        ),
        (
            json!({"type":"object","$defs":{"age":{"type":"integer","minimum":18}},"properties":{"age":{"$ref":"#/$defs/age"}}}),
            json!({"age":18}),
            json!({"age":17}),
        ),
        (
            json!({"type":"object","oneOf":[{"required":["name"]},{"required":["id"]}]}),
            json!({"id":1}),
            json!({"id":1,"name":"both"}),
        ),
        (
            json!({"type":"object","if":{"properties":{"kind":{"const":"adult"}},"required":["kind"]},
            "then":{"properties":{"age":{"minimum":18}},"required":["age"]},"else":{"required":["guardian"]}}),
            json!({"kind":"adult","age":21}),
            json!({"kind":"adult","age":17}),
        ),
        (
            json!({"$schema":"http://json-schema.org/draft-07/schema#","type":"object", "definitions":{"id":{"type":"integer"}},"properties":{"id":{"$ref":"#/definitions/id"}}}),
            json!({"id":1}),
            json!({"id":"bad"}),
        ),
        (
            json!({"type":"object","allOf":[{"properties":{"a":{"type":"number"}}}],"unevaluatedProperties":false}),
            json!({"a":1}),
            json!({"a":1,"b":2}),
        ),
    ];
    for (schema, valid, invalid) in cases {
        assert!(matches!(
            state(schema.clone(), valid).action(),
            Action::Tool { .. }
        ));
        assert!(matches!(
            state(schema, invalid).action(),
            Action::ToolRejected { .. }
        ));
    }
}

#[test]
fn validation_does_not_coerce_or_insert_defaults() {
    let schema = json!({"type":"object","properties":{"age":{"type":"integer","default":20}}});
    let valid = state(schema.clone(), json!({}));
    assert!(matches!(valid.action(), Action::Tool { call } if call.arguments == json!({})));
    assert!(matches!(
        state(schema, json!({"age":"20"})).action(),
        Action::ToolRejected { .. }
    ));
}

#[test]
fn feedback_is_bounded_and_does_not_echo_the_invalid_value() {
    let required: Vec<String> = (0..20).map(|n| format!("field{n}")).collect();
    let state = state(
        json!({"type":"object","required":required,
        "properties":{"secret":{"type":"integer"}}}),
        json!({"secret":"PRIVATE-INVALID-VALUE"}),
    );
    let Action::ToolRejected { result, .. } = state.action() else {
        panic!("must reject");
    };
    assert_eq!(result["error"]["issues"].as_array().unwrap().len(), 8);
    assert_eq!(result["error"]["truncated"], true);
    assert!(!result.to_string().contains("PRIVATE-INVALID-VALUE"));
}

#[test]
fn repeated_invalid_arguments_stop_without_tool_executions() {
    let mut state = initial(schema());
    state.start("Save".into()).unwrap();
    for id in ["one", "two"] {
        state.accept_reply(reply(id, json!({}))).unwrap();
        state.accept_rejection().unwrap();
    }
    assert!(
        matches!(state.action(), Action::Stopped { reason:StopReason::RepeatedToolResult,stats }
        if stats.model_calls==2 && stats.tool_calls==0 && stats.tool_rejections==2)
    );
}

#[test]
fn polling_opt_out_does_not_bypass_repeated_invalid_arguments() {
    let mut t = tool(schema());
    t.allow_repeated_results = true;
    let mut state = Engine::new("test".into(), None, vec![t], 8).unwrap();
    state.start("Save".into()).unwrap();
    for id in ["one", "two"] {
        state.accept_reply(reply(id, json!({}))).unwrap();
        state.accept_rejection().unwrap();
    }
    assert!(matches!(
        state.action(),
        Action::Stopped {
            reason: StopReason::RepeatedToolResult,
            ..
        }
    ));
}

#[test]
fn rejection_consumes_the_tool_attempt_budget_inside_a_batch() {
    let mut state = Engine::with_policy(
        "test".into(),
        None,
        vec![tool(schema())],
        8,
        LoopPolicy {
            max_tool_calls: 1,
            ..Default::default()
        },
    )
    .unwrap();
    state.start("Save".into()).unwrap();
    let mut calls = reply("invalid", json!({}));
    calls
        .tool_calls
        .push(reply("valid", json!({"age":20})).tool_calls.remove(0));
    state.accept_reply(calls).unwrap();
    state.accept_rejection().unwrap();
    assert!(
        matches!(state.action(),Action::Stopped {reason:StopReason::ToolCallLimit,stats}
        if stats.tool_calls==0 && stats.tool_rejections==1)
    );
}

#[test]
fn rejected_arguments_are_checkpointed_without_marking_an_external_operation() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    let frame = store
        .begin_run("a", "s", 0, initial(schema()), "Save".into())
        .unwrap();
    let claim = store.claim("a", "s", frame.revision).unwrap();
    let frame = store
        .advance(
            "a",
            "s",
            claim.revision,
            &claim.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: reply("invalid", json!({})),
            },
        )
        .unwrap();
    assert!(matches!(frame.action, Action::ToolRejected { .. }));
    assert!(store.claim("a", "s", frame.revision).is_err());
    let frame = store.reject("a", "s", frame.revision).unwrap();
    assert!(matches!(frame.action, Action::Model { .. }));
    assert!(store.reject("a", "s", frame.revision).is_err());
    let inspection = store.inspect("a", "s", None).unwrap();
    assert!(inspection["run"]["in_flight"].is_null());
    assert_eq!(inspection["run"]["tools"][0]["status"], "rejected");
    drop(store);
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    let loaded = store.load_run("a", "s", &initial(schema())).unwrap();
    assert_eq!(
        serde_json::to_value(loaded.state).unwrap()["stats"]["tool_rejections"],
        1
    );
}
