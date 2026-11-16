use hive_agent_core::provider::{ModelStream, NativeProvider as HttpProvider};
use hive_agent_core::sessions::SessionStore;
use napi_derive::napi;
use std::sync::Arc;
use std::sync::Mutex;

#[napi]
pub struct NativeEngine {
    inner: hive_agent_core::handles::EngineHandle,
}
#[napi]
impl NativeEngine {
    #[napi(constructor)]
    pub fn new(config: String) -> napi::Result<Self> {
        Ok(Self {
            inner: hive_agent_core::handles::EngineHandle::new(&config)
                .map_err(|e| napi::Error::from_reason(e.to_string()))?,
        })
    }
    #[napi]
    pub fn dispatch(&self, command: String) -> napi::Result<String> {
        self.inner
            .dispatch(&command)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
}

#[napi]
pub struct NativeObjective {
    inner: hive_agent_core::objective::Objective,
}
#[napi]
impl NativeObjective {
    #[napi(constructor)]
    pub fn new(policy: String) -> napi::Result<Self> {
        let policy =
            serde_json::from_str(&policy).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(Self {
            inner: hive_agent_core::objective::Objective::new(policy)
                .map_err(|e| napi::Error::from_reason(e.to_string()))?,
        })
    }
    #[napi]
    pub fn dispatch(&self, command: String) -> napi::Result<String> {
        self.inner
            .dispatch(&command)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
}

#[napi]
pub struct NativeProvider {
    inner: Arc<HttpProvider>,
}
#[napi]
impl NativeProvider {
    #[napi(constructor)]
    pub fn new(config: String) -> napi::Result<Self> {
        Ok(Self {
            inner: Arc::new(
                HttpProvider::from_json(&config)
                    .map_err(|e| napi::Error::from_reason(e.to_string()))?,
            ),
        })
    }
    #[napi]
    pub async fn start(&self, request: String, stream: bool) -> napi::Result<NativeModelRequest> {
        let request =
            serde_json::from_str(&request).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(NativeModelRequest {
            inner: self.inner.start(request, stream),
        })
    }
}

#[napi]
pub struct NativeModelRequest {
    inner: ModelStream,
}
#[napi]
impl NativeModelRequest {
    #[napi]
    pub async fn next(&self) -> napi::Result<Option<String>> {
        self.inner
            .next()
            .await
            .map_err(|e| napi::Error::from_reason(e.to_string()))?
            .map(|event| {
                serde_json::to_string(&event).map_err(|e| napi::Error::from_reason(e.to_string()))
            })
            .transpose()
    }
    #[napi]
    pub fn cancel(&self) {
        self.inner.cancel();
    }
}

#[napi]
pub struct NativeSessionStore {
    store: Arc<Mutex<Option<SessionStore>>>,
}

async fn storage_worker<T: Send + 'static>(
    operation: impl FnOnce() -> napi::Result<T> + Send + 'static,
) -> napi::Result<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|e| napi::Error::from_reason(format!("STORAGE_ERROR: worker failed: {e}")))?
}

#[napi]
pub async fn open_session_store(
    path: String,
    tenant_id: String,
) -> napi::Result<NativeSessionStore> {
    storage_worker(move || NativeSessionStore::new(path, tenant_id)).await
}

