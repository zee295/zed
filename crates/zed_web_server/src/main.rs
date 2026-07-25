mod agent_rpc;
mod auth;
mod debug_adapter;
mod extension_rpc;
mod fs_rpc;
mod git_rpc;
mod highlight_rpc;
mod process_rpc;
mod rpc;
mod sql_rpc;
mod terminal_rpc;

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use axum::{
    Router,
    body::{Body, Bytes, boxed},
    extract::{ConnectInfo, Form, OriginalUri, Path as AxumPath, State, WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{any, get},
};
use clap::Parser;
use serde::Deserialize;
use tokio::fs;

#[derive(Parser)]
#[command(about = "Native backend for Zed Web")]
struct Args {
    root: PathBuf,
    static_root: PathBuf,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8090)]
    port: u16,
    #[arg(long, env = "ZED_WEB_TOKEN")]
    auth_token: Option<String>,
    #[arg(long)]
    secure_cookie: bool,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    restrict_paths: Option<bool>,
    #[arg(long)]
    no_restrict_paths: bool,
}

#[derive(Clone)]
pub struct AppState {
    root: Arc<PathBuf>,
    static_root: Arc<PathBuf>,
    auth_token: Arc<String>,
    secure_cookie: bool,
    restrict_paths: bool,
    login_limiter: Arc<auth::LoginLimiter>,
    events: tokio::sync::broadcast::Sender<serde_json::Value>,
    sql: Arc<sql_rpc::SqlRpc>,
    http: reqwest::Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw_args = std::env::args().collect::<Vec<_>>();
    if raw_args.get(1).map(String::as_str) == Some("__debug-adapter-proxy") {
        return debug_adapter::run_proxy(&raw_args[2..]).await;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zed_web_server=info".into()),
        )
        .init();

    let args = Args::parse();
    let root = args.root.canonicalize().context("invalid project root")?;
    let static_root = args
        .static_root
        .canonicalize()
        .context("invalid static root")?;
    let restrict_paths =
        load_restrict_paths(&root, args.restrict_paths, args.no_restrict_paths).await?;
    initialize_config(&root).await?;
    let (auth_token, token_path, created) = load_auth_token(&root, args.auth_token).await?;
    let sql = Arc::new(sql_rpc::SqlRpc::new(&root)?);
    let (events, _) = tokio::sync::broadcast::channel(256);
    let state = AppState {
        root: Arc::new(root),
        static_root: Arc::new(static_root),
        auth_token: Arc::new(auth_token),
        secure_cookie: args.secure_cookie,
        restrict_paths,
        login_limiter: Arc::new(auth::LoginLimiter::default()),
        events,
        sql,
        http: reqwest::Client::builder()
            .user_agent("ZedRemoteRust/0.1")
            .build()?,
    };

    let protected = Router::new()
        .route("/rpc", get(websocket))
        .route("/sql", axum::routing::post(sql_http))
        .route("/proxy/:provider/*rest", any(proxy_http))
        .route("/", get(index))
        .route("/*path", get(static_file))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));
    let app = Router::new()
        .route("/login", any(login))
        .route("/logout", any(logout))
        .route("/favicon.ico", get(favicon))
        .merge(protected)
        .with_state(state.clone());

    let address = format!("{}:{}", args.host, args.port)
        .parse::<SocketAddr>()
        .context("invalid listen address")?;
    tracing::info!(%address, root = %state.root.display(), "serving Zed Web");
    tracing::info!(path = %token_path.display(), "authentication token file");
    if created {
        tracing::info!(token = %state.auth_token, "new Zed Web access token");
    }
    axum::Server::bind(&address)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;
    Ok(())
}

async fn initialize_config(root: &Path) -> Result<()> {
    let config = root.join(".config/zed");
    fs::create_dir_all(config.join("snippets")).await?;
    create_if_missing(
        &config.join("settings.json"),
        b"{\n  \"project_panel\": { \"dock\": \"left\", \"auto_fold_dirs\": false }\n}\n",
    )
    .await?;
    create_if_missing(&config.join("global_settings.json"), b"{}\n").await
}

async fn create_if_missing(path: &Path, content: &[u8]) -> Result<()> {
    if fs::metadata(path).await.is_err() {
        fs::write(path, content).await?;
    }
    Ok(())
}

async fn load_auth_token(
    root: &Path,
    configured: Option<String>,
) -> Result<(String, PathBuf, bool)> {
    let path = root.join(".zed/web-auth-token");
    if let Some(token) = configured.filter(|token| !token.trim().is_empty()) {
        return Ok((token, path, false));
    }
    if let Ok(token) = fs::read_to_string(&path).await {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok((token, path, false));
        }
    }
    let token = auth::new_session("zed-web-bootstrap");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&path, format!("{token}\n")).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok((token, path, true))
}

