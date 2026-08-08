//! Server-proxied HTTP client for model providers (wasm).
//!
//! The browser cannot reach model providers directly (CORS) and must not hold
//! public-provider API keys. The real `language_models` providers, however,
//! issue ordinary HTTP requests via `cx.http_client()`. This client sits in
//! front of browser `fetch` and rewrites known provider requests to the Rust
//! server's `/proxy/{provider}/...` endpoint. The server performs the upstream
//! request and streams the response back.
//!
//! Default loopback endpoints for Ollama, llama.cpp, and LM Studio are also
//! proxied because `localhost` in a browser means the user's machine, not the
//! Zed Web host. All other URLs pass through untouched to `fetch`.

use std::sync::Arc;

use futures::future::BoxFuture;
use http_client::{AsyncBody, HttpClient, http};

#[derive(Clone, Copy)]
struct ProxyRoute {
    provider: &'static str,
    strip_auth: bool,
}

/// Maps a provider API URI to the proxy provider id used by the Rust server.
fn proxy_route(uri: &http::Uri) -> Option<ProxyRoute> {
    let host = uri.host().unwrap_or_default();
    if host.eq_ignore_ascii_case("api.anthropic.com") {
        Some(ProxyRoute {
            provider: "anthropic",
            strip_auth: true,
        })
    } else if host.eq_ignore_ascii_case("api.openai.com") {
        Some(ProxyRoute {
            provider: "openai",
            strip_auth: true,
        })
    } else if host.eq_ignore_ascii_case("cdn.agentclientprotocol.com") {
        Some(ProxyRoute {
            provider: "acp-registry",
            strip_auth: false,
        })
    } else if host.eq_ignore_ascii_case("api.github.com") {
        Some(ProxyRoute {
            provider: "github-api",
            strip_auth: false,
        })
    } else if host.eq_ignore_ascii_case("github.com") {
        Some(ProxyRoute {
            provider: "github",
            strip_auth: false,
        })
    } else if host.eq_ignore_ascii_case("codeload.github.com") {
        Some(ProxyRoute {
            provider: "github-codeload",
            strip_auth: false,
        })
    } else if host.eq_ignore_ascii_case("raw.githubusercontent.com") {
        Some(ProxyRoute {
            provider: "github-raw",
            strip_auth: false,
        })
    } else if host.eq_ignore_ascii_case("objects.githubusercontent.com") {
        Some(ProxyRoute {
            provider: "github-objects",
            strip_auth: false,
        })
    } else if uri.scheme_str() == Some("http") && is_loopback_host(host) {
        match uri.port_u16() {
            Some(11434) => Some(ProxyRoute {
                provider: "ollama",
                strip_auth: false,
            }),
            Some(8080) => Some(ProxyRoute {
                provider: "llama-cpp",
                strip_auth: false,
            }),
            Some(1234) => Some(ProxyRoute {
                provider: "lm-studio",
                strip_auth: false,
            }),
            _ => None,
        }
    } else {
        None
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host == "[::1]"
}

/// Headers that public upstream providers expect to be injected host-side.
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
    fn rewrite(&self, uri: &http::Uri) -> Option<(http::Uri, ProxyRoute)> {
        let route = proxy_route(uri)?;
        let path = uri.path().trim_start_matches('/');
        let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        let rewritten = format!(
            "{}/proxy/{}/{}{}",
            self.server_base, route.provider, path, query
        );
        rewritten.parse().ok().map(|uri| (uri, route))
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
        if let Some((uri, route)) = self.rewrite(&parts.uri) {
            parts.uri = uri;
            if route.strip_auth {
                // Public-provider credentials are injected by the host.
                let auth_names: Vec<http::header::HeaderName> = parts
                    .headers
                    .keys()
                    .filter(|k| is_auth_header(k))
                    .cloned()
                    .collect();
                for name in auth_names {
                    parts.headers.remove(&name);
                }
            }
        }
        self.inner.send(http::Request::from_parts(parts, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_default_local_model_endpoints() {
        for (uri, expected) in [
            ("http://localhost:11434/api/tags", "ollama"),
            ("http://127.0.0.1:8080/v1/models", "llama-cpp"),
            ("http://[::1]:1234/api/v0/models", "lm-studio"),
        ] {
            let route = proxy_route(&uri.parse().unwrap()).unwrap();
            assert_eq!(route.provider, expected);
            assert!(!route.strip_auth);
        }
    }

    #[test]
    fn leaves_unrecognized_loopback_endpoints_direct() {
        assert!(proxy_route(&"http://localhost:3000/api".parse().unwrap()).is_none());
        assert!(proxy_route(&"https://localhost:11434/api".parse().unwrap()).is_none());
    }

    #[test]
    fn rewrite_preserves_path_and_query() {
        let client = ProxyHttpClient::new(
            Arc::new(http_client::FakeHttpClient::with_404_response()),
            "https://zed.example/",
        );
        let (uri, _) = client
            .rewrite(
                &"http://localhost:11434/api/chat?stream=true"
                    .parse()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(uri, "https://zed.example/proxy/ollama/api/chat?stream=true");
    }
}
