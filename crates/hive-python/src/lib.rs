use hive_agent_core::provider::{ModelStream, NativeProvider as HttpProvider};
use hive_agent_core::sessions::SessionStore;
use pyo3::types::{PyBytes, PyBytesMethods};
use pyo3::{exceptions::PyRuntimeError, prelude::*};
use std::sync::Mutex;
use std::sync::{Arc, OnceLock};

static RUNTIME: OnceLock<std::result::Result<tokio::runtime::Runtime, String>> = OnceLock::new();

#[pyclass(frozen)]
struct NativeEngine {
    inner: hive_agent_core::handles::EngineHandle,
}
#[pymethods]
impl NativeEngine {
    #[new]
    fn new(py: Python<'_>, config: String) -> PyResult<Self> {
        py.detach(|| {
            Ok(Self {
                inner: hive_agent_core::handles::EngineHandle::new(&config)
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
            })
        })
    }
    fn dispatch(&self, py: Python<'_>, command: String) -> PyResult<String> {
        py.detach(|| {
            self.inner
                .dispatch(&command)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
}
fn runtime() -> PyResult<&'static tokio::runtime::Runtime> {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| PyRuntimeError::new_err(format!("RUNTIME_ERROR: {e}")))
}

#[pyclass(frozen)]
struct NativeProvider {
    inner: Arc<HttpProvider>,
}
#[pymethods]
impl NativeProvider {
    #[new]
    fn new(py: Python<'_>, config: String) -> PyResult<Self> {
        py.detach(|| {
            Ok(Self {
                inner: Arc::new(
                    HttpProvider::from_json(&config)
                        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
                ),
            })
        })
    }
    fn start(&self, py: Python<'_>, request: String, stream: bool) -> PyResult<NativeModelRequest> {
        py.detach(|| {
            let request = serde_json::from_str(&request)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            let _guard = runtime()?.enter();
            Ok(NativeModelRequest {
                inner: self.inner.start(request, stream),
            })
        })
    }
}

#[pyclass(frozen)]
struct NativeModelRequest {
    inner: ModelStream,
}
#[pymethods]
impl NativeModelRequest {
    fn next(&self, py: Python<'_>) -> PyResult<Option<String>> {
        py.detach(|| {
            runtime()?
                .block_on(self.inner.next())
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?
                .map(|event| {
                    serde_json::to_string(&event)
                        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
                })
                .transpose()
        })
    }
    fn cancel(&self) {
        self.inner.cancel();
    }
}

#[pyclass(frozen)]
struct NativeSessionStore {
    store: Arc<Mutex<Option<SessionStore>>>,
}

#[pymethods]
impl NativeSessionStore {
    fn open_secret_vault(
        &self,
        py: Python<'_>,
        id: String,
        key_id: String,
        key: &Bound<'_, PyBytes>,
    ) -> PyResult<NativeSecretVault> {
        let key = hive_agent_core::services::secrets::SecretKey::new(key.as_bytes())
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        py.detach(|| {
            Ok(NativeSecretVault {
                inner: hive_agent_core::services::secrets::SecretVaultHandle::open(
                    self.store.clone(),
                    &id,
                    &key_id,
                    key,
                )
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
            })
        })
    }

    fn open_agent(
        &self,
        py: Python<'_>,
        agent: String,
        session: String,
        config: String,
        resume: bool,
    ) -> PyResult<NativeDurableEngine> {
        py.detach(|| {
            Ok(NativeDurableEngine {
                inner: hive_agent_core::durable_handle::DurableEngineHandle::open(
                    self.store.clone(),
                    agent,
                    session,
                    &config,
                    resume,
                )
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
            })
        })
    }
    #[new]
    fn new(py: Python<'_>, path: String, tenant_id: String) -> PyResult<Self> {
        let store = py
            .detach(|| SessionStore::open(path, tenant_id))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self {
            store: Arc::new(Mutex::new(Some(store))),
        })
    }

    fn for_tenant(&self, py: Python<'_>, tenant_id: String) -> PyResult<Self> {
        py.detach(|| {
            let guard = self
                .store
                .lock()
                .map_err(|_| PyRuntimeError::new_err("STORAGE_ERROR: poisoned handle"))?;
            let store = guard
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("CLOSED: session store is closed"))?
                .for_tenant(tenant_id)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            Ok(Self {
                store: Arc::new(Mutex::new(Some(store))),
            })
        })
    }