async fn load_restrict_paths(
    root: &Path,
    cli_value: Option<bool>,
    no_restrict_paths: bool,
) -> Result<bool> {
    if no_restrict_paths {
        if cli_value.is_some() {
            anyhow::bail!("--restrict-paths and --no-restrict-paths cannot be used together");
        }
        return Ok(false);
    }
    if let Some(value) = cli_value {
        return Ok(value);
    }
    if let Some(raw) = std::env::var_os("ZED_WEB_RESTRICT_PATHS") {
        return match raw.to_string_lossy().trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => anyhow::bail!("ZED_WEB_RESTRICT_PATHS must be true/false or 1/0"),
        };
    }
    let path = root.join(".zed/web.json");
    let Ok(content) = fs::read(&path).await else {
        return Ok(false);
    };
    let config: serde_json::Value = serde_json::from_slice(&content)
        .with_context(|| format!("invalid web config {}", path.display()))?;
    match config.get("restrict_paths") {
        Some(serde_json::Value::Bool(value)) => Ok(*value),
        Some(_) => anyhow::bail!("{}: restrict_paths must be true or false", path.display()),
        None => Ok(false),
    }
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    form: Option<Form<LoginForm>>,
) -> Response {
    if auth::authenticated(&headers, &state.auth_token) {
        return Redirect::to("/").into_response();
    }
    let Some(Form(form)) = form else {
        return security_headers(Html(login_page(None)).into_response());
    };
    if !state.login_limiter.is_allowed(address.ip()) {
        return security_headers(
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "900")],
                Html(login_page(Some("Too many attempts. Try again later."))),
            )
                .into_response(),
        );
    }
    if !auth::token_matches(&form.token, &state.auth_token) {
        state.login_limiter.record_failure(address.ip());
        return security_headers(
            (
                StatusCode::UNAUTHORIZED,
                Html(login_page(Some("Invalid access token."))),
            )
                .into_response(),
        );
    }
    state.login_limiter.record_success(address.ip());
    let mut response = Redirect::to("/").into_response();
    let secure = if state.secure_cookie { "; Secure" } else { "" };
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        auth::AUTH_COOKIE,
        auth::new_session(&state.auth_token),
        30 * 24 * 60 * 60,
        secure
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("generated cookie is valid"),
    );
    response
}

async fn logout(State(state): State<AppState>) -> Response {
    let mut response = Redirect::to("/login").into_response();
    let secure = if state.secure_cookie { "; Secure" } else { "" };
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "zed_web_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{secure}"
        ))
        .expect("generated cookie is valid"),
    );
    response
}

async fn favicon() -> Response {
    security_headers(StatusCode::NO_CONTENT.into_response())
}

async fn websocket(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !auth::same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin WebSocket rejected").into_response();
    }
    upgrade
        .max_message_size(16 * 1024 * 1024)
        .max_frame_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| rpc::serve(socket, state))
        .into_response()
}

async fn sql_http(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    let method = body
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Sql::query")
        .to_string();
    let params = body
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let sql = state.sql.clone();
    let payload = match tokio::task::spawn_blocking(move || sql.dispatch(&method, &params)).await {
        Ok(Ok(result)) => serde_json::json!({"result": result, "error": null}),
        Ok(Err(error)) => serde_json::json!({"result": null, "error": error.to_string()}),
        Err(error) => serde_json::json!({"result": null, "error": error.to_string()}),
    };
    security_headers(axum::Json(payload).into_response())
}

async fn proxy_http(
    State(state): State<AppState>,
    method: axum::http::Method,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    AxumPath((provider, rest)): AxumPath<(String, String)>,
    body: Bytes,
) -> Response {
    let (upstream, key_variables, auth_header) = match provider.as_str() {
        "anthropic" => (
            "https://api.anthropic.com",
            &["ANTHROPIC_API_KEY", "ZED_AGENT_API_KEY"][..],
            "x-api-key",
        ),
        "openai" => (
            "https://api.openai.com",
            &["OPENAI_API_KEY", "ZED_AGENT_API_KEY"][..],
            "authorization",
        ),
        "acp-registry" => ("https://cdn.agentclientprotocol.com", &[][..], ""),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                format!("unknown proxy provider: {provider}"),
            )
                .into_response();
        }
    };
    let query = uri
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let target = format!("{upstream}/{}{query}", rest.trim_start_matches('/'));
    let request_method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut request = state.http.request(request_method, target);
    for (name, value) in &headers {
        if matches!(
            name.as_str(),
            "host"
                | "origin"
                | "referer"
                | "content-length"
                | "connection"
                | "accept-encoding"
                | "authorization"
                | "x-api-key"
                | "x-zed-proxy-target"
        ) {
            continue;
        }
        request = request.header(name.as_str(), value.as_bytes());
    }
    request = request.header("accept-encoding", "identity");
    let key = key_variables
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|key| !key.is_empty());
    if let Some(key) = key {
        request = if auth_header == "authorization" {
            request.bearer_auth(key)
        } else {
            request.header(auth_header, key)
        };
    }
    let upstream = match request.body(body).send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(?error, %provider, "proxy request failed");
            return (StatusCode::BAD_GATEWAY, error.to_string()).into_response();
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let proxy_body = if provider == "acp-registry" && rest.ends_with("/registry.json") {
        match upstream.bytes().await {
            Ok(bytes) => Body::from(rewrite_acp_registry(&bytes)),
            Err(error) => return (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
        }
    } else {
        Body::wrap_stream(upstream.bytes_stream())
    };
    let mut response = Response::new(boxed(proxy_body));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        if let Ok(content_type) = HeaderValue::from_str(&content_type) {
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
        }
    }
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("*"),
    );
    security_headers(response)
}

