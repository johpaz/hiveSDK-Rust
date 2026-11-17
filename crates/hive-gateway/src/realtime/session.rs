use super::LiveConfig;
use crate::{
    Result, activity,
    config::{self, App},
    server,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use hive_agent_core::{Usage, provider::CancellationToken};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};

const MAX_MEDIA: usize = 768 * 1024;

/// The base64 JPEG of a `video` message, or why it is refused.
fn video_frame(value: &Value) -> std::result::Result<&str, &'static str> {
    let data = value["data"].as_str().ok_or("Fotograma sin datos")?;
    if data.len() > MAX_MEDIA * 4 / 3 + 8 {
        return Err("Fotograma demasiado grande");
    }
    let decoded = STANDARD.decode(data).map_err(|_| "JPEG base64 inválido")?;
    if !decoded.starts_with(&[0xff, 0xd8])
        || decoded.len() > MAX_MEDIA
        || value["mimeType"]
            .as_str()
            .is_some_and(|mime| mime != "image/jpeg")
    {
        return Err("HiveLive solo admite fotogramas JPEG");
    }
    if value["source"]
        .as_str()
        .is_some_and(|source| !matches!(source, "camera" | "screen"))
    {
        return Err("Fuente visual inválida");
    }
    Ok(data)
}
const MAX_TEXT: usize = 16_000;
#[derive(PartialEq, Debug)]
pub(crate) enum End {
    Closed,
    Resume,
}
struct Turn {
    user: String,
    assistant: String,
    usage: Option<Usage>,
    delegated: bool,
    audio: bool,
    finished: Option<Instant>,
}
impl Turn {
    fn empty() -> Self {
        Self {
            user: String::new(),
            assistant: String::new(),
            usage: None,
            delegated: false,
            audio: false,
            finished: None,
        }
    }
}
struct Cached {
    fingerprint: [u8; 32],
    response: Value,
}
pub(crate) struct LiveSession {
    pub app: Arc<App>,
    pub config: LiveConfig,
    call: String,
    turn: u64,
    current: Turn,
    total_in: u64,
    total_out: u64,
    pub resume: Option<String>,
    generating: Option<Instant>,
    suppress: bool,
    synthetic: bool,
    idle: Instant,
    speeches: VecDeque<String>,
    speak_at: Option<Instant>,
    calls: HashMap<String, Cached>,
    cancelled: HashSet<String>,
    seen: HashSet<String>,
    events: tokio::sync::broadcast::Receiver<Value>,
    pub delivery: mpsc::Sender<Value>,
    work: mpsc::Receiver<Value>,
}
impl LiveSession {
    pub fn new(app: Arc<App>, config: LiveConfig) -> Self {
        let events = app.office_events.subscribe();
        let (delivery, work) = mpsc::channel(64);
        Self {
            app,
            config,
            call: uuid::Uuid::new_v4().to_string(),
            turn: 0,
            current: Turn::empty(),
            total_in: 0,
            total_out: 0,
            resume: None,
            generating: None,
            suppress: false,
            synthetic: false,
            idle: Instant::now(),
            speeches: VecDeque::new(),
            speak_at: None,
            calls: HashMap::new(),
            cancelled: HashSet::new(),
            seen: HashSet::new(),
            events,
            delivery,
            work,
        }
    }
    pub async fn setup<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        remote: &mut WebSocketStream<S>,
    ) -> Result<()> {
        // A consumed handle cannot authorize a second reconnect without a fresh update.
        let resume = self.resume.take();
        send(
            remote,
            Message::text(self.config.setup(resume.as_deref()).to_string()),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match remote.next().await {
                    Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => {
                        let value = decode_provider_message(message)?;
                        if value.get("error").is_some() {
                            return Err(provider_error(&self.config, &value));
                        }
                        if value.get("setupComplete").is_some() {
                            return Ok(());
                        }
                        return Err("Gemini Live respondió antes de confirmar setupComplete".into());
                    }
                    Some(Ok(Message::Ping(data))) => send(remote, Message::Pong(data)).await?,
                    Some(Ok(Message::Pong(_))) => {},
                    Some(Ok(Message::Close(frame))) => {
                        let detail = frame.map_or_else(
                            || "sin código ni motivo".to_owned(),
                            |frame| format!("cierre {}: {}", u16::from(frame.code), self.config.redact(&frame.reason)),
                        );
                        return Err(format!("Gemini Live rechazó el inicio de {} ({detail})", self.config.model).into());
                    }
                    Some(Err(error)) => return Err(format!(
                        "Gemini Live: error de transporte al iniciar {}: {}",
                        self.config.model, self.config.redact(&error.to_string())
                    ).into()),
                    None => return Err(format!("Gemini Live cerró antes de confirmar la sesión de {} (sin frame de cierre)", self.config.model).into()),
                    Some(Ok(_)) => return Err("Gemini Live envió un frame inesperado durante el inicio".into()),
                }
            }
        })
        .await
        .map_err(|_| "Gemini Live no confirmó la configuración a tiempo")??;
        if resume.is_none() && !self.config.history.is_empty() {
            send(
                remote,
                Message::text(
                    json!({"clientContent":{"turns":self.config.history,"turnComplete":false}})
                        .to_string(),
                ),
            )
            .await?;
        }
        Ok(())
    }
    pub async fn ready<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        client: &mut WebSocketStream<S>,
    ) -> Result<()> {
        send(client,Message::text(json!({"type":"pricing","inputPer1M":self.config.input_price,"outputPer1M":self.config.output_price}).to_string())).await?;
        send(client,Message::text(json!({"type":"ready","model":self.config.model,"voice":self.config.voice,"threadId":self.config.thread}).to_string())).await
    }
    async fn finish_turn(&mut self) -> Result<()> {
        let previous = std::mem::replace(&mut self.current, Turn::empty());
        if previous.finished.is_none() {
            self.current = previous;
            return Ok(());
        }
        if previous.user.is_empty()
            && previous.assistant.is_empty()
            && !previous.delegated
            && !previous.audio
        {
            return Ok(());
        }
        let sequence = self.turn;
        self.turn += 1;
        if !previous.delegated {
            for (role, content) in [("user", previous.user), ("assistant", previous.assistant)] {
                if !content.trim().is_empty() {
                    server::record_tagged(
                        self.app.clone(),
                        self.config.thread.clone(),
                        format!("live:{}:{sequence}:{role}", self.call),
                        role,
                        content,
                        "realtime_chat",
                    )
                    .await?;
                }
            }
        }
        if let Some(usage) = previous.usage {
            let app = self.app.clone();
            let model = self.config.model.clone();
            let id = format!("live:{}:{sequence}", self.call);
            let measured = usage.clone();
            tokio::task::spawn_blocking(move || {
                crate::dashboard::record_usage(
                    &app,
                    &id,
                    "gemini",
                    &model,
                    Some(&measured),
                    server::now(),
                )
            })
            .await??;
            self.total_in = self.total_in.saturating_add(usage.input_tokens);
            self.total_out = self.total_out.saturating_add(usage.output_tokens);
        }
        Ok(())
    }
    fn queue_speech(&mut self, text: String, urgent: bool) {
        self.speeches.push_back(text);
        while self.speeches.len() > 4 {
            self.speeches.pop_front();
        }
        self.speak_at =
            Some(Instant::now() + Duration::from_millis(if urgent { 250 } else { 1500 }));
    }
    fn narration(&mut self, event: Value) -> Option<Value> {
        if event["user_id"] != self.config.user || event["thread_id"] != self.config.thread {
            return None;
        }
        let id = event["id"].as_str()?.to_owned();
        if !self.seen.insert(id) {
            return None;
        }
        if self.seen.len() > 8192 {
            self.seen.clear();
        }
        let label = event["label"].as_str().unwrap_or_default();
        let kind = event["kind"].as_str().unwrap_or_default();
        if matches!(kind, "delegated" | "failed") {
            self.queue_speech(label.chars().take(500).collect(), kind == "failed");
        }
        Some(
            json!({"type":"narration","kind":kind,"status":event["status"],"label":label,
            "detail":event["detail"],"agent":event["agent_name"],"at":event["created_at"]}),
        )
    }
    fn tool<F>(&mut self, call: &Value, start: &mut F) -> Result<Value>
    where
        F: FnMut(String, mpsc::Sender<Value>) -> Result<String>,
    {
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or("Gemini Live envió una herramienta sin id válido")?;
        let name = call["name"]
            .as_str()
            .ok_or("Herramienta de voz sin nombre")?;
        let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
        if !args.is_object() {
            return Err("Argumentos de herramienta de voz inválidos".into());
        }
        let fingerprint: [u8; 32] = Sha256::digest(format!("{name}\0{args}")).into();
        if let Some(cached) = self.calls.get(id) {
            if cached.fingerprint != fingerprint {
                return Err("Gemini Live reutilizó un id de herramienta con otros argumentos; no se repitió la operación".into());
            }
            return Ok(json!({"id":id,"name":name,"response":cached.response}));
        }
        if self.calls.len() >= 4096 {
            return Err("La llamada alcanzó el límite de su registro de herramientas; vuelve a abrir HiveLive".into());
        }
        let response = if self.cancelled.contains(id) {
            json!({"ok":false,"error":"Llamada de herramienta cancelada por el proveedor"})
        } else {
            match name {
                "consultar_a_bee" => {
                    let prompt = args["peticion"]
                        .as_str()
                        .map(str::trim)
                        .filter(|text| !text.is_empty() && text.len() <= MAX_TEXT);
                    if self.synthetic {
                        json!({"ok":false,"error":"Este es un aviso de la colmena, no un pedido nuevo. No vuelvas a delegarlo."})
                    } else if let Some(prompt) = prompt {
                        match start(prompt.to_owned(), self.delivery.clone()) {
                            Ok(job) => {
                                self.current.delegated = true;
                                json!({"ok":true,"status":"accepted","job_id":job,"message":"Petición aceptada; el resultado real llegará después"})
                            }
                            Err(error) => {
                                json!({"ok":false,"error":self.config.redact(&error.to_string())})
                            }
                        }
                    } else {
                        json!({"ok":false,"error":"Falta una petición de texto válida, de hasta 16000 bytes"})
                    }
                }
                "estado_de_la_colmena" => {
                    let snapshot = activity::snapshot(&self.app.db, &self.config.user)?;
                    json!({"ok":true,"agents":snapshot["data"]["nodes"],"relations":snapshot["data"]["edges"]})
                }
                "cancelar_tarea" => {
                    let token = self
                        .app
                        .active
                        .lock()
                        .map_err(|_| "Turnos bloqueados")?
                        .get(&self.config.thread)
                        .cloned();
                    if let Some(token) = token {
                        token.cancel();
                        json!({"ok":true,"status":"cancellation_requested"})
                    } else {
                        json!({"ok":false,"error":"No hay un turno activo en esta conversación"})
                    }
                }
                _ => json!({"ok":false,"error":"Herramienta no disponible en HiveLive"}),
            }
        };
        self.calls.insert(
            id.to_owned(),
            Cached {
                fingerprint,
                response: response.clone(),
            },
        );
        Ok(json!({"id":id,"name":name,"response":response}))
    }

    pub async fn relay<L, R, F>(
        &mut self,
        client: &mut WebSocketStream<L>,
        remote: &mut WebSocketStream<R>,
        cancel: &CancellationToken,
        start: &mut F,
    ) -> Result<End>
    where
        L: AsyncRead + AsyncWrite + Unpin,
        R: AsyncRead + AsyncWrite + Unpin,
        F: FnMut(String, mpsc::Sender<Value>) -> Result<String>,
    {
        let mut clock = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                _=cancel.cancelled()=>return Ok(End::Closed),
                _=clock.tick()=> {
                    if self.current.finished.is_some_and(|at|at.elapsed()>=Duration::from_millis(250)) {self.finish_turn().await?;}
                    if self.app.active.lock().map_err(|_|"Turnos bloqueados")?.contains_key(&self.config.thread) {self.idle=Instant::now();}
                    if self.idle.elapsed()>=Duration::from_secs(180) {
                        send(client,Message::text(json!({"type":"closed","reason":"inactividad"}).to_string())).await?;return Ok(End::Closed);
                    }
                    if self.speak_at.is_some_and(|at|Instant::now()>=at) && self.generating.is_none_or(|at|at.elapsed()>=Duration::from_secs(20)) {
                        self.finish_turn().await?;
                        let text=self.speeches.drain(..).collect::<Vec<_>>().join(". ");self.speak_at=None;
                        if !text.is_empty() {self.synthetic=true;self.idle=Instant::now();
                            send(remote,Message::text(json!({"realtimeInput":{"text":format!("[COLMENA] Información, no un nuevo pedido. Cuenta brevemente: {text}")}}).to_string())).await?;}
                    }
                }
                event=self.work.recv()=>if let Some(event)=event {
                    let text=if event["ok"]==true {event["content"].as_str().unwrap_or("La colmena terminó sin texto")} else {event["error"].as_str().unwrap_or("La consulta necesita revisión")};
                    self.queue_speech(self.config.redact(text).chars().take(8000).collect(),true);self.idle=Instant::now();
                },
                event=self.events.recv()=>match event {
                    Ok(event) if event["type"]=="canvas:work_event"=> {
                        if let Some(id)=event["data"]["eventId"].as_str() && let Some(row)=self.app.db.col_get("narrationEvents",id)?
                            && let Some(frame)=self.narration(row.doc) {send(client,Message::text(frame.to_string())).await?;}
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_))=> {
                        let mut events:Vec<_>=config::docs(&self.app.db,"narrationEvents")?.into_iter().filter(|event|event["thread_id"]==self.config.thread).collect();
                        events.sort_by_key(|event|event["created_at"].as_u64().unwrap_or(0));
                        for event in events.into_iter().rev().take(4).rev() {if let Some(frame)=self.narration(event) {send(client,Message::text(frame.to_string())).await?;}}
                    }
                    _=>{},
                },
                incoming=client.next()=>match incoming {
                    Some(Ok(Message::Binary(pcm)))=> {
                        if pcm.is_empty() || pcm.len()>256*1024 || pcm.len()%2!=0 {return Err("PCM de entrada inválido: se espera PCM16 mono a 16 kHz".into());}
                        send(remote,Message::text(json!({"realtimeInput":{"audio":{"data":STANDARD.encode(pcm),"mimeType":"audio/pcm;rate=16000"}}}).to_string())).await?;
                    }
                    Some(Ok(Message::Text(text)))=> {
                        let value:Value=serde_json::from_str(&text)?;
                        match value["type"].as_str() {
                            Some("ping")=>send(client,Message::text(json!({"type":"pong"}).to_string())).await?,
                            Some("text")=> {
                                let text=value["content"].as_str().filter(|text|!text.trim().is_empty() && text.len()<=MAX_TEXT).ok_or("Texto de voz inválido")?;
                                self.finish_turn().await?;self.synthetic=false;self.idle=Instant::now();self.current.user.push_str(text);
                                send(remote,Message::text(json!({"realtimeInput":{"text":text}}).to_string())).await?;
                            }
                            Some("video")=> {
                                match video_frame(&value) {
                                    Ok(data)=> {
                                        self.idle=Instant::now();
                                        send(remote,Message::text(json!({"realtimeInput":{"video":{"data":data,"mimeType":"image/jpeg"}}}).to_string())).await?;
                                    }
                                    // One bad frame is that frame's problem, not the call's: the client is told and the call goes on.
                                    Err(reason)=> send(client,Message::text(json!({"type":"error","error":reason}).to_string())).await?,
                                }
                            }
                            Some("interrupt")=> {
                                self.suppress=self.generating.is_some();
                                send(client,Message::text(json!({"type":"interrupted"}).to_string())).await?;
                                send(remote,Message::text(json!({"clientContent":{"turns":[{"role":"user","parts":[{"text":"Interrumpe tu respuesta actual y espera mi siguiente indicación."}]}],"turnComplete":false}}).to_string())).await?;
                            }
                            _=>return Err("Mensaje no soportado por HiveLive".into()),
                        }
                    }
                    Some(Ok(Message::Ping(data)))=>send(client,Message::Pong(data)).await?,
                    Some(Ok(Message::Pong(_)))=>{},
                    _=>{self.finish_turn().await?;return Ok(End::Closed);},
                },
                incoming=remote.next()=>match incoming {
                    Some(Ok(message @ (Message::Text(_) | Message::Binary(_))))=> {
                        let value=decode_provider_message(message)?;
                        if value.get("error").is_some() {return Err(provider_error(&self.config,&value));}
                        let content=&value["serverContent"];
                        if (content["inputTranscription"]["text"].as_str().is_some() || content["modelTurn"].is_object()) && self.current.finished.is_some() {self.finish_turn().await?;}
                        if let Some(update)=value.get("sessionResumptionUpdate") {
                            self.resume=if update["resumable"]==true {update["newHandle"].as_str().filter(|handle|!handle.is_empty()).map(str::to_owned)} else {None};
                        }
                        if let Some(usage)=value.get("usageMetadata").filter(|_|self.current.finished.is_some() || !self.current.user.is_empty() || !self.current.assistant.is_empty() || self.current.delegated || self.current.audio || content["modelTurn"].is_object() || content["inputTranscription"].is_object() || content["outputTranscription"].is_object()) {self.current.usage=Some(parse_usage(usage)?);
                            let usage=self.current.usage.as_ref().unwrap();send(client,Message::text(json!({"type":"usage","inputTokens":self.total_in.saturating_add(usage.input_tokens),"outputTokens":self.total_out.saturating_add(usage.output_tokens)}).to_string())).await?;
                        }
                        if content["interrupted"]==true {self.generating=None;self.suppress=false;send(client,Message::text(json!({"type":"interrupted"}).to_string())).await?;}
                        for (field,role) in [("inputTranscription","user"),("outputTranscription","assistant")] {
                            if let Some(text)=content[field]["text"].as_str() {
                                if role=="user" {self.synthetic=false;self.current.user.push_str(text);} else {self.current.assistant.push_str(text);}
                                if self.current.user.len()>128*1024 || self.current.assistant.len()>128*1024 {return Err("Transcripción de voz demasiado larga".into());}
                                self.idle=Instant::now();send(client,Message::text(json!({"type":"transcript","role":role,"text":text}).to_string())).await?;
                            }
                        }
                        if let Some(parts)=content["modelTurn"]["parts"].as_array() {
                            for part in parts {if let Some(data)=part["inlineData"]["data"].as_str() {
                                let mime=part["inlineData"]["mimeType"].as_str().unwrap_or_default();
                                if !mime.starts_with("audio/pcm") || !mime.split(';').any(|part|part.trim()=="rate=24000") {return Err("Gemini Live devolvió audio de formato no admitido".into());}
                                let pcm=STANDARD.decode(data).map_err(|_|"Audio de Gemini Live inválido")?;
                                if pcm.len()>2*1024*1024 || pcm.len()%2!=0 {return Err("Audio de Gemini Live inválido".into());}
                                self.idle=Instant::now();self.generating.get_or_insert_with(Instant::now);
                                self.current.audio=true;
                                if !self.suppress {send(client,Message::Binary(pcm)).await?;}
                            }}
                        }
                        if let Some(calls)=value["toolCall"]["functionCalls"].as_array() {
                            if calls.len()>32 {return Err("Demasiadas herramientas en un frame de voz".into());}
                            let mut responses=Vec::new();for call in calls {
                                responses.push(self.tool(call,start)?);
                                send(client,Message::text(json!({"type":"tool","name":call["name"],"ok":responses.last().unwrap()["response"]["ok"]}).to_string())).await?;
                            }
                            send(remote,Message::text(json!({"toolResponse":{"functionResponses":responses}}).to_string())).await?;
                        }
                        if let Some(ids)=value["toolCallCancellation"]["ids"].as_array() {for id in ids.iter().filter_map(Value::as_str) {self.cancelled.insert(id.to_owned());}}
                        if content["turnComplete"]==true {
                            self.generating=None;self.suppress=false;self.current.finished=Some(Instant::now());
                            send(client,Message::text(json!({"type":"turn_complete"}).to_string())).await?;
                        }
                        if value.get("goAway").is_some() {
                            if self.resume.is_some() {return Ok(End::Resume);}
                            return Err("Gemini Live va a cerrar sin un estado reanudable; vuelve a abrir la llamada".into());
                        }
                    }
                    Some(Ok(Message::Ping(data)))=>send(remote,Message::Pong(data)).await?,
                    Some(Ok(Message::Pong(_)))=>{},
                    _=>return if self.resume.is_some() {Ok(End::Resume)}else{Err("Gemini Live cerró la conexión sin un estado reanudable".into())},
                },
            }
        }
    }
}

