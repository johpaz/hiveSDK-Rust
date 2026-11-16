use super::{Inner, invalid};
use crate::{Error, Result, provider::CancellationToken};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    time::Duration,
};

const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

fn default_timeout() -> u64 {
    15_000
}

/// Details are dropped on purpose: request errors can embed the URL.
fn http(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Timeout
    } else {
        Error::State("web request failed".into())
    }
}

/// Search backends. The endpoint and credentials are trusted configuration, so
/// they bypass the public-host rule that applies to model-supplied URLs.
#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SearchProvider {
    /// Brave Search API; results are normalized to title/url/snippet.
    Brave {
        api_key: String,
        #[serde(default)]
        endpoint: Option<String>,
    },
    /// Any GET JSON endpoint; the bounded JSON response is returned unchanged.
    Custom {
        endpoint: String,
        query_param: String,
        #[serde(default)]
        count_param: Option<String>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebPolicy {
    /// Allow loopback, private and link-local targets. Off by default.
    #[serde(default)]
    pub allow_private_hosts: bool,
    /// When non-empty, `web_fetch` is limited to these exact host names.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub search: Option<SearchProvider>,
}

impl WebPolicy {
    pub(super) fn validate(&self) -> Result<()> {
        if self.timeout_ms == 0 || self.timeout_ms > 120_000 {
            return Err(invalid("web timeout_ms must be 1..=120000"));
        }
        match &self.search {
            Some(SearchProvider::Brave { api_key, endpoint }) => {
                if api_key.is_empty() {
                    return Err(invalid("search api_key must not be empty"));
                }
                parse_endpoint(endpoint.as_deref().unwrap_or(BRAVE_ENDPOINT))?;
            }
            Some(SearchProvider::Custom {
                endpoint,
                query_param,
                ..
            }) => {
                if query_param.is_empty() {
                    return Err(invalid("search query_param must not be empty"));
                }
                parse_endpoint(endpoint)?;
            }
            None => {}
        }
        Ok(())
    }
}

fn parse_endpoint(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).map_err(|_| invalid("invalid URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid("URL must be http(s) without embedded credentials"));
    }
    Ok(url)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || (o[0] == 198 && o[1] == 51 && o[2] == 100)
                || (o[0] == 203 && o[1] == 0 && o[2] == 113)
                || (o[0] == 100 && (64..128).contains(&o[1])))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            (first & 0xe000) == 0x2000
                && !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || (first & 0xfe00) == 0xfc00
                    || (first & 0xffc0) == 0xfe80
                    || (first == 0x2001 && (v6.segments()[1] < 0x200 || v6.segments()[1] == 0xdb8))
                    || first == 0x2002)
        }
    }
}

/// Resolve once and pin the validated addresses in the HTTP connector.
async fn check_target(policy: &WebPolicy, url: &reqwest::Url) -> Result<Vec<SocketAddr>> {
    check_host(&policy.allowed_hosts, policy.allow_private_hosts, url).await
}

/// The host rules shared by `web_fetch` and the browser tools: an optional exact
/// host allowlist, and no loopback/private/link-local targets unless allowed.
pub(super) async fn check_host(
    allowed_hosts: &[String],
    allow_private: bool,
    url: &reqwest::Url,
) -> Result<Vec<SocketAddr>> {
    let host = url
        .host_str()
        .ok_or_else(|| invalid("URL has no host"))?
        .to_owned();
    if !allowed_hosts.is_empty() && !allowed_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return Err(invalid("host is not in the allowed_hosts list"));
    }
    if allow_private {
        return Ok(Vec::new());
    }
    let lowered = host.to_ascii_lowercase();
    if lowered == "localhost" || lowered.ends_with(".localhost") || lowered.ends_with(".local") {
        return Err(invalid("host resolves to a non-public address"));
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let trimmed = host.trim_matches(['[', ']']).to_owned();
    let addresses: Vec<SocketAddr> = match trimmed.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::task::spawn_blocking(move || {
            (trimmed.as_str(), port)
                .to_socket_addrs()
                .map(|found| found.take(65).collect())
        })
        .await
        .map_err(|_| Error::State("DNS lookup task failed".into()))?
        .map_err(|_| invalid("host could not be resolved"))?,
    };
    if addresses.is_empty()
        || addresses.len() > 64
        || addresses.iter().any(|addr| !public_ip(addr.ip()))
    {
        return Err(invalid("host resolves to a non-public address"));
    }
    Ok(addresses)
}

