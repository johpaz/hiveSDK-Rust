//! User-scoped persisted interactive surfaces; compatible with Rune's A2UI adapter.
use crate::{
    Result,
    config::{self, App},
};
use hivedb_core::ColOp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
pub(crate) fn specs() -> Vec<Value> {
    [
        ("a2ui_create_surface","Crea una superficie del panel interactivo. Usa surfaceId estable y catálogo estándar A2UI. Después envía componentes completos con a2ui_update_components.",json!({"surfaceId":{"type":"string"},"catalogId":{"type":"string"},"theme":{"type":"object","properties":{"agentDisplayName":{"type":"string"},"primaryColor":{"type":"string"}}}}),vec!["surfaceId"]),
        ("a2ui_update_components","Actualiza la lista PLANA de componentes A2UI. Cada componente lleva id y component. Ejemplo: [{\"id\":\"root\",\"component\":\"Column\",\"children\":[\"label\"]},{\"id\":\"label\",\"component\":\"Text\",\"text\":\"Hola\"}]. Usa root como raíz. Los botones llevan action:{event:{name,context}} y child con el id de su texto.",json!({"surfaceId":{"type":"string"},"components":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}}}),vec!["surfaceId","components"]),
        ("a2ui_update_data_model","Reemplaza datos enlazados en una ruta JSON Pointer del panel. value puede contener JSON serializado como texto.",json!({"surfaceId":{"type":"string"},"path":{"type":"string"},"value":{"type":"string"}}),vec!["surfaceId","value"]),
        ("a2ui_delete_surface","Elimina una superficie de este usuario.",json!({"surfaceId":{"type":"string"}}),vec!["surfaceId"]),
    ].into_iter().map(|(name,description,properties,required)|json!({"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required}})).collect()
}
fn identity(args: &Value) -> Result<&str> {
    args["surfaceId"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
        })
        .ok_or("surfaceId inválido".into())
}
pub(crate) fn frames(row: &Value) -> Vec<Value> {
    let mut result =
        vec![json!({"type":"a2ui:createSurface","userId":row["user_id"],"data":row["create"]})];
    if row["components"].as_array().is_some_and(|c| !c.is_empty()) {
        result.push(json!({"type":"a2ui:updateComponents","userId":row["user_id"],"data":{"surfaceId":row["create"]["surfaceId"],"components":row["components"]}}));
    }
    result.push(json!({"type":"a2ui:updateDataModel","userId":row["user_id"],"data":{"surfaceId":row["create"]["surfaceId"],"path":"","value":row["data"]}}));
    result
}
pub(crate) fn execute(
    app: &App,
    user: &str,
    thread: &str,
    agent: &Value,
    name: &str,
    args: &Value,
    execution: &str,
) -> Result<Value> {
    let surface = identity(args)?;
    let key = format!("{user}:{surface}");
    let _guard = app.activity_lock.lock().map_err(|_| "Panel bloqueado")?;
    if app
        .db
        .col_get("tools", name)?
        .is_some_and(|row| row.doc["active"] == false || row.doc["enabled"] == false)
    {
        return Err("La herramienta de panel está desactivada en HiveDB".into());
    }
    let op = format!("{user}:{:x}", Sha256::digest(execution.as_bytes()));
    if let Some(previous) = app.db.col_get("hive_rust_a2ui_operations", &op)? {
        return Ok(previous.doc["result"].clone());
    }
    let current = app.db.col_get("hive_rust_a2ui", &key)?;
    let mut row = if name == "a2ui_create_surface" {
        json!({"id":key,"user_id":user,"thread_id":thread,"create":{"surfaceId":surface,"catalogId":args["catalogId"].as_str().unwrap_or("https://github.com/google/A2UI/blob/main/specification/0.8/json/standard_catalog_definition.json"),"theme":args["theme"].as_object().cloned().unwrap_or_default()},"components":[],"data":{}})
    } else {
        current
            .as_ref()
            .ok_or("Primero crea la superficie")?
            .doc
            .clone()
    };
    row["create"]["theme"]["agentDisplayName"] = json!(agent["name"].as_str().unwrap_or("Hive"));
    match name {
        "a2ui_create_surface" => {}
        "a2ui_update_components" => {
            let components = args["components"]
                .as_array()
                .filter(|c| !c.is_empty() && c.len() <= 300)
                .ok_or("Envía entre 1 y 300 componentes completos")?;
            for component in components {
                let id = config::string(component, "id")?;
                if id.is_empty() || id.len() > 128 {
                    return Err("Id de componente inválido".into());
                }
                if !(component["component"].is_string()
                    || component["component"]
                        .as_object()
                        .is_some_and(|o| o.len() == 1))
                {
                    return Err("Cada componente necesita su definición completa".into());
                }
                let rows = row["components"]
                    .as_array_mut()
                    .ok_or("Superficie inválida")?;
                if let Some(at) = rows.iter().position(|c| c["id"] == id) {
                    rows[at] = component.clone();
                } else {
                    rows.push(component.clone());
                }
            }
        }
        "a2ui_update_data_model" => {
            let path = args["path"].as_str().unwrap_or_default();
            let value = args["value"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| args["value"].clone());
            if path.is_empty() || path == "/" {
                row["data"] = value;
            } else {
                if !path.starts_with('/') || path.len() > 1024 {
                    return Err("Ruta de datos inválida".into());
                }
                let keys = path[1..]
                    .split('/')
                    .map(|s| s.replace("~1", "/").replace("~0", "~"))
                    .collect::<Vec<_>>();
                let mut target = &mut row["data"];
                for part in &keys[..keys.len() - 1] {
                    if !target.is_object() {
                        *target = json!({});
                    }
                    target = target
                        .as_object_mut()
                        .ok_or("Datos inválidos")?
                        .entry(part.clone())
                        .or_insert_with(|| json!({}));
                }
                if !target.is_object() {
                    *target = json!({});
                }
                target[keys.last().ok_or("Ruta vacía")?] = value;
            }
        }
        "a2ui_delete_surface" => {}
        _ => return Err("Herramienta de panel desconocida".into()),
    }
    if row.to_string().len() > 512 * 1024
        || row["components"].as_array().is_some_and(|c| c.len() > 300)
    {
        return Err("La superficie supera el tamaño máximo".into());
    }
    let result = json!({"ok":true,"surfaceId":surface});
    let mut ops = vec![ColOp::Put {
        collection: "hive_rust_a2ui_operations".into(),
        id: op.clone(),
        doc: json!({"id":op,"user_id":user,"result":result}),
        expected_version: Some(0),
    }];
    ops.push(if name == "a2ui_delete_surface" {
        ColOp::Delete {
            collection: "hive_rust_a2ui".into(),
            id: key,
        }
    } else {
        ColOp::Put {
            collection: "hive_rust_a2ui".into(),
            id: key,
            doc: row.clone(),
            expected_version: Some(current.map_or(0, |r| r.version)),
        }
    });
    app.db.col_batch(&ops)?;
    if name == "a2ui_delete_surface" {
        let _ = app
            .office_events
            .send(json!({"type":"a2ui:deleteSurface","userId":user,"data":{"surfaceId":surface}}));
    } else {
        for frame in frames(&row) {
            let _ = app.office_events.send(frame);
        }
    }
    Ok(result)
}
pub(crate) fn action(app: &App, user: &str, data: &Value) -> Result<(String, String)> {
    let surface = identity(data)?;
    let row = app
        .db
        .col_get("hive_rust_a2ui", &format!("{user}:{surface}"))?
        .ok_or("Superficie desconocida")?
        .doc;
    let source = config::string(data, "sourceComponentId")?;
    if !row["components"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|r| r["id"] == source))
    {
        return Err("Componente desconocido".into());
    }
    let name = config::string(data, "name")?;
    if name.len() > 128 || data.to_string().len() > 32768 {
        return Err("Acción demasiado grande".into());
    }
    Ok((
        config::string(&row, "thread_id")?.into(),
        format!(
            "Acción del usuario en el panel {surface}: {name}. Datos del formulario: {}",
            data["context"]
        ),
    ))
}
