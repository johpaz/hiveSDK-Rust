use hive_agent_core::{dispatch, handles::EngineHandle};
use serde_json::{Value, json};

fn command(handle: &EngineHandle, value: Value) -> Value {
    serde_json::from_str(&handle.dispatch(&value.to_string()).unwrap()).unwrap()
}
fn config() -> Value {
    json!({"op":"create","model":"test","tools":[{"name":"read","description":"Read","parameters":{"type":"object"}}],"policy":{"max_output_tokens":8,"structured_output":{"name":"answer","schema":{"type":"object","required":["answer"],"properties":{"answer":{"type":"string"}},"additionalProperties":false}}}})
}
fn tool_reply() -> Value {
    json!({"content":"Reading", "tool_calls":[{"id":"c","name":"read","arguments":{}}],"usage":{"input_tokens":3,"output_tokens":2}})
}
fn final_reply() -> Value {
    json!({"content":" \n{\"answer\":\"sí🦀\"}\t","usage":{"input_tokens":4,"output_tokens":2}})
}

#[test]
fn owned_transitions_match_legacy_actions_without_state_roundtrips() {
    let config = config();
    let handle = EngineHandle::new(&config.to_string()).unwrap();
    let mut legacy: Value = serde_json::from_str(&dispatch(&config.to_string()).unwrap()).unwrap();
    for mut input in [
        json!({"op":"start","prompt":"Answer"}),
        json!({"op":"reply","reply":tool_reply()}),
        json!({"op":"tool_result","id":"c","result":5}),
        json!({"op":"reply","reply":final_reply()}),
    ] {
        let owned = command(&handle, input.clone());
        input["state"] = legacy["state"].clone();
        legacy = serde_json::from_str(&dispatch(&input.to_string()).unwrap()).unwrap();
        assert_eq!(owned["action"], legacy["action"]);
        assert_eq!(owned["state"].as_object().unwrap().len(), 1);
        assert!(owned["state"].get("messages").is_none());
        assert_eq!(command(&handle, json!({"op":"messages"})), json!([]));
    }
    command(&handle, json!({"op":"commit"}));
    assert_eq!(
        command(&handle, json!({"op":"messages"})),
        legacy["state"]["messages"]
    );
    command(&handle, json!({"op":"abort"}));
    assert_eq!(
        command(&handle, json!({"op":"messages"})),
        legacy["state"]["messages"]
    );
}

#[test]
fn active_turn_guards_and_failed_commands_cannot_publish_history() {
    let handle = EngineHandle::new(&config().to_string()).unwrap();
    assert!(handle.dispatch(r#"{"op":"commit"}"#).is_err());
    command(&handle, json!({"op":"start","prompt":"Answer"}));
    assert!(
        handle
            .dispatch(r#"{"op":"start","prompt":"Again"}"#)
            .is_err()
    );
    assert!(handle.dispatch(r#"{"op":"commit"}"#).is_err());
    assert!(
        handle
            .dispatch(r#"{"op":"set_context_plan","plan":null}"#)
            .is_err()
    );
    assert!(
        handle
            .dispatch(r#"{"op":"tool_result","id":"wrong","result":5}"#)
            .is_err()
    );
    command(&handle, json!({"op":"reply","reply":tool_reply()}));
    command(&handle, json!({"op":"tool_result","id":"c","result":5}));
    command(&handle, json!({"op":"abort"}));
    assert_eq!(command(&handle, json!({"op":"messages"})), json!([]));
    command(&handle, json!({"op":"start","prompt":"Explicit new run"}));
    command(&handle, json!({"op":"reply","reply":final_reply()}));
    command(&handle, json!({"op":"commit"}));
    assert_eq!(
        command(&handle, json!({"op":"messages"}))
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn terminal_stops_and_context_queries_preserve_committed_state() {
    let handle = EngineHandle::new(&config().to_string()).unwrap();
    command(&handle, json!({"op":"start","prompt":"Answer"}));
    command(&handle, json!({"op":"reply","reply":final_reply()}));
    command(&handle, json!({"op":"commit"}));
    let source = command(&handle, json!({"op":"capture_context","start":0,"end":2}));
    let plan = json!({"summary":{"id":"s","source":source,"content":"Known"},"retrieved":[]});
    command(&handle, json!({"op":"set_context_plan","plan":plan}));
    assert_eq!(command(&handle, json!({"op":"context_plan"})), plan);
    assert_eq!(
        command(&handle, json!({"op":"read_context","source":source}))
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        command(&handle, json!({"op":"search_context","query":"Answer"}))["hits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    command(&handle, json!({"op":"start","prompt":"Invalid final"}));
    let result = command(
        &handle,
        json!({"op":"reply","reply":{"content":"bad","usage":{"input_tokens":3,"output_tokens":2}}}),
    );
    assert_eq!(result["action"]["reason"], "invalid_structured_output");
    command(&handle, json!({"op":"commit"}));
    assert!(
        handle
            .dispatch(r#"{"op":"start","prompt":"Again"}"#)
            .is_err()
    );
    assert_eq!(
        command(&handle, json!({"op":"messages"}))[3]["content"],
        "bad"
    );
}

#[test]
fn handle_never_accepts_host_state_or_unknown_transition_fields() {
    let handle = EngineHandle::new(&config().to_string()).unwrap();
    assert!(
        handle
            .dispatch(r#"{"op":"start","prompt":"Answer","state":{}}"#)
            .is_err()
    );
    assert!(EngineHandle::new(r#"{"op":"tick","state":{}}"#).is_err());
    assert!(EngineHandle::new(r#"{"op":"create","model":"test","state":{}}"#).is_err());
    assert_eq!(command(&handle, json!({"op":"messages"})), json!([]));
}