    fn dispatch(&self, py: Python<'_>, command: String) -> PyResult<String> {
        py.detach(|| {
            let guard = self
                .store
                .lock()
                .map_err(|_| PyRuntimeError::new_err("STORAGE_ERROR: poisoned handle"))?;
            guard
                .as_ref()
                .ok_or_else(|| PyRuntimeError::new_err("CLOSED: session store is closed"))?
                .dispatch(&command)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            self.store
                .lock()
                .map_err(|_| PyRuntimeError::new_err("STORAGE_ERROR: poisoned handle"))?
                .take();
            Ok(())
        })
    }
}

#[pyfunction]
fn dispatch(py: Python<'_>, command: String) -> PyResult<String> {
    py.detach(|| hive_agent_core::dispatch(&command))
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

/// Evaluate a declarative tool policy for one call: `{decision, rule, reason}` as JSON.
#[pyfunction]
fn evaluate_tool_policy(policy: String, tool: String, arguments: String) -> PyResult<String> {
    let parse =
        |text: &str| serde_json::from_str(text).map_err(|e| PyRuntimeError::new_err(e.to_string()));
    hive_agent_core::policy::evaluate_json(parse(&policy)?, &tool, parse(&arguments)?)
        .map(|value| value.to_string())
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(evaluate_tool_policy, module)?)?;
    module.add_class::<NativeBrowser>()?;
    module.add_function(wrap_pyfunction!(connect_browser, module)?)?;
    module.add_class::<NativeTools>()?;
    module.add_class::<NativeOracle>()?;
    module.add_class::<NativeDurableEngine>()?;
    module.add_class::<NativeEngine>()?;
    module.add_class::<NativeObjective>()?;
    module.add_class::<NativeSessionStore>()?;
    module.add_class::<NativeSecretVault>()?;
    module.add_class::<NativeProvider>()?;
    module.add_class::<NativeModelRequest>()?;
    module.add_function(wrap_pyfunction!(dispatch, module)?)?;
    module.add_class::<NativeMcpClient>()?;
    module.add_class::<NativeMcpManager>()?;
    module.add_function(wrap_pyfunction!(create_mcp_manager, module)?)?;
    module.add_class::<NativeMcpRequest>()?;
    module.add_function(wrap_pyfunction!(connect_mcp, module)?)?;
    Ok(())
}

