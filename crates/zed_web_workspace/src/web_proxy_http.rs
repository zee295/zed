//! Server-proxied HTTP client for model providers (wasm).
//!
//! The browser cannot reach `api.anthropic.com` / `api.openai.com` directly
//! (CORS) and must not hold API keys. The real `language_models` providers,
//! however, issue ordinary HTTPS requests via `cx.http_client()` and attach
//! auth headers themselves. This client sits in front of the browser `fetch`
//! transport and rewrites any request whose host is a known provider API to
//! the host server's `/proxy/{provider}/...` endpoint, stripping the outgoing
//! auth header. The Python server then injects the host-held key and performs
//! the real upstream request, streaming the response back.
//!
//! All other URLs pass through untouched to `fetch` (same-origin only, since
//! arbitrary cross-origin browser requests are still CORS-limited).

use std::sync::Arc;

use futures::future::BoxFuture;
use http_client::{AsyncBody, HttpClient, http};

/// Maps a provider API host to the proxy provider id used by server.py.
fn provider_for_host(host: &str) -> Option<&'static str> {
    if host.eq_ignore_ascii_case("api.anthropic.com") {
        Some("anthropic")
    } else if host.eq_ignore_ascii_case("api.openai.com") {
        Some("openai")
    } else if host.eq_ignore_ascii_case("cdn.agentclientprotocol.com") {
        Some("acp-registry")
    } else {
        None
    }
}

/// Headers that the upstream provider expects to be injected host-side; the
/// browser copy is dropped so no key material ever lives in the page.
fn is_auth_header(name: &http::header::HeaderName) -> bool {
    let n = name.as_str().to_ascii_lowercase();
    n == "authorization" || n == "x-api-key"
}

pub struct ProxyHttpClient {
    inner: Arc<dyn HttpClient>,
    /// e.g. "http://127.0.0.1:8080"
    server_base: String,
}

impl ProxyHttpClient {
    pub fn new(inner: Arc<dyn HttpClient>, server_base: impl Into<String>) -> Self {
        Self {
            inner,
            server_base: server_base.into().trim_end_matches('/').to_string(),
        }
    }

    /// Rewrite `https://api.anthropic.com/v1/messages?x=1`
    ///   → `{server}/proxy/anthropic/v1/messages?x=1`.
    fn rewrite(&self, uri: &http::Uri) -> http::Uri {
        let host = uri.host().unwrap_or_default();
        let Some(provider) = provider_for_host(host) else {
            return uri.clone();
        };
        let path = uri.path().trim_start_matches('/');
        let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        let rewritten = format!("{}/proxy/{}/{}{}", self.server_base, provider, path, query);
        rewritten.parse().unwrap_or_else(|_| uri.clone())
    }
}

impl HttpClient for ProxyHttpClient {
    fn user_agent(&self) -> Option<&http::header::HeaderValue> {
        self.inner.user_agent()
    }

    fn proxy(&self) -> Option<&http_client::Url> {
        None
    }

    fn send(
        &self,
        req: http::Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<http::Response<AsyncBody>>> {
        let (mut parts, body) = req.into_parts();
        parts.uri = self.rewrite(&parts.uri);
        // Drop any provider auth header — the host injects the real key.
        let auth_names: Vec<http::header::HeaderName> = parts
            .headers
            .keys()
            .filter(|k| is_auth_header(k))
            .cloned()
            .collect();
        for name in auth_names {
            parts.headers.remove(&name);
        }
        self.inner.send(http::Request::from_parts(parts, body))
    }
}