#[napi]
impl NativeSessionStore {
    #[napi]
    pub async fn open_agent_async(
        &self,
        agent: String,
        session: String,
        config: String,
        resume: bool,
    ) -> napi::Result<NativeDurableEngine> {
        let store = self.store.clone();
        storage_worker(move || Self { store }.open_agent(agent, session, config, resume)).await
    }
    #[napi]
    pub async fn dispatch_async(&self, command: String) -> napi::Result<String> {
        let store = self.store.clone();
        storage_worker(move || Self { store }.dispatch(command)).await
    }
    #[napi]
    pub async fn for_tenant_async(&self, tenant_id: String) -> napi::Result<Self> {
        let store = self.store.clone();
        storage_worker(move || Self { store }.for_tenant(tenant_id)).await
    }
    #[napi]
    pub async fn close_async(&self) -> napi::Result<()> {
        let store = self.store.clone();
        storage_worker(move || Self { store }.close()).await
    }
    #[napi]
    pub fn open_agent(
        &self,
        agent: String,
        session: String,
        config: String,
        resume: bool,
    ) -> napi::Result<NativeDurableEngine> {
        Ok(NativeDurableEngine {
            inner: Arc::new(
                hive_agent_core::durable_handle::DurableEngineHandle::open(
                    self.store.clone(),
                    agent,
                    session,
                    &config,
                    resume,
                )
                .map_err(|e| napi::Error::from_reason(e.to_string()))?,
            ),
        })
    }
    #[napi(constructor)]
    pub fn new(path: String, tenant_id: String) -> napi::Result<Self> {
        let store = SessionStore::open(path, tenant_id)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(Self {
            store: Arc::new(Mutex::new(Some(store))),
        })
    }

    #[napi]
    pub fn for_tenant(&self, tenant_id: String) -> napi::Result<Self> {
        let guard = self
            .store
            .lock()
            .map_err(|_| napi::Error::from_reason("STORAGE_ERROR: poisoned handle"))?;
        let store = guard
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("CLOSED: session store is closed"))?
            .for_tenant(tenant_id)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(Self {
            store: Arc::new(Mutex::new(Some(store))),
        })
    }

    #[napi]
    pub fn dispatch(&self, command: String) -> napi::Result<String> {
        let guard = self
            .store
            .lock()
            .map_err(|_| napi::Error::from_reason("STORAGE_ERROR: poisoned handle"))?;
        guard
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("CLOSED: session store is closed"))?
            .dispatch(&command)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    #[napi]
    pub fn close(&self) -> napi::Result<()> {
        self.store
            .lock()
            .map_err(|_| napi::Error::from_reason("STORAGE_ERROR: poisoned handle"))?
            .take();
        Ok(())
    }
}

#[napi]
pub fn dispatch(command: String) -> napi::Result<String> {
    hive_agent_core::dispatch(&command).map_err(|e| napi::Error::from_reason(e.to_string()))
}

#[napi]
pub struct NativeDurableEngine {
    inner: Arc<hive_agent_core::durable_handle::DurableEngineHandle>,
}
#[napi]
impl NativeDurableEngine {
    #[napi]
    pub fn dispatch(&self, command: String) -> napi::Result<String> {
        self.inner
            .dispatch(&command)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    #[napi]
    pub async fn dispatch_async(&self, command: String) -> napi::Result<String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.dispatch(&command))
            .await
            .map_err(|e| {
                napi::Error::from_reason(format!("STORAGE_ERROR: durable worker failed: {e}"))
            })?
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
}

