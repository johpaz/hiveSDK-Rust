//! Native HTTP client and dashboard collections, with bounded external responses.
use crate::{
    Result,
    config::{self, App},
    server,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
pub(crate) fn safe_url(raw: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return String::new();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.into()
}
pub(crate) fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}
pub(crate) fn blocking<T: Send + 'static>(
    future: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(future)
    })
    .join()
    .map_err(|_| "Falló el cliente HTTP nativo")?
}
pub(crate) async fn limited(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err("La respuesta supera el límite de tamaño".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > max {
            return Err("La respuesta supera el límite de tamaño".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
pub(crate) async fn http(payload: Value) -> Result<Value> {
    let raw = payload["url"].as_str().ok_or("Escribe la URL")?;
    let mut url = reqwest::Url::parse(raw)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Usa una URL HTTP/HTTPS sin credenciales incrustadas".into());
    }
    if let Some(query) = payload["query_params"].as_object() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value.as_str().unwrap_or_default());
        }
    }
    let method = payload["method"].as_str().unwrap_or("GET");
    if !matches!(
        method,
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    ) {
        return Err("Método HTTP no permitido".into());
    }
    let mut request = client()?.request(reqwest::Method::from_bytes(method.as_bytes())?, url);
    if let Some(headers) = payload["headers"].as_object() {
        for (key, value) in headers {
            if matches!(
                key.to_lowercase().as_str(),
                "host" | "content-length" | "transfer-encoding" | "connection"
            ) {
                return Err("Cabecera de transporte no editable".into());
            }
            request = request.header(key, value.as_str().ok_or("Cabecera inválida")?);
        }
    }
    if !matches!(method, "GET" | "HEAD")
        && let Some(body) = payload["body"].as_str()
    {
        request = request.body(body.to_owned());
    }
    let response = request.send().await?;
    let status = response.status();
    let headers: serde_json::Map<String, Value> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or_default())))
        .collect();
    let body = limited(response, 4 * 1024 * 1024).await?;
    Ok(
        json!({"ok":status.is_success(),"status":status.as_u16(),"statusText":status.canonical_reason().unwrap_or_default(),"headers":headers,"body":String::from_utf8_lossy(&body)}),
    )
}
pub(crate) fn route(
    app: &App,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
    user: &str,
) -> Result<Option<(u16, Value)>> {
    if path.starts_with("/api/cron") {
        return crate::scheduler::route(app, method, path, query, body, user);
    }
    if path == "/api/http-request" && method == "POST" {
        let payload = serde_json::from_slice(body)?;
        let value = blocking(http(payload))
            .unwrap_or_else(|e| json!({"ok":false,"error":format!("Falló la petición HTTP: {e}")}));
        return Ok(Some((200, value)));
    }
    if path == "/api/notes" && method == "GET" {
        let owned: std::collections::HashSet<_> = config::docs(&app.db, "conversationThreads")?
            .into_iter()
            .filter(|r| r["user_id"] == user)
            .filter_map(|r| r["id"].as_str().map(str::to_owned))
            .collect();
        let mut rows = config::docs(&app.db, "scratchpad")?
            .into_iter()
            .filter(|r| r["threadId"].as_str().is_some_and(|id| owned.contains(id)))
            .collect::<Vec<_>>();
        rows.sort_by_key(|r| std::cmp::Reverse(r["updatedAt"].as_u64().unwrap_or(0)));
        let notes=rows.into_iter().take(50).map(|r|json!({"id":r["id"],"thread_id":r["threadId"],"key":r["key"],"value":r["value"],"source":r["source"],"created_at":r["createdAt"].as_u64().unwrap_or(0)/1000})).collect::<Vec<_>>();
        return Ok(Some((200, json!({"notes":notes}))));
    }
    if path == "/api/a2ui/surfaces" && method == "GET" {
        let rows = config::docs(&app.db, "hive_rust_a2ui")?
            .into_iter()
            .filter(|r| r["user_id"] == user)
            .collect::<Vec<_>>();
        return Ok(Some((
            200,
            json!({"sessionId":format!("canvas:{user}"),"surfaces":rows}),
        )));
    }
    if let Some(id) = path.strip_prefix("/api/a2ui/surfaces/")
        && method == "DELETE"
    {
        let id = server::decode(id)?;
        let Some(row) = app.db.col_get("hive_rust_a2ui", &format!("{user}:{id}"))? else {
            return Ok(Some((404, json!({"error":"Superficie desconocida"}))));
        };
        if row.doc["user_id"] != user {
            return Ok(Some((404, json!({"error":"Superficie desconocida"}))));
        };
        app.db.col_batch(&[hivedb_core::ColOp::Delete {
            collection: "hive_rust_a2ui".into(),
            id: format!("{user}:{id}"),
        }])?;
        let _ = app
            .office_events
            .send(json!({"type":"a2ui:deleteSurface","userId":user,"data":{"surfaceId":id}}));
        return Ok(Some((200, json!({"success":true,"surfaceId":id}))));
    }
    if path == "/api/setup/verify-provider" && method == "POST" {
        let body: Value = serde_json::from_slice(body)?;
        let Some(provider) = body["provider"].as_str() else {
            return Ok(Some((400, json!({"error":"Falta proveedor"}))));
        };
        let Some(row) = app.db.col_get("providers", provider)? else {
            return Ok(Some((404, json!({"error":"Proveedor desconocido"}))));
        };
        let key = body["apiKey"].as_str().ok_or("Falta la clave")?.to_owned();
        let base = config::provider_endpoint(provider, row.doc["base_url"].as_str())?;
        let provider = provider.to_owned();
        let checked = blocking(async move {
            let (url, header, value) = if provider == "gemini" {
                (format!("{base}/models"), "x-goog-api-key", key.clone())
            } else {
                (
                    format!("{base}/models"),
                    "Authorization",
                    format!("Bearer {key}"),
                )
            };
            let mut request = client()?.get(url).header(header, value);
            if provider == "anthropic" {
                request = request.header("anthropic-version", "2023-06-01");
            }
            let response = request
                .send()
                .await
                .map_err(|_| "No se pudo contactar con el proveedor")?;
            if !response.status().is_success() {
                return Err(format!(
                    "El proveedor rechazó la verificación (HTTP {})",
                    response.status().as_u16()
                )
                .into());
            }
            Ok(json!({"ok":true,"valid":true}))
        });
        return Ok(Some(match checked {
            Ok(value) => (200, value),
            Err(e) => (400, json!({"ok":false,"valid":false,"error":e.to_string()})),
        }));
    }
    Ok(None)
}

/// Loads persisted UI surfaces whenever a native chat subscribes to the canvas.
pub(crate) fn surfaces(app: &Arc<App>, user: &str) -> Result<Vec<Value>> {
    Ok(config::docs(&app.db, "hive_rust_a2ui")?
        .into_iter()
        .filter(|row| row["user_id"] == user)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn desktop_api_client_rejects_unsafe_transports_and_bounds_responses() {
        for url in ["file:///tmp/private", "https://user:pass@example.test/path"] {
            assert!(http(json!({"url":url})).await.is_err());
        }
        assert!(
            http(json!({"url":"https://example.test","method":"TRACE"}))
                .await
                .is_err()
        );
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .body(vec![1u8, 2, 3])
                .unwrap(),
        );
        assert_eq!(limited(response, 3).await.unwrap(), vec![1, 2, 3]);
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .body(vec![1u8; 4])
                .unwrap(),
        );
        assert!(limited(response, 3).await.is_err());
        assert_eq!(
            safe_url("https://user:pass@example.test/api?token=secret"),
            "https://example.test/api"
        );
    }
}
