//! Native meeting transcription over an authenticated socket and HiveDB history.
use crate::{
    Result,
    config::{self, App},
    integrations, management, server,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use hivedb_core::PutOptions;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
#[derive(Clone)]
pub(crate) struct Stt {
    model: String,
    wire_model: String,
    key: String,
    url: String,
}
fn stt(app: &App, requested: Option<&str>) -> Result<Stt> {
    let providers = config::docs(&app.db, "providers")?;
    let models = config::docs(&app.db, "models")?
        .into_iter()
        .filter(|m| {
            crate::model_catalog::supports(m, "stt")
                && m["enabled"] == true
                && providers.iter().any(|p| {
                    p["id"] == m["provider_id"] && p["enabled"] == true && p["active"] == true
                })
        })
        .collect::<Vec<_>>();
    let defaults = crate::model_defaults::get(app)?;
    let requested = requested.or_else(|| defaults["stt_model_id"].as_str());
    if requested.is_some_and(|id| !models.iter().any(|m| m["id"] == id)) {
        return Err(
            "El modelo STT seleccionado ya no está habilitado; revisa Providers > Voz".into(),
        );
    }
    let model = requested
        .and_then(|id| models.iter().find(|m| m["id"] == id))
        .or_else(|| models.iter().find(|m| m["active"] == true))
        .or_else(|| models.first())
        .ok_or("Configura un proveedor y un modelo de transcripción STT habilitado en Providers")?;
    let provider = config::string(model, "provider_id")?;
    if provider == "gemini" {
        return Err(
            "Las reuniones necesitan un modelo STT compatible con audio/transcriptions".into(),
        );
    }
    let key = config::secret(&app.db, &app.home, &format!("provider:{provider}:api_key"))?
        .filter(|key| !key.trim().is_empty())
        .ok_or("Falta la clave del proveedor STT")?;
    let p = providers
        .iter()
        .find(|p| p["id"] == provider)
        .ok_or("Proveedor STT desconocido")?;
    let base = p["base_url"].as_str().unwrap_or(match provider {
        "groq" => "https://api.groq.com/openai/v1",
        "openai" => "https://api.openai.com/v1",
        _ => "",
    });
    if base.is_empty() {
        return Err("Falta la URL del proveedor STT".into());
    }
    Ok(Stt {
        model: config::string(model, "id")?.into(),
        wire_model: config::wire_model(provider, config::string(model, "id")?).into(),
        key,
        url: format!("{}/audio/transcriptions", base.trim_end_matches('/')),
    })
}
pub(crate) fn allowed(app: &App, id: &str, user: &str) -> Result<bool> {
    Ok(app
        .db
        .col_get("meetingSessions", id)?
        .is_some_and(|r| management::owned(&r.doc, user)))
}
fn segments(app: &App, id: &str) -> Result<Vec<Value>> {
    let mut rows = app
        .db
        .col_scan(
            "meetingSegments",
            &hivedb_core::ScanOptions {
                prefix: Some(format!("{id}:")),
                ..Default::default()
            },
        )?
        .into_iter()
        .map(|r| r.doc)
        .collect::<Vec<_>>();
    rows.sort_by_key(|r| r["seq"].as_u64().unwrap_or(0));
    Ok(rows)
}
pub(crate) fn route(
    app: &App,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Option<(u16, Value)>> {
    if path != "/api/meetings" && !path.starts_with("/api/meetings/") {
        return Ok(None);
    }
    let user = management::owner(app)?;
    if path == "/api/meetings" {
        if method == "GET" {
            let mut sessions = config::docs(&app.db, "meetingSessions")?
                .into_iter()
                .filter(|r| management::owned(r, &user))
                .collect::<Vec<_>>();
            sessions.sort_by_key(|r| std::cmp::Reverse(r["started_at"].as_u64().unwrap_or(0)));
            sessions.truncate(50);
            for session in &mut sessions {
                session["segment_count"] =
                    json!(segments(app, config::string(session, "id")?)?.len());
            }
            return Ok(Some((200, json!({"ok":true,"sessions":sessions}))));
        }
        if method == "POST" {
            let body: Value = serde_json::from_slice(body)?;
            let selected = match stt(app, body["stt_model"].as_str()) {
                Ok(selected) => selected,
                Err(e) => return Ok(Some((400, json!({"ok":false,"error":e.to_string()})))),
            };
            let title = body["title"].as_str().unwrap_or("Reunión sin título");
            if title.len() > 1000 {
                return Ok(Some((
                    400,
                    json!({"ok":false,"error":"Título demasiado largo"}),
                )));
            };
            let id = uuid::Uuid::new_v4().to_string();
            let session = json!({"id":id,"user_id":user,"title":title,"status":"active","stt_model":selected.model,"started_at":server::now(),"stopped_at":null,"report_path":null});
            app.db.col_put(
                "meetingSessions",
                &id,
                &session,
                PutOptions {
                    expected_version: Some(0),
                },
            )?;
            return Ok(Some((200, json!({"ok":true,"session":session}))));
        }
    }
    let parts = path
        .trim_start_matches("/api/meetings/")
        .split('/')
        .collect::<Vec<_>>();
    let id = server::decode(parts[0])?;
    if !allowed(app, &id, &user)? {
        return Ok(Some((
            404,
            json!({"ok":false,"error":"Reunión desconocida"}),
        )));
    };
    let action = parts.get(1).copied().unwrap_or_default();
    if method == "GET" {
        return Ok(Some((
            200,
            json!({"ok":true,"session":app.db.col_get("meetingSessions",&id)?.map(|r|r.doc),"segments":segments(app,&id)?}),
        )));
    }
    if method == "POST" && action == "stop" {
        management::patch(
            app,
            "meetingSessions",
            &id,
            &json!({"status":"stopped","stopped_at":server::now()}),
            &[("status", "status"), ("stopped_at", "stopped_at")],
        )?;
        if let Some(token) = app
            .live_sessions
            .lock()
            .map_err(|_| "Sesiones bloqueadas")?
            .get(&format!("meeting:{id}"))
        {
            token.cancel();
        }
        return Ok(Some((200, json!({"ok":true}))));
    }
    if method == "POST" && action == "report" {
        let session = app
            .db
            .col_get("meetingSessions", &id)?
            .ok_or("Reunión desconocida")?
            .doc;
        let rows = segments(app, &id)?;
        if session["status"] == "active" {
            return Ok(Some((
                409,
                json!({"ok":false,"error":"Detén la reunión antes de exportar el informe"}),
            )));
        };
        if rows.is_empty() {
            return Ok(Some((
                400,
                json!({"ok":false,"error":"La reunión no tiene transcripciones para exportar"}),
            )));
        };
        let folder = app.home.join("workspace/meetings");
        std::fs::create_dir_all(&folder)?;
        let path = folder.join(format!("{:x}.md", Sha256::digest(id.as_bytes())));
        let mut text = format!(
            "# {}\n\nTranscripción de la reunión\n\n",
            session["title"].as_str().unwrap_or("Reunión")
        );
        for row in rows {
            text.push_str(&format!(
                "- {}: {}\n",
                row["speaker"].as_str().unwrap_or("Participante"),
                row["text"].as_str().unwrap_or_default()
            ));
        }
        std::fs::write(&path, text)?;
        management::patch(
            app,
            "meetingSessions",
            &id,
            &json!({"status":"report_ready","report_path":path}),
            &[("status", "status"), ("report_path", "report_path")],
        )?;
        return Ok(Some((200, json!({"ok":true,"report_path":path}))));
    }
    Ok(Some((
        405,
        json!({"ok":false,"error":"Método de reunión no permitido"}),
    )))
}
async fn transcribe(config: &Stt, audio: Vec<u8>) -> Result<String> {
    if audio.len() < 44 || !audio.starts_with(b"RIFF") || &audio[8..12] != b"WAVE" {
        return Err("Se espera audio WAV PCM".into());
    }
    let boundary = format!("hive-{}", uuid::Uuid::new_v4());
    let mut body=format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"meeting.wav\"\r\nContent-Type: audio/wav\r\n\r\n",config.wire_model).into_bytes();
    body.extend(audio);
    body.extend(format!("\r\n--{boundary}--\r\n").as_bytes());
    let response = integrations::client()?
        .post(&config.url)
        .bearer_auth(&config.key)
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .map_err(|_| "No se pudo contactar al proveedor STT")?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("El proveedor STT respondió HTTP {}", status.as_u16()).into());
    }
    let value: Value = serde_json::from_slice(&integrations::limited(response, 512 * 1024).await?)?;
    Ok(value["text"]
        .as_str()
        .ok_or("El proveedor STT no devolvió texto")?
        .trim()
        .to_owned())
}
struct Registered {
    app: Arc<App>,
    id: String,
}
impl Drop for Registered {
    fn drop(&mut self) {
        if let Ok(mut slots) = self.app.live_sessions.lock() {
            slots.remove(&format!("meeting:{}", self.id));
        }
    }
}
pub(crate) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    app: Arc<App>,
    user: String,
    id: String,
    socket: WebSocketStream<S>,
) -> Result<()> {
    serve_with(app, user, id, socket, |config, audio| async move {
        transcribe(&config, audio).await
    })
    .await
}
pub(crate) async fn serve_with<S, F, Fut>(
    app: Arc<App>,
    user: String,
    id: String,
    mut socket: WebSocketStream<S>,
    mut transcribe_audio: F,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(Stt, Vec<u8>) -> Fut + Send,
    Fut: std::future::Future<Output = Result<String>> + Send,
{
    let mut owns_session = false;
    let result:Result<()>=async {
        if !allowed(&app,&id,&user)? {return Err("Reunión desconocida".into());}
        let session=app.db.col_get("meetingSessions",&id)?.ok_or("Reunión desconocida")?.doc;if session["status"]!="active" {return Err("La reunión ya está cerrada".into());}
        let cancel=app.shutdown.child_token();
        {let mut slots=app.live_sessions.lock().map_err(|_|"Sesiones bloqueadas")?;if slots.contains_key(&format!("meeting:{id}")){return Err("La reunión ya tiene una captura conectada".into());}slots.insert(format!("meeting:{id}"),cancel.clone());}
        owns_session=true;
        let _registered=Registered{app:app.clone(),id:id.clone()};
        let config=stt(&app,session["stt_model"].as_str())?;
        loop {
            let frame=tokio::select!{_=cancel.cancelled()=>break,frame=socket.next()=>frame};
            let value:Value=match frame {Some(Ok(message @ (Message::Text(_)|Message::Binary(_))))=>serde_json::from_slice(&message.into_data())?,Some(Ok(Message::Ping(data)))=>{socket.send(Message::Pong(data)).await?;continue;},Some(Ok(Message::Pong(_)))=>continue,_=>break};
            match value["type"].as_str() {
                Some("ping")=>socket.send(Message::text(json!({"type":"pong"}).to_string())).await?,
                Some("meeting_stop")=>break,
                Some("audio_chunk")=>{
                    let encoded=value["audio"].as_str().ok_or("Falta audio de la reunión")?;if encoded.len()>700_000 || value["mime_type"]!="audio/wav" {return Err("Fragmento de audio inválido o demasiado grande".into());}
                    let audio=STANDARD.decode(encoded).map_err(|_|"Audio base64 inválido")?;
                    if audio.len()<44 || !audio.starts_with(b"RIFF") || &audio[8..12]!=b"WAVE" {return Err("Se espera audio WAV PCM".into());}
                    let text=tokio::select!{_=cancel.cancelled()=>break,text=transcribe_audio(config.clone(),audio)=>text?};
                    if text.is_empty(){continue;}
                    let seq=segments(&app,&id)?.iter().filter_map(|r|r["seq"].as_u64()).max().map_or(0,|seq|seq+1);let key=format!("{id}:{seq:020}");
                    app.db.col_put("meetingSegments",&key,&json!({"id":key,"session_id":id,"seq":seq,"speaker":value["speaker"],"text":text,"created_at":server::now()}),PutOptions{expected_version:Some(0)})?;
                    socket.send(Message::text(json!({"type":"transcript_segment","seq":seq,"speaker":value["speaker"],"text":text}).to_string())).await?;
                }
                _=>return Err("Frame de reunión no compatible".into()),
            }
        }
        Ok(())
    }.await;
    if let Err(error) = result {
        let _ = socket
            .send(Message::text(
                json!({"type":"error","error":error.to_string()}).to_string(),
            ))
            .await;
    }
    if owns_session && allowed(&app, &id, &user)? {
        management::patch(
            &app,
            "meetingSessions",
            &id,
            &json!({"status":"stopped","stopped_at":server::now()}),
            &[("status", "status"), ("stopped_at", "stopped_at")],
        )?;
    }
    let _ = socket
        .send(Message::text(
            json!({"type":"meeting_stopped","segment_count":segments(&app,&id)?.len()}).to_string(),
        ))
        .await;
    let _ = socket.close(None).await;
    Ok(())
}