#[napi]
pub struct NativeMcpClient {
    inner: Arc<hive_agent_core::mcp::McpClient>,
}
#[napi]
pub async fn connect_mcp(config: String) -> napi::Result<NativeMcpClient> {
    Ok(NativeMcpClient {
        inner: hive_agent_core::mcp::McpClient::connect_json(&config)
            .await
            .map_err(|e| napi::Error::from_reason(e.to_string()))?,
    })
}
#[napi]
impl NativeMcpClient {
    #[napi]
    pub async fn start_catalog_request(
        &self,
        method: String,
        params: String,
    ) -> napi::Result<NativeMcpRequest> {
        let params =
            serde_json::from_str(&params).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(NativeMcpRequest {
            inner: self
                .inner
                .start_catalog_request(&method, params)
                .map_err(|e| napi::Error::from_reason(e.to_string()))?,
        })
    }
    #[napi]
    pub fn catalog(&self) -> napi::Result<String> {
        serde_json::to_string(self.inner.tools())
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    #[napi]
    pub fn info(&self) -> napi::Result<String> {
        serde_json::to_string(self.inner.info())
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    #[napi]
    pub async fn start_call(
        &self,
        name: String,
        arguments: String,
        context: String,
    ) -> napi::Result<NativeMcpRequest> {
        let arguments = serde_json::from_str(&arguments)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let context =
            serde_json::from_str(&context).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(NativeMcpRequest {
            inner: self
                .inner
                .start_call(name, arguments, context)
                .map_err(|e| napi::Error::from_reason(e.to_string()))?,
        })
    }
    #[napi]
    pub async fn close(&self) {
        self.inner.close().await;
    }
}
impl Drop for NativeMcpClient {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}
#[napi]
pub struct NativeMcpRequest {
    inner: hive_agent_core::mcp::McpRequest,
}
#[napi]
impl NativeMcpRequest {
    #[napi]
    pub async fn result(&self) -> napi::Result<String> {
        let result = self
            .inner
            .result()
            .await
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        serde_json::to_string(&result).map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    #[napi]
    pub fn cancel(&self) {
        self.inner.cancel();
    }
}

#[napi]
pub struct NativeBrowser {
    inner: Arc<hive_agent_core::browser::Browser>,
}
#[napi]
pub async fn connect_browser(url: String, timeout_ms: u32) -> napi::Result<NativeBrowser> {
    Ok(NativeBrowser {
        inner: hive_agent_core::browser::Browser::connect(&url, timeout_ms.into())
            .await
            .map_err(|e| napi::Error::from_reason(e.to_string()))?,
    })
}
#[napi]
impl NativeBrowser {
    #[napi]
    pub async fn dispatch(&self, command: String) -> napi::Result<String> {
        let command =
            serde_json::from_str(&command).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let value = self
            .inner
            .dispatch(command)
            .await
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        serde_json::to_string(&value).map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    #[napi]
    pub fn cancel(&self) {
        self.inner.cancel();
    }
    #[napi]
    pub async fn close(&self) {
        self.inner.close().await;
    }
}
impl Drop for NativeBrowser {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}

/// Filesystem, command and web tools behind one sandbox configuration.
#[napi]
pub struct NativeTools {
    inner: hive_agent_core::tools::NativeTools,
    cancellation: hive_agent_core::provider::CancellationToken,
}
#[napi]
impl NativeTools {
    #[napi(constructor)]
    pub fn new(config: String) -> napi::Result<Self> {
        let config =
            serde_json::from_str(&config).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(Self {
            inner: hive_agent_core::tools::NativeTools::from_json(config)
                .map_err(|e| napi::Error::from_reason(e.to_string()))?,
            cancellation: hive_agent_core::provider::CancellationToken::new(),
        })
    }
    #[napi]
    pub async fn dispatch(&self, command: String) -> napi::Result<String> {
        let command =
            serde_json::from_str(&command).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let value = self
            .inner
            .dispatch(command, &self.cancellation)
            .await
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        serde_json::to_string(&value).map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    /// Capture an independent cancellation scope before starting a worker.
    #[napi]
    pub fn fork(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            cancellation: hive_agent_core::provider::CancellationToken::new(),
        }
    }
    #[napi]
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
    /// Stops the browser the `browser_*` tools started, if any.
    #[napi]
    pub async fn close(&self) {
        self.inner.close().await;
    }
    /// Synchronous best-effort stop for process-exit hooks.
    #[napi]
    pub fn close_now(&self) {
        self.inner.close_now();
    }
}

/// Opt-in decision oracle. The wrapper charges every call to an Objective.
#[napi]
pub struct NativeOracle {
    inner: Arc<hive_agent_core::oracle::Oracle>,
    cancellation: Mutex<hive_agent_core::provider::CancellationToken>,
}
#[napi]
impl NativeOracle {
    #[napi(constructor)]
    pub fn new(config: String) -> napi::Result<Self> {
        let config =
            serde_json::from_str(&config).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(
                hive_agent_core::oracle::Oracle::from_json(config)
                    .map_err(|e| napi::Error::from_reason(e.to_string()))?,
            ),
            cancellation: Mutex::new(hive_agent_core::provider::CancellationToken::new()),
        })
    }
    /// `{state, questions}` in, the reply JSON out.
    #[napi]
    pub async fn ask(&self, input: String) -> napi::Result<String> {
        let input =
            serde_json::from_str(&input).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let token = self.cancellation.lock().unwrap().clone();
        let reply = self
            .inner
            .ask_json(input, &token)
            .await
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        serde_json::to_string(&reply).map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    #[napi]
    pub fn status(&self) -> napi::Result<String> {
        let status = self
            .inner
            .status()
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        serde_json::to_string(&status).map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    #[napi]
    pub fn record_overruled(&self, reason: String) -> napi::Result<bool> {
        self.inner
            .record_overruled(&reason)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    #[napi]
    pub fn record_agreement(&self) -> napi::Result<()> {
        self.inner
            .record_agreement()
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    #[napi]
    pub fn reset(&self) -> napi::Result<()> {
        self.inner
            .reset()
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    /// Aborts a call in flight; later calls get a fresh token.
    #[napi]
    pub fn cancel(&self) {
        let mut token = self.cancellation.lock().unwrap();
        token.cancel();
        *token = hive_agent_core::provider::CancellationToken::new();
    }
}

#[napi]
pub struct NativeSecretVault {
    inner: Arc<hive_agent_core::services::secrets::SecretVaultHandle>,
}
#[napi]
impl NativeSessionStore {
    #[napi]
    pub async fn open_secret_vault(
        &self,
        id: String,
        key_id: String,
        key: napi::bindgen_prelude::Buffer,
    ) -> napi::Result<NativeSecretVault> {
        let key = hive_agent_core::services::secrets::SecretKey::new(&key)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let parent = self.store.clone();
        tokio::task::spawn_blocking(move || {
            Ok(NativeSecretVault {
                inner: Arc::new(
                    hive_agent_core::services::secrets::SecretVaultHandle::open(
                        parent, &id, &key_id, key,
                    )
                    .map_err(|e| napi::Error::from_reason(e.to_string()))?,
                ),
            })
        })
        .await
        .map_err(|_| napi::Error::from_reason("SECRET_WORKER_FAILED"))?
    }
}
#[napi]
impl NativeSecretVault {
    #[napi]
    pub async fn dispatch_async(&self, command: String) -> napi::Result<String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            inner
                .dispatch(command)
                .map_err(|e| napi::Error::from_reason(e.to_string()))
        })
        .await
        .map_err(|_| napi::Error::from_reason("SECRET_WORKER_FAILED"))?
    }
    #[napi]
    pub async fn rekey_async(
        &self,
        key_id: String,
        key: napi::bindgen_prelude::Buffer,
    ) -> napi::Result<String> {
        let key = hive_agent_core::services::secrets::SecretKey::new(&key)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let value = inner
                .rekey(&key_id, key)
                .map_err(|e| napi::Error::from_reason(e.to_string()))?;
            serde_json::to_string(&value)
                .map_err(|_| napi::Error::from_reason("SECRET_OPERATION_FAILED"))
        })
        .await
        .map_err(|_| napi::Error::from_reason("SECRET_WORKER_FAILED"))?
    }
    #[napi]
    pub fn close(&self) -> napi::Result<()> {
        self.inner
            .close()
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
    #[napi]
    pub async fn close_async(&self) -> napi::Result<()> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            inner
                .close()
                .map_err(|e| napi::Error::from_reason(e.to_string()))
        })
        .await
        .map_err(|_| napi::Error::from_reason("SECRET_WORKER_FAILED"))?
    }
}