fn client(policy: &WebPolicy, pinned: Option<(&str, &[SocketAddr])>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(policy.timeout_ms))
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none());
    if let Some((host, addresses)) = pinned
        && !addresses.is_empty()
    {
        builder = builder.resolve_to_addrs(host, addresses);
    }
    builder.build().map_err(http)
}

/// Read at most `max` bytes and report whether the body was cut.
async fn bounded(mut response: reqwest::Response, max: usize) -> Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http)? {
        let room = max - bytes.len();
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            return Ok((bytes, true));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, false))
}

async fn send(
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
) -> Result<reqwest::Response> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        response = request.send() => response.map_err(http),
    }
}

pub(super) async fn fetch(
    inner: &Inner,
    arguments: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let policy = inner
        .web
        .as_ref()
        .ok_or_else(|| invalid("web tools are disabled"))?;
    let url = parse_endpoint(arguments["url"].as_str().unwrap_or_default())?;
    let addresses = check_target(policy, &url).await?;
    let response = send(
        client(
            policy,
            Some((url.host_str().expect("validated host"), &addresses)),
        )?
        .get(url.clone())
        .header("accept", "text/*, application/json;q=0.9, */*;q=0.1"),
        cancel,
    )
    .await?;
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let (content_type, location) = (header("content-type"), header("location"));
    let (body, truncated) = bounded(response, inner.max_output).await?;
    Ok(
        json!({"url":url.as_str(),"status":status,"content_type":content_type,
              "location":location,"body":String::from_utf8_lossy(&body),"truncated":truncated}),
    )
}

pub(super) async fn search(
    inner: &Inner,
    arguments: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let policy = inner
        .web
        .as_ref()
        .ok_or_else(|| invalid("web tools are disabled"))?;
    let provider = policy
        .search
        .as_ref()
        .ok_or_else(|| invalid("web search is not configured"))?;
    let query = arguments["query"].as_str().unwrap_or_default();
    let limit = arguments["limit"].as_u64().unwrap_or(5);
    let http = client(policy, None)?;
    let request = match provider {
        SearchProvider::Brave { api_key, endpoint } => {
            let mut url = parse_endpoint(endpoint.as_deref().unwrap_or(BRAVE_ENDPOINT))?;
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("count", &limit.to_string());
            http.get(url)
                .header("accept", "application/json")
                .header("x-subscription-token", api_key)
        }
        SearchProvider::Custom {
            endpoint,
            query_param,
            count_param,
            headers,
        } => {
            let mut url = parse_endpoint(endpoint)?;
            {
                let mut pairs = url.query_pairs_mut();
                pairs.append_pair(query_param, query);
                if let Some(count) = count_param {
                    pairs.append_pair(count, &limit.to_string());
                }
            }
            headers
                .iter()
                .fold(http.get(url), |request, (name, value)| {
                    request.header(name, value)
                })
        }
    };
    let response = send(request, cancel).await?;
    let status = response.status();
    let (body, truncated) = bounded(response, inner.max_output.max(1024 * 1024)).await?;
    if !status.is_success() {
        return Err(Error::State(format!(
            "search provider returned HTTP {}",
            status.as_u16()
        )));
    }
    if truncated {
        return Err(Error::State(
            "search response exceeded the size limit".into(),
        ));
    }
    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|_| Error::State("search provider returned invalid JSON".into()))?;
    Ok(match provider {
        SearchProvider::Brave { .. } => {
            let results: Vec<Value> = parsed["web"]["results"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .take(limit as usize)
                        .map(|item| json!({"title":item["title"],"url":item["url"],"snippet":item["description"]}))
                        .collect()
                })
                .unwrap_or_default();
            json!({"results":results})
        }
        SearchProvider::Custom { .. } => json!({"response":parsed}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_public_addresses_pass_the_default_guard() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "240.0.0.1",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "2001:db8::1",
            "2002:a00:1::1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
    }
}
