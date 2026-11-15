//! Hive's managed GGUF backend: prepare the requested model, then infer using `local`.
//! Only readiness GETs are polled. Load is requested once; inference is never retried.
use super::{HttpProvider, NativeProvider, OpenAiConfig, OutputTokenParameter, Protocol};
use crate::{Error, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex, OnceLock, Weak},
    time::Duration,
};
use tokio::sync::Mutex;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HiveAgentsConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "super::default_response_limit")]
    pub max_response_bytes: usize,
    #[serde(default = "default_context")]
    pub context_window: u64,
}
fn default_timeout() -> u64 {
    120_000
}
fn default_context() -> u64 {
    50_000
}

pub(super) struct Managed {
    pub context_window: u64,
    pub api_base: String,
    pub lease: Arc<Mutex<()>>,
}

// One backend serves the `local` alias. Keep preparation AND streaming under the
// same lease, including providers constructed for separate specialist instances.
fn backend_lease(api_base: &str) -> Result<Arc<Mutex<()>>> {
    type Leases = StdMutex<HashMap<String, Weak<Mutex<()>>>>;
    static LEASES: OnceLock<Leases> = OnceLock::new();
    let mut leases = LEASES
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| Error::State("HiveAgents backend leases are poisoned".into()))?;
    leases.retain(|_, lease| lease.strong_count() > 0);
    if let Some(lease) = leases.get(api_base).and_then(Weak::upgrade) {
        return Ok(lease);
    }
    let lease = Arc::new(Mutex::new(()));
    leases.insert(api_base.to_owned(), Arc::downgrade(&lease));
    Ok(lease)
}

pub(super) fn create(config: HiveAgentsConfig) -> Result<NativeProvider> {
    if config.context_window == 0 {
        return Err(Error::Input(
            "HiveAgents context_window must be positive".into(),
        ));
    }
    let base = config.base_url.trim_end_matches('/');
    let api_base = base.strip_suffix("/v1").unwrap_or(base).to_owned();
    let common = OpenAiConfig {
        base_url: format!("{api_base}/v1"),
        api_key: config.api_key,
        timeout_ms: config.timeout_ms,
        max_response_bytes: config.max_response_bytes,
        output_token_parameter: OutputTokenParameter::MaxTokens,
    };
    let mut provider = HttpProvider::new(common, Protocol::OpenAi)?;
    provider.managed = Some(Managed {
        context_window: config.context_window,
        lease: backend_lease(&api_base)?,
        api_base,
    });
    Ok(NativeProvider {
        inner: Arc::new(provider),
    })
}

#[derive(Clone, Default, Deserialize)]
pub(super) struct Status {
    #[serde(default)]
    loaded: bool,
    model: Option<Model>,
}
#[derive(Clone, Deserialize)]
struct Model {
    name: Option<String>,
}
impl Status {
    fn serves(&self, model: &str) -> bool {
        self.loaded && self.model.as_ref().and_then(|m| m.name.as_deref()) == Some(model)
    }
}