#[pyclass(frozen)]
struct NativeDurableEngine {
    inner: hive_agent_core::durable_handle::DurableEngineHandle,
}
#[pymethods]
impl NativeDurableEngine {
    fn dispatch(&self, py: Python<'_>, command: String) -> PyResult<String> {
        py.detach(|| {
            self.inner
                .dispatch(&command)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
}

#[pyclass(frozen)]
struct NativeObjective {
    inner: hive_agent_core::objective::Objective,
}
#[pymethods]
impl NativeObjective {
    #[new]
    fn new(policy: String) -> PyResult<Self> {
        let policy =
            serde_json::from_str(&policy).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self {
            inner: hive_agent_core::objective::Objective::new(policy)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
        })
    }
    fn dispatch(&self, command: String) -> PyResult<String> {
        self.inner
            .dispatch(&command)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }
}

#[pyclass(frozen)]
struct NativeMcpClient {
    inner: Arc<hive_agent_core::mcp::McpClient>,
}
#[pyfunction]
fn connect_mcp(py: Python<'_>, config: String) -> PyResult<NativeMcpClient> {
    py.detach(|| {
        Ok(NativeMcpClient {
            inner: runtime()?
                .block_on(hive_agent_core::mcp::McpClient::connect_json(&config))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
        })
    })
}
#[pymethods]
impl NativeMcpClient {
    fn start_catalog_request(
        &self,
        py: Python<'_>,
        method: String,
        params: String,
    ) -> PyResult<NativeMcpRequest> {
        py.detach(|| {
            let params = serde_json::from_str(&params)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            let _guard = runtime()?.enter();
            Ok(NativeMcpRequest {
                inner: self
                    .inner
                    .start_catalog_request(&method, params)
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
            })
        })
    }
    fn tools_changed(&self) -> bool {
        self.inner.tools_changed()
    }
    fn refresh_tools(&self, py: Python<'_>) -> PyResult<String> {
        py.detach(|| {
            runtime()?
                .block_on(self.inner.refresh_tools())
                .map(|value| value.to_string())
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
    fn catalog(&self) -> PyResult<String> {
        serde_json::to_string(&*self.inner.tools())
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }
    fn info(&self) -> PyResult<String> {
        serde_json::to_string(self.inner.info()).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }
    fn start_call(
        &self,
        py: Python<'_>,
        name: String,
        arguments: String,
        context: String,
    ) -> PyResult<NativeMcpRequest> {
        py.detach(|| {
            let arguments = serde_json::from_str(&arguments)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            let context = serde_json::from_str(&context)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            let _guard = runtime()?.enter();
            Ok(NativeMcpRequest {
                inner: self
                    .inner
                    .start_call(name, arguments, context)
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
            })
        })
    }
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            runtime()?.block_on(self.inner.close());
            Ok(())
        })
    }
}
impl Drop for NativeMcpClient {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}
/// Several named MCP servers that can be reloaded while running.
#[pyclass(frozen)]
struct NativeMcpManager {
    inner: Arc<hive_agent_core::mcp::McpManager>,
}
#[pyfunction]
fn create_mcp_manager() -> NativeMcpManager {
    NativeMcpManager {
        inner: Arc::new(hive_agent_core::mcp::McpManager::new()),
    }
}
#[pymethods]
impl NativeMcpManager {
    fn apply(&self, py: Python<'_>, desired: String, drain_ms: u64) -> PyResult<String> {
        py.detach(|| {
            let desired = serde_json::from_str(&desired)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            runtime()?
                .block_on(self.inner.apply(desired, drain_ms))
                .map(|value| value.to_string())
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
    fn reconnect(&self, py: Python<'_>, name: String) -> PyResult<String> {
        py.detach(|| {
            runtime()?
                .block_on(self.inner.reconnect(&name))
                .map(|value| value.to_string())
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
    fn refresh_tools(&self, py: Python<'_>, name: String) -> PyResult<String> {
        py.detach(|| {
            runtime()?
                .block_on(self.inner.refresh_tools(&name))
                .map(|value| value.to_string())
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
    fn status(&self) -> String {
        self.inner.status().to_string()
    }
    fn catalog(&self) -> String {
        self.inner.catalog().to_string()
    }
    fn start_call(
        &self,
        py: Python<'_>,
        server: String,
        tool: String,
        arguments: String,
        context: String,
    ) -> PyResult<NativeMcpRequest> {
        py.detach(|| {
            let parse = |text: &str| {
                serde_json::from_str(text).map_err(|e| PyRuntimeError::new_err(e.to_string()))
            };
            let _guard = runtime()?.enter();
            Ok(NativeMcpRequest {
                inner: self
                    .inner
                    .start_call(&server, &tool, parse(&arguments)?, parse(&context)?)
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
            })
        })
    }
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            runtime()?.block_on(self.inner.close_all());
            Ok(())
        })
    }
}
impl Drop for NativeMcpManager {
    fn drop(&mut self) {
        self.inner.cancel_all();
    }
}
#[pyclass(frozen)]
struct NativeMcpRequest {
    inner: hive_agent_core::mcp::McpRequest,
}
#[pymethods]
impl NativeMcpRequest {
    fn result(&self, py: Python<'_>) -> PyResult<String> {
        py.detach(|| {
            let result = runtime()?
                .block_on(self.inner.result())
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            serde_json::to_string(&result).map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
    fn cancel(&self) {
        self.inner.cancel();
    }
}

#[pyclass(frozen)]
struct NativeBrowser {
    inner: Arc<hive_agent_core::browser::Browser>,
}
#[pyfunction]
fn connect_browser(py: Python<'_>, url: String, timeout_ms: u64) -> PyResult<NativeBrowser> {
    py.detach(|| {
        Ok(NativeBrowser {
            inner: runtime()?
                .block_on(hive_agent_core::browser::Browser::connect(&url, timeout_ms))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
        })
    })
}
#[pymethods]
impl NativeBrowser {
    fn dispatch(&self, py: Python<'_>, command: String) -> PyResult<String> {
        py.detach(|| {
            let command = serde_json::from_str(&command)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            let value = runtime()?
                .block_on(self.inner.dispatch(command))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            serde_json::to_string(&value).map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
    fn cancel(&self) {
        self.inner.cancel();
    }
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            runtime()?.block_on(self.inner.close());
            Ok(())
        })
    }
}
impl Drop for NativeBrowser {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}

/// Filesystem, command and web tools behind one sandbox configuration.
#[pyclass(frozen)]
struct NativeTools {
    inner: hive_agent_core::tools::NativeTools,
    cancellation: hive_agent_core::provider::CancellationToken,
}
#[pymethods]
impl NativeTools {
    #[new]
    fn new(config: String) -> PyResult<Self> {
        let config =
            serde_json::from_str(&config).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self {
            inner: hive_agent_core::tools::NativeTools::from_json(config)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
            cancellation: hive_agent_core::provider::CancellationToken::new(),
        })
    }
    fn dispatch(&self, py: Python<'_>, command: String) -> PyResult<String> {
        py.detach(|| {
            let command = serde_json::from_str(&command)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            let value = runtime()?
                .block_on(self.inner.dispatch(command, &self.cancellation))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            serde_json::to_string(&value).map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
    fn fork(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            cancellation: hive_agent_core::provider::CancellationToken::new(),
        }
    }
    fn cancel(&self) {
        self.cancellation.cancel();
    }
    /// Stops the browser the `browser_*` tools started, if any.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            runtime()?.block_on(self.inner.close());
            Ok(())
        })
    }
    fn close_now(&self) {
        self.inner.close_now();
    }
}

/// Opt-in decision oracle. The wrapper charges every call to an Objective.
#[pyclass(frozen)]
struct NativeOracle {
    inner: Arc<hive_agent_core::oracle::Oracle>,
    cancellation: Mutex<hive_agent_core::provider::CancellationToken>,
}
#[pymethods]
impl NativeOracle {
    #[new]
    fn new(config: String) -> PyResult<Self> {
        let config =
            serde_json::from_str(&config).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(
                hive_agent_core::oracle::Oracle::from_json(config)
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?,
            ),
            cancellation: Mutex::new(hive_agent_core::provider::CancellationToken::new()),
        })
    }
    /// `{state, questions}` in, the reply JSON out.
    fn ask(&self, py: Python<'_>, input: String) -> PyResult<String> {
        py.detach(|| {
            let input =
                serde_json::from_str(&input).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            let token = self.cancellation.lock().unwrap().clone();
            let reply = runtime()?
                .block_on(self.inner.ask_json(input, &token))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            serde_json::to_string(&reply).map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
    fn status(&self) -> PyResult<String> {
        let status = self
            .inner
            .status()
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        serde_json::to_string(&status).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }
    fn record_overruled(&self, reason: String) -> PyResult<bool> {
        self.inner
            .record_overruled(&reason)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }
    fn record_agreement(&self) -> PyResult<()> {
        self.inner
            .record_agreement()
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }
    fn reset(&self) -> PyResult<()> {
        self.inner
            .reset()
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }
    /// Aborts a call in flight; later calls get a fresh token.
    fn cancel(&self) {
        let mut token = self.cancellation.lock().unwrap();
        token.cancel();
        *token = hive_agent_core::provider::CancellationToken::new();
    }
}

#[pyclass(frozen)]
struct NativeSecretVault {
    inner: hive_agent_core::services::secrets::SecretVaultHandle,
}
#[pymethods]
impl NativeSecretVault {
    fn dispatch(&self, py: Python<'_>, command: String) -> PyResult<String> {
        py.detach(|| self.inner.dispatch(command))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }
    fn rekey(&self, py: Python<'_>, key_id: String, key: &Bound<'_, PyBytes>) -> PyResult<String> {
        let key = hive_agent_core::services::secrets::SecretKey::new(key.as_bytes())
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        py.detach(|| {
            let value = self
                .inner
                .rekey(&key_id, key)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            serde_json::to_string(&value)
                .map_err(|_| PyRuntimeError::new_err("SECRET_OPERATION_FAILED"))
        })
    }
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.close())
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }
}
