//! Canonical HiveDB settings. Reads redact secrets; edits use versioned writes.
use crate::{
    Result,
    config::{self, App},
    server,
};
use hivedb_core::{ColOp, PutOptions};
use serde_json::{Value, json};
type Reply = Option<(u16, Value)>;
fn ok(value: Value) -> Result<Reply> {
    Ok(Some((200, value)))
}
fn fail(status: u16, message: &str) -> Result<Reply> {
    Ok(Some((status, json!({"ok":false,"error":message}))))
}
pub(crate) fn owner(app: &App) -> Result<String> {
    Ok(config::string(
        config::docs(&app.db, "users")?
            .first()
            .ok_or("Sin usuario")?,
        "id",
    )?
    .into())
}
pub(crate) fn owned(row: &Value, user: &str) -> bool {
    row["user_id"]
        .as_str()
        .is_none_or(|id| id.is_empty() || id == user)
}
pub(crate) fn patch(
    app: &App,
    col: &str,
    id: &str,
    body: &Value,
    mapping: &[(&str, &str)],
) -> Result<bool> {
    Ok(hive_agent_core::services::patch_fields(
        &app.db, col, id, body, mapping,
    )?)
}
fn parse(body: &[u8]) -> Result<Value> {
    if body.is_empty() {
        Ok(json!({}))
    } else {
        Ok(serde_json::from_slice(body)?)
    }
}
pub(crate) fn route(
    app: &App,
    method: &str,
    path: &str,
    raw_query: &str,
    body: &[u8],
) -> Result<Reply> {
    let supported = matches!(
        path,
        "/api/providers"
            | "/api/channels"
            | "/api/tools"
            | "/api/skills"
            | "/api/mcp/servers"
            | "/api/ethics"
            | "/api/ethics/activate"
            | "/api/users"
            | "/api/user/settings"
            | "/api/agents"
    ) || [
        "/api/providers/",
        "/api/models/",
        "/api/agents/",
        "/api/channels/",
        "/api/tools/",
        "/api/skills/",
    ]
    .iter()
    .any(|p| path.starts_with(p));
    if !supported || path == "/api/agents/proposals" {
        return Ok(None);
    }
    if method == "POST" && path.starts_with("/api/providers/") && path.ends_with("/sync-models") {
        let id = server::decode(
            path.trim_start_matches("/api/providers/")
                .trim_end_matches("/sync-models"),
        )?;
        return Ok(Some(match crate::model_catalog::sync(app, &id) {
            Ok(value) => (200, value),
            Err(e) => (400, json!({"ok":false,"error":e.to_string()})),
        }));
    }
    let _guard = app
        .activity_lock
        .lock()
        .map_err(|_| "Configuración bloqueada")?;
    let user = owner(app)?;
    let query = server::query(raw_query)?;
    let body = parse(body)?;
    if method == "GET" {
        match path {
            "/api/providers" => {
                let mut models = config::docs(&app.db, "models")?;
                crate::model_defaults::annotate(app, &mut models)?;
                let mut providers = vec![];
                for p in config::docs(&app.db, "providers")? {
                    let id = config::string(&p, "id")?;
                    let has_key = app
                        .db
                        .col_get("secrets", &format!("provider:{id}:api_key"))?
                        .is_some();
                    providers.push(json!({"id":id,"name":p["name"].as_str().unwrap_or(id),"base_url":p["base_url"].as_str().map(crate::integrations::safe_url),"category":p["category"].as_str().unwrap_or("llm"),"enabled":p["enabled"]==true,"active":p["active"]==true,"has_api_key":has_key,"masked_api_key":if has_key {Some("••••••••")}else{None},"models":models.iter().filter(|m|m["provider_id"]==id).cloned().collect::<Vec<_>>()}));
                }
                return ok(json!({"providers":providers}));
            }
            "/api/agents" => return ok(json!({"agents":crate::activity::agents(&app.db,&user)?})),
            "/api/channels" => {
                let mut rows = vec![];
                for channel in config::docs(&app.db, "channels")?
                    .into_iter()
                    .filter(|channel| owned(channel, &user))
                {
                    let id = config::string(&channel, "id")?;
                    let raw = channel["config"]
                        .as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| channel["config"].clone());
                    let configured = raw.as_object().is_some_and(|obj| !obj.is_empty())
                        || app
                            .db
                            .col_get("secrets", &format!("channel:{id}:config"))?
                            .is_some();
                    rows.push(json!({"id":id,"type":channel["type"],"enabled":channel["enabled"]==true,"active":channel["active"]==true,"status":"disconnected","isConfigured":configured}));
                }
                return ok(json!({"channels":rows}));
            }
            "/api/tools" => {
                return ok(
                    json!({"tools":config::docs(&app.db,"tools")?.iter().map(|row|json!({"id":row["id"],"name":row["name"],"description":row["description"],"category":row["category"],"enabled":row["enabled"],"active":row["active"],"runtime_available":row["id"]=="task_delegate" || row["id"].as_str().is_some_and(|id|crate::panel::specs().iter().any(|tool|tool["name"]==id))})).collect::<Vec<_>>()}),
                );
            }
            "/api/skills" => {
                let mut rows = config::docs(&app.db, "skills")?;
                for row in &mut rows {
                    for key in ["tools", "triggers"] {
                        if let Some(list) = row[key].as_array() {
                            row[key] = json!(
                                list.iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(",")
                            );
                        }
                    }
                }
                return ok(json!({"skills":rows}));
            }
            "/api/ethics" => return ok(json!({"ethics":config::docs(&app.db,"ethics")?})),
            "/api/mcp/servers" => {
                let rows=config::docs(&app.db,"mcpServers")?.into_iter().map(|r|json!({"id":r["id"],"name":r["name"],"enabled":r["enabled"]==true,"status":"disconnected","tools_count":0,"tools":[],"config":{"transport":r["transport"],"url":r["url"].as_str().map(crate::integrations::safe_url)}})).collect::<Vec<_>>();
                return ok(json!(rows));
            }
            "/api/users" => {
                let rows=config::docs(&app.db,"users")?.into_iter().filter(|r|r["id"]==user).map(|r|json!({"id":r["id"],"name":r["name"],"email":r["email"],"language":r["language"],"timezone":r["timezone"],"occupation":r["occupation"],"notes":r["notes"],"preferred_cron_channel":r["preferred_cron_channel"]})).collect::<Vec<_>>();
                return ok(json!({"users":rows}));
            }
            _ => {}
        }
    }
    if path == "/api/user/settings" && method == "PATCH" {
        if query.get("userId").is_some_and(|id| id != &user) {
            return fail(404, "Usuario desconocido");
        }
        patch(
            app,
            "users",
            &user,
            &body,
            &[
                ("name", "name"),
                ("email", "email"),
                ("language", "language"),
                ("timezone", "timezone"),
                ("occupation", "occupation"),
                ("notes", "notes"),
                ("preferred_cron_channel", "preferred_cron_channel"),
            ],
        )?;
        return ok(json!({"ok":true}));
    }
    if path == "/api/ethics/activate" && method == "POST" {
        let Some(id) = body["ethicsId"].as_str() else {
            return fail(400, "Falta ethicsId");
        };
        if !patch(
            app,
            "ethics",
            id,
            &json!({"active":body["active"],"enabled":body["active"]}),
            &[("active", "active"), ("enabled", "enabled")],
        )? {
            return fail(404, "Protocolo desconocido");
        }
        return ok(json!({"success":true}));
    }
    if path == "/api/agents" && method == "POST" {
        let Some(name) = body["name"].as_str().filter(|s| !s.trim().is_empty()) else {
            return fail(400, "El agente necesita un nombre");
        };
        match (body["providerId"].as_str(), body["modelId"].as_str()) {
            (Some(provider), Some(model)) => {
                if !app.db.col_get("models", model)?.is_some_and(|row| {
                    row.doc["provider_id"] == provider
                        && row.doc["enabled"] == true
                        && row.doc["model_type"]
                            .as_str()
                            .is_none_or(|kind| kind == "llm")
                }) {
                    return fail(
                        400,
                        "Selecciona un modelo de lenguaje habilitado para el agente",
                    );
                }
            }
            (None, None) => {}
            _ => return fail(400, "Selecciona proveedor y modelo de lenguaje"),
        }
        let id = uuid::Uuid::new_v4().to_string();
        let coordinator = config::docs(&app.db, "agents")?
            .iter()
            .any(|a| owned(a, &user) && a["role"] == "coordinator");
        let doc = json!({"id":id,"user_id":user,"name":name.trim(),"description":body["description"].as_str().unwrap_or(""),"system_prompt":body["systemPrompt"].as_str().unwrap_or(""),"tone":"friendly","role":if coordinator{"worker"}else{"coordinator"},"enabled":true,"provider_id":body["providerId"],"model_id":body["modelId"],"workspace":body["workspace"],"max_iterations":10,"created_at":server::now()});
        app.db.col_put(
            "agents",
            &id,
            &doc,
            PutOptions {
                expected_version: Some(0),
            },
        )?;
        return ok(json!({"ok":true,"agentId":id,"agent":doc}));
    }
    if path == "/api/channels" && method == "POST" {
        let Some(kind) = body["type"].as_str().filter(|s| {
            matches!(
                *s,
                "telegram" | "discord" | "whatsapp" | "slack" | "webchat"
            )
        }) else {
            return fail(400, "Tipo de canal inválido");
        };
        let id = uuid::Uuid::new_v4().to_string();
        let secret = format!("channel:{id}:config");
        config::store_secret(app, &secret, &body["config"].to_string())?;
        app.db.col_put("channels",&id,&json!({"id":id,"type":kind,"user_id":user,"active":true,"enabled":true,"config":"{}","created_at":server::now()}),PutOptions{expected_version:Some(0)})?;
        return ok(json!({"ok":true,"id":id,"status":"disconnected"}));
    }
    let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    if parts.len() >= 3 && parts[0] == "api" {
        let id = server::decode(parts[2])?;
        let action = parts.get(3).copied().unwrap_or("");
        let col = parts[1];
        if col == "agents" && method == "PUT" {
            let Some(row) = app.db.col_get("agents", &id)? else {
                return fail(404, "Agente desconocido");
            };
            if !owned(&row.doc, &user) {
                return fail(404, "Agente desconocido");
            }
            if body
                .get("maxIterations")
                .is_some_and(|n| n.as_u64().is_none_or(|n| !(1..=200).contains(&n)))
            {
                return fail(400, "Iteraciones entre 1 y 200");
            }
            if body
                .get("role")
                .is_some_and(|role| !matches!(role.as_str(), Some("coordinator" | "worker")))
            {
                return fail(400, "Rol inválido");
            }
            patch(
                app,
                "agents",
                &id,
                &body,
                &[
                    ("name", "name"),
                    ("description", "description"),
                    ("systemPrompt", "system_prompt"),
                    ("tone", "tone"),
                    ("enabled", "enabled"),
                    ("role", "role"),
                    ("maxIterations", "max_iterations"),
                    ("providerId", "provider_id"),
                    ("modelId", "model_id"),
                ],
            )?;
            return ok(json!({"ok":true}));
        }
        if col == "providers" && matches!(method, "PUT" | "POST") {
            if app.db.col_get("providers", &id)?.is_none() {
                return fail(404, "Proveedor desconocido");
            }
            if method == "PUT" {
                if let Some(key) = body["apiKey"].as_str().filter(|s| !s.trim().is_empty()) {
                    config::store_secret(app, &format!("provider:{id}:api_key"), key.trim())?;
                }
                if body["clearApiKey"] == true {
                    app.db.col_batch(&[ColOp::Delete {
                        collection: "secrets".into(),
                        id: format!("provider:{id}:api_key"),
                    }])?;
                }
                patch(
                    app,
                    "providers",
                    &id,
                    &body,
                    &[
                        ("name", "name"),
                        ("base_url", "base_url"),
                        ("baseUrl", "base_url"),
                        ("enabled", "enabled"),
                        ("active", "active"),
                    ],
                )?;
                return ok(json!({"ok":true}));
            }
            if action == "toggle" {
                let Some(active) = body["active"].as_bool() else {
                    return fail(400, "Falta active");
                };
                patch(
                    app,
                    "providers",
                    &id,
                    &json!({"active":active,"enabled":active}),
                    &[("active", "active"), ("enabled", "enabled")],
                )?;
                return ok(json!({"ok":true,"active":active}));
            }
        }
        if col == "models" && action == "default" && method == "POST" {
            let purpose = body["purpose"].as_str().unwrap_or_default();
            return Ok(Some(match crate::model_defaults::set(app, &id, purpose) {
                Ok(()) => (200, json!({"ok":true})),
                Err(e) => (400, json!({"ok":false,"error":e.to_string()})),
            }));
        }
        if action == "toggle"
            && method == "POST"
            && matches!(col, "models" | "tools" | "skills" | "channels")
        {
            let Some(active) = body["active"].as_bool() else {
                return fail(400, "Falta active");
            };
            let Some(row) = app.db.col_get(col, &id)? else {
                return fail(404, "Registro desconocido");
            };
            if !owned(&row.doc, &user) {
                return fail(404, "Registro desconocido");
            }
            if col == "models"
                && active
                && row.doc["model_type"] == "realtime"
                && row.doc["provider_id"] == "gemini"
            {
                let mut ops = vec![];
                for stored in app.db.col_scan("models", &Default::default())? {
                    if stored.doc["provider_id"] == "gemini"
                        && stored.doc["model_type"] == "realtime"
                    {
                        let mut doc = stored.doc;
                        doc["active"] = json!(doc["id"] == id);
                        if doc["id"] == id {
                            doc["enabled"] = json!(true);
                        }
                        ops.push(ColOp::Put {
                            collection: "models".into(),
                            id: config::string(&doc, "id")?.into(),
                            doc,
                            expected_version: Some(stored.version),
                        });
                    }
                }
                app.db.col_batch(&ops)?;
            } else {
                patch(
                    app,
                    col,
                    &id,
                    &json!({"active":active,"enabled":active}),
                    &[("active", "active"), ("enabled", "enabled")],
                )?;
            }
            return ok(json!({"ok":true,"active":active}));
        }
        if col == "channels" && action == "reconnect" && method == "POST" {
            if app.db.col_get("channels", &id)?.is_none() {
                return fail(404, "Canal desconocido");
            };
            if body["config"]
                .as_object()
                .is_some_and(|config| !config.is_empty())
            {
                config::store_secret(
                    app,
                    &format!("channel:{id}:config"),
                    &body["config"].to_string(),
                )?;
            }
            return fail(
                409,
                "Configuración guardada. El adaptador de conexión de este canal aún no está disponible en Rust",
            );
        }
        if col == "channels" && parts.last() == Some(&"status") && method == "GET" {
            let concrete = server::decode(parts.get(3).copied().unwrap_or_default())?;
            if !app
                .db
                .col_get("channels", &concrete)?
                .is_some_and(|row| owned(&row.doc, &user))
            {
                return fail(404, "Canal desconocido");
            }
            return ok(
                json!({"status":"disconnected","error":"Adaptador de canal pendiente en Rust"}),
            );
        }
    }
    Ok(None)
}