trait Control: Sync {
    fn status(&self) -> impl std::future::Future<Output = Result<Status>> + Send;
    fn load(
        &self,
        model: &str,
        context: u64,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
    fn wait(&self) -> impl std::future::Future<Output = ()> + Send;
}

async fn ensure_loaded(
    control: &impl Control,
    model: &str,
    context: u64,
    checks: usize,
) -> Result<()> {
    let model = model.strip_prefix("hiveagents/").unwrap_or(model);
    if model.is_empty() {
        return Err(Error::Input("HiveAgents model ID must not be empty".into()));
    }
    if model == "local" {
        return Ok(());
    }
    if control.status().await?.serves(model) {
        return Ok(());
    }
    control.load(model, context).await?;
    for attempt in 0..checks {
        if control.status().await?.serves(model) {
            return Ok(());
        }
        if attempt + 1 < checks {
            control.wait().await;
        }
    }
    Err(Error::Provider(format!(
        "HiveAgents model {model} is not ready after the load request; no inference was sent"
    )))
}

struct HttpControl<'a> {
    provider: &'a HttpProvider,
    managed: &'a Managed,
}
fn transient(status: u16) -> bool {
    matches!(status, 502 | 503 | 504 | 524 | 530)
}
impl HttpControl<'_> {
    fn call(&self, method: reqwest::Method, route: &str) -> reqwest::RequestBuilder {
        let mut call = self
            .provider
            .client
            .request(method, format!("{}{route}", self.managed.api_base))
            .header("content-type", "application/json");
        if let Some(key) = &self.provider.config.api_key {
            call = call.bearer_auth(key);
        }
        call.headers(self.provider.headers.clone())
    }
    async fn bytes(&self, mut response: reqwest::Response) -> Result<Vec<u8>> {
        let cap = self.provider.config.max_response_bytes.min(64 * 1024);
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(super::http_error)? {
            if bytes.len().saturating_add(chunk.len()) > cap {
                return Err(Error::Provider(
                    "HiveAgents control response exceeds byte limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}
impl Control for HttpControl<'_> {
    async fn status(&self) -> Result<Status> {
        let response = self
            .call(reqwest::Method::GET, "/api/status")
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) if error.is_timeout() || error.is_connect() => return Ok(Status::default()),
            Err(error) => return Err(super::http_error(error)),
        };
        // Like the original Hive adapter, transient status failures may mean an
        // unloaded backend. Authentication and protocol failures remain explicit.
        if transient(response.status().as_u16()) {
            return Ok(Status::default());
        }
        if !response.status().is_success() {
            return Err(Error::Provider(format!(
                "HiveAgents /api/status returned HTTP {}",
                response.status().as_u16()
            )));
        }
        serde_json::from_slice(&self.bytes(response).await?)
            .map_err(|_| Error::Provider("HiveAgents returned invalid model status JSON".into()))
    }
    async fn load(&self, model: &str, context: u64) -> Result<()> {
        let response = self
            .call(reqwest::Method::POST, "/api/load")
            .json(&json!({"model":model,"config":{"ctx":context}}))
            .timeout(Duration::from_secs(90))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) if error.is_timeout() => return Ok(()), // Only poll status; never repeat this POST.
            Err(error) => return Err(super::http_error(error)),
        };
        if transient(response.status().as_u16()) {
            return Ok(());
        }
        if !response.status().is_success() {
            return Err(Error::Provider(format!(
                "HiveAgents /api/load returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let bytes = self.bytes(response).await?;
        if !bytes.is_empty() {
            let reply: Value = serde_json::from_slice(&bytes).map_err(|_| {
                Error::Provider("HiveAgents returned invalid load response JSON".into())
            })?;
            if reply["success"] == false || reply["error"].as_str().is_some_and(|e| !e.is_empty()) {
                return Err(Error::Provider(
                    "HiveAgents rejected the model load request".into(),
                ));
            }
        }
        Ok(())
    }
    async fn wait(&self) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub(super) async fn prepare(provider: &HttpProvider, managed: &Managed, model: &str) -> Result<()> {
    ensure_loaded(
        &HttpControl { provider, managed },
        model,
        managed.context_window,
        90,
    )
    .await
}

pub(super) fn blocked_header(name: &str) -> bool {
    name == "user-agent" || name.starts_with("x-stainless-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    struct Mock {
        statuses: StdMutex<VecDeque<Status>>,
        loads: StdMutex<Vec<(String, u64)>>,
        fail_load: bool,
    }
    impl Mock {
        fn new(statuses: &[(bool, &str)]) -> Self {
            Self {
                statuses: StdMutex::new(
                    statuses
                        .iter()
                        .map(|(loaded, name)| Status {
                            loaded: *loaded,
                            model: Some(Model {
                                name: Some((*name).into()),
                            }),
                        })
                        .collect(),
                ),
                loads: StdMutex::new(vec![]),
                fail_load: false,
            }
        }
    }
    impl Control for Mock {
        async fn status(&self) -> Result<Status> {
            Ok(self
                .statuses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default())
        }
        async fn load(&self, model: &str, context: u64) -> Result<()> {
            self.loads.lock().unwrap().push((model.into(), context));
            if self.fail_load {
                Err(Error::Provider(
                    "HiveAgents /api/load returned HTTP 403".into(),
                ))
            } else {
                Ok(())
            }
        }
        async fn wait(&self) {}
    }
    #[tokio::test]
    async fn warm_model_does_not_reload() {
        let mock = Mock::new(&[(true, "Qwen.gguf")]);
        ensure_loaded(&mock, "Qwen.gguf", 32768, 3).await.unwrap();
        assert!(mock.loads.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn cold_model_loads_once_with_hivedb_context_and_waits_for_exact_model() {
        let mock = Mock::new(&[
            (false, ""),
            (false, ""),
            (true, "other.gguf"),
            (true, "Qwen.gguf"),
        ]);
        ensure_loaded(&mock, "hiveagents/Qwen.gguf", 32768, 4)
            .await
            .unwrap();
        assert_eq!(
            *mock.loads.lock().unwrap(),
            vec![("Qwen.gguf".into(), 32768)]
        );
    }
    #[tokio::test]
    async fn accepted_load_does_not_authorize_inference_until_the_model_is_ready() {
        let mock = Mock::new(&[(true, "other.gguf")]);
        let error = ensure_loaded(&mock, "Qwen.gguf", 50000, 3)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no inference was sent"));
        assert_eq!(mock.loads.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn rejected_load_is_not_retried() {
        let mut mock = Mock::new(&[(false, "")]);
        mock.fail_load = true;
        assert!(
            ensure_loaded(&mock, "Qwen.gguf", 50000, 3)
                .await
                .unwrap_err()
                .to_string()
                .contains("403")
        );
        assert_eq!(mock.loads.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn local_alias_uses_current_backend_without_loading_a_literal_local_model() {
        let mock = Mock::new(&[]);
        ensure_loaded(&mock, "local", 50000, 1).await.unwrap();
        assert!(mock.loads.lock().unwrap().is_empty());
    }
    #[test]
    fn providers_for_the_same_backend_share_a_lease() {
        let a = backend_lease("https://fixture.invalid").unwrap();
        let b = backend_lease("https://fixture.invalid").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(
            &a,
            &backend_lease("https://other.invalid").unwrap()
        ));
    }
}
