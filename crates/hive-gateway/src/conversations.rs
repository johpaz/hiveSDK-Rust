//! Conversation IDs are opaque; all history access checks the stored owner.
use crate::{
    Result, activity,
    config::{self, App},
};
use hivedb_core::ScanOptions;
use serde_json::{Value, json};

pub(crate) fn wire(row: &Value) -> Value {
    json!({"id":row["id"],"title":row["title"],"channel":row["channel"],"peerKind":row["peer_kind"],
        "messageCount":row["message_count"].as_u64().unwrap_or(0),"lastMessageAt":row["last_message_at"].as_u64().unwrap_or(0),"createdAt":row["created_at"].as_u64().unwrap_or(0)})
}
pub fn owned(app: &App, thread: &str, user: &str) -> Result<bool> {
    Ok(app
        .db
        .col_get("conversationThreads", thread)?
        .is_some_and(|row| {
            row.doc["user_id"] == user
                && row.doc["channel"] == "webchat"
                && row.doc["archived"] != true
        }))
}
/// Whether a turn may run in this conversation for this user: their web chat, or a
/// conversation of one of their channels. Only the web chat is readable through the web
/// history and conversation routes (`owned`); channel threads stay out of them.
pub fn runnable(app: &App, thread: &str, user: &str) -> Result<bool> {
    Ok(app
        .db
        .col_get("conversationThreads", thread)?
        .is_some_and(|row| {
            row.doc["user_id"] == user
                && matches!(
                    row.doc["channel"].as_str(),
                    Some("webchat" | "telegram" | "discord" | "slack" | "whatsapp" | "whatsapp_qr")
                )
                && row.doc["archived"] != true
        }))
}
pub fn visible_messages(app: &App, thread: &str, limit: usize) -> Result<Vec<Value>> {
    let mut messages: Vec<_> = app
        .db
        .col_scan(
            "conversations",
            &ScanOptions {
                prefix: Some(format!("{thread}:")),
                reverse: true,
                ..Default::default()
            },
        )?
        .into_iter()
        .filter(|row| {
            row.doc["thread_id"] == thread
                && matches!(row.doc["role"].as_str(), Some("user" | "assistant"))
                && !matches!(
                    row.doc["source"].as_str(),
                    Some(
                        "task_complete"
                            | "delegation_summary"
                            | "legacy_internal"
                            | "realtime_chat"
                    )
                )
        })
        .take(limit)
        .map(|row| {
            let mut doc = row.doc;
            doc["id"] = json!(row.id);
            doc
        })
        .collect();
    messages.reverse();
    Ok(messages)
}
pub fn route(
    app: &App,
    path: &str,
    query: &std::collections::HashMap<String, String>,
    user: &str,
) -> Result<Option<(u16, Value)>> {
    if !matches!(path, "/api/conversations" | "/api/chat/history") {
        return Ok(None);
    }
    let user = user.to_owned();
    if path == "/api/conversations" {
        let mut rows: Vec<_> = config::docs(&app.db, "conversationThreads")?
            .into_iter()
            .filter(|row| {
                row["user_id"] == user
                    && (row["archived"] != true
                        || query.get("includeArchived").is_some_and(|v| v == "true"))
                    && query
                        .get("channel")
                        .is_none_or(|channel| row["channel"] == channel.as_str())
            })
            .collect();
        rows.sort_by(|a, b| {
            b["last_message_at"]
                .as_u64()
                .unwrap_or(0)
                .cmp(&a["last_message_at"].as_u64().unwrap_or(0))
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });
        return Ok(Some((
            200,
            json!({"conversations":rows.iter().map(wire).collect::<Vec<_>>()}),
        )));
    }
    let Some(thread) = query.get("threadId").or_else(|| query.get("sessionId")) else {
        return Ok(Some((400, json!({"error":"threadId es obligatorio"}))));
    };
    if !owned(app, thread, &user)? {
        return Ok(Some((404, json!({"error":"Conversación desconocida"}))));
    }
    let limit = match query.get("limit") {
        None => 40,
        Some(value) => match value.parse::<usize>() {
            Ok(value) if (1..=200).contains(&value) => value,
            _ => {
                return Ok(Some((
                    400,
                    json!({"error":"limit debe estar entre 1 y 200"}),
                )));
            }
        },
    };
    let messages = visible_messages(app, thread, limit)?;
    let mut narrations: Vec<_> = config::docs(&app.db, "narrationEvents")?
        .into_iter()
        .filter(|row| {
            row["thread_id"] == thread.as_str()
                && (row["user_id"] == user
                    || row["user_id"].as_str().is_none_or(|id| id.is_empty()))
        })
        .collect();
    narrations.sort_by(|a, b| {
        a["created_at"]
            .as_u64()
            .cmp(&b["created_at"].as_u64())
            .then_with(|| a["revision"].as_u64().cmp(&b["revision"].as_u64()))
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let skip = narrations.len().saturating_sub(256);
    let narrations: Vec<_> = narrations
        .into_iter()
        .skip(skip)
        .map(|event| activity::narration_frame(&app.db, &event))
        .collect::<Result<_>>()?;
    let active = app
        .active
        .lock()
        .map_err(|_| "Turnos bloqueados")?
        .contains_key(thread);
    let continuation = if active {
        Err("Hay un turno en curso en esta conversación".into())
    } else {
        let cached = {
            app.sessions
                .lock()
                .map_err(|_| "Sesiones bloqueadas")?
                .get(thread)
                .cloned()
        };
        let session = match cached {
            Some(handle) => Ok(handle),
            None => app
                .selection_for_user(&user)
                .and_then(|selection| app.session(thread, &selection)),
        };
        session.and_then(|handle| {
            let frame: Value = serde_json::from_str(&handle.dispatch(r#"{"op":"frame"}"#)?)?;
            if !matches!(frame["action"]["type"].as_str(),Some("idle"|"complete")) {
                return Err("El turno se detuvo o quedó interrumpido. Revisa su actividad y crea una conversación nueva para continuar sin repetir operaciones".into());
            }
            Ok(())
        })
    };
    let error=continuation.as_ref().err().map(|error| {
        let text=error.to_string();
        if text.contains("SESSION_CONFIG_MISMATCH") {"Esta conversación conserva una configuración anterior. Puedes consultar su historial y crear una conversación nueva para continuar".to_owned()}
        else if text.contains("SESSION_INTERRUPTED") || text.contains("SESSION_OPERATION_UNCERTAIN") {"El turno quedó interrumpido y necesita revisión. Crea una conversación nueva para continuar sin repetir operaciones".to_owned()}
        else {text}
    });
    Ok(Some((
        200,
        json!({"threadId":thread,"messages":messages,"narrations":narrations,"active":active,
        "canContinue":continuation.is_ok(),"continuationError":error}),
    )))
}