fn rewrite_acp_registry(bytes: &[u8]) -> Vec<u8> {
    let platform = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-aarch64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-aarch64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x86_64"
    } else {
        return bytes.to_vec();
    };
    let Ok(mut registry) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    for agent in registry
        .get_mut("agents")
        .and_then(serde_json::Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        let Some(binary) = agent
            .pointer_mut("/distribution/binary")
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        if let Some(host) = binary.get(platform).cloned() {
            binary.insert("wasm-host".to_string(), host);
        }
    }
    serde_json::to_vec(&registry).unwrap_or_else(|_| bytes.to_vec())
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    for name in ["workspace.html", "index.html"] {
        let target = state.static_root.join(name);
        if target.is_file() {
            return serve_file(&target, &headers).await;
        }
    }
    (StatusCode::NOT_FOUND, "workspace entry point not found").into_response()
}

async fn static_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let target = state.static_root.join(path);
    let Ok(target) = target.canonicalize() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !target.starts_with(&*state.static_root) || !target.is_file() {
        return StatusCode::FORBIDDEN.into_response();
    }
    serve_file(&target, &headers).await
}

async fn serve_file(target: &Path, headers: &HeaderMap) -> Response {
    let accepted = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let (source, encoding) = if accepted.contains("br") && compressed_path(target, "br").is_file() {
        (compressed_path(target, "br"), Some("br"))
    } else if accepted.contains("gzip") && compressed_path(target, "gz").is_file() {
        (compressed_path(target, "gz"), Some("gzip"))
    } else {
        (target.to_path_buf(), None)
    };
    let metadata = fs::metadata(&source).await.ok();
    let etag = metadata.as_ref().map(|metadata| {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        format!("\"{:x}-{:x}\"", metadata.len(), modified)
    });
    if etag.as_ref().is_some_and(|etag| {
        headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            == Some(etag)
    }) {
        let mut response = security_headers(StatusCode::NOT_MODIFIED.into_response());
        apply_static_cache_headers(&mut response, etag.as_deref());
        return response;
    }
    let Ok(content) = fs::read(source).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = Response::new(boxed(Body::from(content)));
    if let Ok(value) = HeaderValue::from_str(
        mime_guess::from_path(target)
            .first_or_octet_stream()
            .as_ref(),
    ) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    if let Some(encoding) = encoding {
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    let mut response = security_headers(response);
    apply_static_cache_headers(&mut response, etag.as_deref());
    response
}

fn apply_static_cache_headers(response: &mut Response, etag: Option<&str>) {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=0, must-revalidate"),
    );
    if let Some(etag) = etag.and_then(|etag| HeaderValue::from_str(etag).ok()) {
        response.headers_mut().insert(header::ETAG, etag);
    }
}

fn compressed_path(target: &Path, extension: &str) -> PathBuf {
    PathBuf::from(format!("{}.{}", target.display(), extension))
}

fn security_headers(mut response: Response) -> Response {
    for (name, value) in [
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-embedder-policy", "require-corp"),
        ("cross-origin-resource-policy", "same-origin"),
        ("cache-control", "no-store"),
    ] {
        response.headers_mut().insert(
            header::HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    response
}

fn login_page(error: Option<&str>) -> String {
    let message = error
        .map(|error| format!("<p class=\"error\" role=\"alert\">{error}</p>"))
        .unwrap_or_default();
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Sign in to Zed Web</title><style>
:root{{color-scheme:dark;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}}
*{{box-sizing:border-box}}body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#202329;color:#e6e8eb}}
main{{width:min(360px,calc(100vw - 32px))}}input,button{{width:100%;height:38px;margin-top:12px}}
.error{{color:#ff8d8d}}</style></head><body><main><h1>Zed Web</h1>
<p>Enter the access token configured on this server.</p>{message}
<form method="post" action="/login"><label for="token">Access token</label>
<input id="token" name="token" type="password" autofocus required>
<button type="submit">Sign in</button></form></main></body></html>"#
    )
}