fn parse_usage(value: &Value) -> Result<Usage> {
    let count = |key: &str| -> Result<u64> {
        match value.get(key) {
            Some(value) => value
                .as_u64()
                .ok_or_else(|| "Contador de uso Live inválido".into()),
            None => Ok(0),
        }
    };
    Ok(Usage {
        input_tokens: count("promptTokenCount")?
            .checked_add(count("toolUsePromptTokenCount")?)
            .ok_or("Uso Live fuera de rango")?,
        output_tokens: count("responseTokenCount")?
            .checked_add(count("thoughtsTokenCount")?)
            .ok_or("Uso Live fuera de rango")?,
    })
}
async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    message: Message,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), socket.send(message))
        .await
        .map_err(|_| "La conexión de voz dejó de responder")?
        .map_err(|_| "La conexión de voz se cerró durante el envío")?;
    Ok(())
}
// Gemini sends JSON in both text and binary WebSocket frames. This only decodes
// provider messages: Rune's binary input is raw PCM and follows a separate path.
fn decode_provider_message(message: Message) -> Result<Value> {
    serde_json::from_slice(&message.into_data())
        .map_err(|_| "Gemini Live envió un mensaje JSON inválido".into())
}

fn provider_error(config: &LiveConfig, value: &Value) -> Box<dyn std::error::Error + Send + Sync> {
    format!(
        "Gemini Live: {}",
        config.redact(
            value["error"]["message"]
                .as_str()
                .unwrap_or("respuesta de error del proveedor")
        )
    )
    .into()
}
struct Registered {
    app: Arc<App>,
    user: String,
}
impl Drop for Registered {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.app.live_sessions.lock() {
            sessions.remove(&self.user);
        }
    }
}

