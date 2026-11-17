//! The native Ollama adapter against a real Ollama server. Set `HIVE_TEST_OLLAMA_MODEL` to a *local*
//! model that supports tools (and optionally `HIVE_TEST_OLLAMA_URL`, default
//! `http://127.0.0.1:11434/api`). Without the variable it does nothing; use a model that runs on the
//! machine, not a `:cloud` one, which would send the prompts to a remote service.
use hive_agent_core::ModelRequest;
use hive_agent_core::provider::{ModelEvent, ollama::Ollama};
use serde_json::json;
use std::sync::Arc;

async fn ask(
    provider: &Arc<Ollama>,
    request: serde_json::Value,
    stream: bool,
) -> (Vec<ModelEvent>, hive_agent_core::ModelReply) {
    let request: ModelRequest = serde_json::from_value(request).unwrap();
    let events = provider.start(request, stream);
    let mut out = vec![];
    while let Some(event) = events.next().await.unwrap() {
        out.push(event);
    }
    let reply = out
        .iter()
        .find_map(|e| {
            if let ModelEvent::Reply { reply } = e {
                Some(reply.clone())
            } else {
                None
            }
        })
        .expect("a reply");
    (out, reply)
}

#[tokio::test]
async fn a_real_ollama_answers_streams_and_calls_a_tool() {
    let Ok(model) = std::env::var("HIVE_TEST_OLLAMA_MODEL") else {
        eprintln!("HIVE_TEST_OLLAMA_MODEL is not set: skipped");
        return;
    };
    assert!(!model.contains("cloud"), "use a local model");
    let base = std::env::var("HIVE_TEST_OLLAMA_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11434/api".into());
    let provider = Arc::new(
        Ollama::from_json(
            &json!({"base_url":base,"timeout_ms":240000,"num_ctx":4096,"think":false}).to_string(),
        )
        .unwrap(),
    );

    // Plain answer, with usage.
    let (_, reply) = ask(&provider, json!({"model":model,"messages":[{"role":"user","content":"Responde únicamente con la palabra: listo"}],"tools":[],"max_output_tokens":64}), false).await;
    assert!(
        reply
            .content
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains("listo")),
        "{reply:?}"
    );
    let usage = reply.usage.expect("Ollama reports token counts");
    assert!(usage.input_tokens > 0 && usage.output_tokens > 0);

    // The same, streamed: deltas add up to the reply.
    let (events, reply) = ask(&provider, json!({"model":model,"messages":[{"role":"user","content":"Cuenta del 1 al 5 separados por comas, sin nada más."}],"tools":[],"max_output_tokens":64}), true).await;
    let streamed: String = events
        .iter()
        .filter_map(|e| {
            if let ModelEvent::TextDelta { text } = e {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(Some(streamed.as_str()), reply.content.as_deref());
    assert!(streamed.contains('3'), "{streamed}");

    // A tool call, and then the result is understood.
    let tool = json!({"name":"clima","description":"Devuelve la temperatura actual de una ciudad","parameters":{"type":"object","properties":{"ciudad":{"type":"string"}},"required":["ciudad"]}});
    let (_, reply) = ask(&provider, json!({"model":model,"messages":[{"role":"user","content":"¿Qué temperatura hace ahora en Bogotá? Usa la herramienta."}],"tools":[tool],"max_output_tokens":256}), false).await;
    assert_eq!(reply.tool_calls.len(), 1, "{reply:?}");
    let call = &reply.tool_calls[0];
    assert_eq!(call.name, "clima");
    assert!(
        call.arguments["ciudad"]
            .as_str()
            .is_some_and(|c| c.to_lowercase().contains("bogot")),
        "{:?}",
        call.arguments
    );
    let history = json!([
        {"role":"user","content":"¿Qué temperatura hace ahora en Bogotá? Usa la herramienta."},
        {"role":"assistant","content":null,"tool_calls":[{"id":call.id,"name":call.name,"arguments":call.arguments}]},
        {"role":"tool","content":"{\"temperatura_c\":17}","tool_call_id":call.id}]);
    let (_, reply) = ask(
        &provider,
        json!({"model":model,"messages":history,"tools":[tool],"max_output_tokens":256}),
        false,
    )
    .await;
    assert!(
        reply.content.as_deref().is_some_and(|t| t.contains("17")),
        "{reply:?}"
    );
}