pub(crate) async fn serve<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    app: Arc<App>,
    user: String,
    query: HashMap<String, String>,
    mut client: WebSocketStream<S>,
) -> Result<()> {
    let _connection = app.dashboard.connect();
    let result:Result<()>=async {
        let host=app.clone();let owner=user.clone();
        let config=tokio::task::spawn_blocking(move||LiveConfig::load(&host,&owner,&query)).await??;
        let cancel=app.shutdown.child_token();
        {let mut sessions=app.live_sessions.lock().map_err(|_|"Sesiones Live bloqueadas")?;
            if sessions.contains_key(&user) {return Err("Ya hay una llamada HiveLive activa para este usuario".into());}
            sessions.insert(user.clone(),cancel.clone());}
        let _registered=Registered {app:app.clone(),user:user.clone()};
        let mut session=LiveSession::new(app.clone(),config);let mut first=true;
        let run:Result<()>=async {
            loop {
                let mut connected=None;
                for attempt in 0..if first {1}else{3} {
                    let mut request=session.config.endpoint.as_str().into_client_request().map_err(|_|"URL Live inválida")?;
                    request.headers_mut().insert("x-goog-api-key",session.config.key.parse().map_err(|_|"Clave Live inválida")?);
                    let config=WebSocketConfig {max_message_size:Some(8*1024*1024),max_frame_size:Some(8*1024*1024),..Default::default()};
                    let socket=tokio::select! {_=cancel.cancelled()=>return Ok(()),socket=tokio::time::timeout(Duration::from_secs(15),tokio_tungstenite::connect_async_with_config(request,Some(config),true))=>socket};
                    match socket {
                        Ok(Ok((mut remote,_)))=>{session.setup(&mut remote).await?;connected=Some(remote);break;}
                        _ if !first && attempt<2=>tokio::time::sleep(Duration::from_millis(250)).await,
                        _=>return Err("No se pudo conectar a Gemini Live; revisa el proveedor y el modelo realtime de HiveDB".into()),
                    }
                }
                let mut remote=connected.ok_or("Gemini Live no está disponible")?;
                if first {session.ready(&mut client).await?;first=false;}
                let host=app.clone();let owner=user.clone();let thread=session.config.thread.clone();
                let end=session.relay(&mut client,&mut remote,&cancel,&mut move|prompt,delivery|server::start_live_work(host.clone(),owner.clone(),thread.clone(),prompt,delivery,false)).await?;
                let _=tokio::time::timeout(Duration::from_secs(1),remote.close(None)).await;
                if end==End::Closed {return Ok(());}
            }
        }.await;
        let finalized=session.finish_turn().await;
        if let Err(error)=run {return Err(session.config.redact(&error.to_string()).into());}
        finalized?;
        Ok(())
    }.await;
    if let Err(error) = result {
        eprintln!("[HiveLive] {error}");
        let _ = send(
            &mut client,
            Message::text(json!({"type":"error","error":error.to_string()}).to_string()),
        )
        .await;
    }
    let _ = send(
        &mut client,
        Message::text(json!({"type":"closed"}).to_string()),
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(1), client.close(None)).await;
    Ok(())
}
