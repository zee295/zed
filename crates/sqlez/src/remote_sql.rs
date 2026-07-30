//! Sync HTTP bridge from WASM to server-side SQLite.
//!
//! The browser has no usable local SQLite for Zed's sync `Connection` API, and
//! waiting on WebSocket from the main thread deadlocks (messages never arrive).
//! Synchronous XHR to `POST /sql` lets statement bind/step keep working while
//! the real database lives on the server under `{project}/.zed/remote.sqlite`.

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::OnceLock;
#[cfg(target_family = "wasm")]
use std::sync::atomic::{AtomicBool, Ordering};

static SQL_ENDPOINT: OnceLock<String> = OnceLock::new();
static SQL_RPC_ENDPOINT: OnceLock<String> = OnceLock::new();
#[cfg(target_family = "wasm")]
static SQL_CLIENT_ID: OnceLock<String> = OnceLock::new();
#[cfg(target_family = "wasm")]
static ASYNC_SQL_CLIENT: OnceLock<wasm_rpc::RpcClient> = OnceLock::new();
#[cfg(target_family = "wasm")]
static DATABASE_PREPARED: AtomicBool = AtomicBool::new(false);

/// Override the SQL HTTP endpoint (default: same-origin `/sql`).
pub fn set_sql_endpoint(url: impl Into<String>) {
    let _ = SQL_ENDPOINT.set(url.into());
}

/// Set the WebSocket endpoint used by the pre-started sqlez network worker.
pub fn set_sql_rpc_endpoint(url: impl Into<String>) {
    let _ = SQL_RPC_ENDPOINT.set(url.into());
}

/// Share the app's asynchronous WebSocket client with sqlez.
///
/// Synchronous statements still use the worker bridge, but callers can
/// prefetch latency-sensitive reads without parking the WASM UI thread.
#[cfg(target_family = "wasm")]
pub fn set_async_sql_client(client: wasm_rpc::RpcClient) {
    let _ = ASYNC_SQL_CLIENT.set(client);
}

#[cfg(target_family = "wasm")]
pub fn mark_database_prepared() {
    DATABASE_PREPARED.store(true, Ordering::Release);
}

#[cfg(target_family = "wasm")]
pub fn database_prepared() -> bool {
    DATABASE_PREPARED.load(Ordering::Acquire)
}

fn endpoint() -> &'static str {
    SQL_ENDPOINT.get().map(|s| s.as_str()).unwrap_or("/sql")
}

fn rpc_endpoint() -> Option<&'static str> {
    SQL_RPC_ENDPOINT.get().map(String::as_str)
}

#[cfg(target_family = "wasm")]
fn request_with_client_id(body: &Value) -> Value {
    let mut body = body.clone();
    let client_id = SQL_CLIENT_ID
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone();
    if let Some(params) = body.get_mut("params").and_then(Value::as_object_mut) {
        params.insert("client_id".into(), client_id.into());
    }
    body
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SqlParam {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    #[serde(rename = "blob")]
    Blob {
        #[serde(rename = "type")]
        kind: String,
        data: String,
    },
}

impl SqlParam {
    pub fn null() -> Self {
        SqlParam::Null
    }

    pub fn int(v: i64) -> Self {
        SqlParam::Int(v)
    }

    pub fn float(v: f64) -> Self {
        SqlParam::Float(v)
    }

    pub fn text(v: impl Into<String>) -> Self {
        SqlParam::Text(v.into())
    }

    pub fn blob(bytes: &[u8]) -> Self {
        use base64::Engine as _;
        SqlParam::Blob {
            kind: "blob".into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            SqlParam::Null => Value::Null,
            SqlParam::Int(v) => json!(v),
            SqlParam::Float(v) => json!(v),
            SqlParam::Text(v) => json!(v),
            SqlParam::Blob { data, .. } => json!({ "type": "blob", "data": data }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SqlBatchParam {
    Value(SqlParam),
    LastRowId {
        #[serde(rename = "type")]
        kind: &'static str,
        query: usize,
    },
}

impl SqlBatchParam {
    pub fn last_rowid(query: usize) -> Self {
        Self::LastRowId {
            kind: "batch_last_rowid",
            query,
        }
    }
}

impl From<SqlParam> for SqlBatchParam {
    fn from(value: SqlParam) -> Self {
        Self::Value(value)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlBatchQuery {
    sql: String,
    params: Vec<SqlBatchParam>,
}

impl SqlBatchQuery {
    pub fn new(sql: impl Into<String>, params: Vec<SqlParam>) -> Self {
        Self {
            sql: sql.into(),
            params: params.into_iter().map(Into::into).collect(),
        }
    }

    pub fn with_params(sql: impl Into<String>, params: Vec<SqlBatchParam>) -> Self {
        Self {
            sql: sql.into(),
            params,
        }
    }
}

#[derive(Debug, Clone)]
pub enum SqlCell {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlCell {
    fn from_json(value: &Value) -> Self {
        match value {
            Value::Null => SqlCell::Null,
            Value::Bool(b) => SqlCell::Int(i64::from(*b)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    SqlCell::Int(i)
                } else if let Some(u) = n.as_u64() {
                    SqlCell::Int(u as i64)
                } else {
                    SqlCell::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            Value::String(s) => SqlCell::Text(s.clone()),
            Value::Object(map) => {
                if map.get("type").and_then(|t| t.as_str()) == Some("blob") {
                    use base64::Engine as _;
                    let data = map.get("data").and_then(|d| d.as_str()).unwrap_or_default();
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .unwrap_or_default();
                    SqlCell::Blob(bytes)
                } else {
                    SqlCell::Text(value.to_string())
                }
            }
            Value::Array(_) => SqlCell::Text(value.to_string()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SqlQueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<SqlCell>>,
    pub last_rowid: i64,
    pub changes: i64,
}

#[derive(Deserialize)]
struct HttpEnvelope {
    result: Option<Value>,
    error: Option<String>,
}

/// Execute SQL on the server and return rows (empty for pure writes).
pub fn query(sql: &str, params: &[SqlParam]) -> Result<SqlQueryResult> {
    let params_json: Vec<Value> = params.iter().map(|p| p.to_json()).collect();

    // Read-through cache: a pure single-statement SELECT is served from an
    // in-memory cache when identical (sql+params) was just fetched. Any write
    // (non-SELECT) clears the cache, so reads after a write always hit the
    // server. The app is single-tenant (one client), so cross-client staleness
    // is not a concern. This collapses hot polling loops (e.g. repeated
    // kv_store reads every frame) into one server round-trip per key.
    #[cfg(target_family = "wasm")]
    {
        if let Some(cached) = read_cache::get_common(sql, &params_json) {
            return Ok(cached);
        }
        if let Some(cached) = read_cache_get(sql, &params_json) {
            return Ok(cached);
        }
        let result = run_query(sql, params_json.clone())?;
        if is_pure_select(sql) {
            read_cache_put(sql, &params_json, &result);
        } else {
            read_cache_clear();
            if touches_common_store(sql) {
                read_cache::apply_common_write(sql, &params_json);
            }
        }
        return Ok(result);
    }

    #[cfg(not(target_family = "wasm"))]
    {
        run_query(sql, params_json)
    }
}

fn run_query(sql: &str, params_json: Vec<Value>) -> Result<SqlQueryResult> {
    let body = json!({
        "method": "Sql::query",
        "params": {
            "sql": sql,
            "params": params_json,
        }
    });
    let value = http_sql(&body)?;
    parse_query_result(value)
}

/// Fetch a SELECT through the normal async WebSocket and seed the synchronous
/// statement cache. The subsequent sqlez statement performs no blocking RPC.
#[cfg(target_family = "wasm")]
pub async fn prefetch_query(sql: &str, params: &[SqlParam]) -> Result<()> {
    prefetch_query_result(sql, params).await?;
    Ok(())
}

/// Fetch a SELECT through the normal async WebSocket, seed the synchronous
/// statement cache, and return the decoded result to callers that need values
/// to prefetch dependent queries.
#[cfg(target_family = "wasm")]
pub async fn prefetch_query_result(sql: &str, params: &[SqlParam]) -> Result<SqlQueryResult> {
    let params_json: Vec<Value> = params.iter().map(SqlParam::to_json).collect();
    let client = ASYNC_SQL_CLIENT
        .get()
        .context("asynchronous SQL client is not initialized")?;
    let request = request_with_client_id(&json!({
        "params": {
            "sql": sql,
            "params": params_json,
        }
    }));
    let rpc_params = request
        .get("params")
        .cloned()
        .context("build asynchronous SQL parameters")?;
    let value: Value = client.call("Sql::query", &rpc_params).await?;
    let result = parse_query_result(value)?;
    read_cache_put(sql, &params_json, &result);
    Ok(result)
}

/// Execute a write through the asynchronous WebSocket transport. This is used
/// for UI-state persistence where synchronous HTTP would block the renderer.
#[cfg(target_family = "wasm")]
pub async fn execute_async(sql: &str, params: &[SqlParam]) -> Result<SqlQueryResult> {
    let params_json: Vec<Value> = params.iter().map(SqlParam::to_json).collect();
    let value = async_call(
        "Sql::query",
        request_with_client_id(&json!({
            "params": {
                "sql": sql,
                "params": params_json,
            }
        }))
        .get("params")
        .cloned()
        .context("build asynchronous SQL parameters")?,
    )
    .await?;
    let result = parse_query_result(value)?;
    read_cache_clear();
    if touches_common_store(sql) {
        read_cache::apply_common_write(sql, &params_json);
    }
    Ok(result)
}

/// Execute dependent statements as one atomic server-side transaction.
///
/// A parameter can refer to the row ID inserted by an earlier query in the
/// batch. This lets callers persist parent/child data without a network round
/// trip for every generated SQLite ID.
#[cfg(target_family = "wasm")]
pub async fn execute_batch_async(queries: Vec<SqlBatchQuery>) -> Result<()> {
    async_call(
        "Sql::batch",
        request_with_client_id(&json!({
            "params": {
                "queries": queries,
            }
        }))
        .get("params")
        .cloned()
        .context("build asynchronous SQL batch parameters")?,
    )
    .await?;
    read_cache_clear();
    Ok(())
}

#[cfg(target_family = "wasm")]
async fn async_call(method: &str, params: Value) -> Result<Value> {
    ASYNC_SQL_CLIENT
        .get()
        .context("asynchronous SQL client is not initialized")?
        .call(method, &params)
        .await
}

#[cfg(target_family = "wasm")]
mod read_cache {
    use super::{SqlCell, SqlQueryResult};
    use serde_json::Value;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    #[derive(Default)]
    pub struct CommonReads {
        kv: HashMap<String, String>,
        scoped: HashMap<(String, String), String>,
    }

    thread_local! {
        static CACHE: RefCell<HashMap<String, SqlQueryResult>> = RefCell::new(HashMap::new());
    }
    static COMMON: LazyLock<Mutex<Option<CommonReads>>> = LazyLock::new(Default::default);

    fn key(sql: &str, params: &[Value]) -> String {
        format!(
            "{}\u{1f}{}",
            sql,
            serde_json::to_string(params).unwrap_or_default()
        )
    }

    pub fn get(sql: &str, params: &[Value]) -> Option<SqlQueryResult> {
        CACHE.with(|c| c.borrow().get(&key(sql, params)).cloned())
    }

    pub fn put(sql: &str, params: &[Value], result: &SqlQueryResult) {
        CACHE.with(|c| {
            c.borrow_mut().insert(key(sql, params), result.clone());
        });
    }

    pub fn clear() {
        CACHE.with(|c| c.borrow_mut().clear());
    }

    pub fn prime(kv: Vec<(String, String)>, scoped: Vec<(String, String, String)>) {
        if let Ok(mut common) = COMMON.try_lock() {
            *common = Some(CommonReads {
                kv: kv.into_iter().collect(),
                scoped: scoped
                    .into_iter()
                    .map(|(namespace, key, value)| ((namespace, key), value))
                    .collect(),
            });
        }
    }

    pub fn get_common(sql: &str, params: &[Value]) -> Option<SqlQueryResult> {
        let normalized = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        let common = COMMON.try_lock().ok()?;
        let common = common.as_ref()?;
        let value = if normalized.contains("FROM scoped_kv_store WHERE") && params.len() == 2 {
            common.scoped.get(&(
                params[0].as_str()?.to_owned(),
                params[1].as_str()?.to_owned(),
            ))
        } else if normalized.contains("FROM kv_store WHERE") && params.len() == 1 {
            common.kv.get(params[0].as_str()?)
        } else {
            return None;
        };
        Some(SqlQueryResult {
            columns: vec!["value".to_owned()],
            rows: value
                .map(|value| vec![vec![SqlCell::Text(value.clone())]])
                .unwrap_or_default(),
            ..Default::default()
        })
    }

    pub fn apply_common_write(sql: &str, params: &[Value]) {
        let normalized = sql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        let Ok(mut common) = COMMON.try_lock() else {
            return;
        };
        let Some(common) = common.as_mut() else {
            return;
        };
        let text = |index: usize| params.get(index).and_then(Value::as_str).map(str::to_owned);

        if normalized.starts_with("insert or replace into scoped_kv_store") && params.len() == 3 {
            if let (Some(namespace), Some(key), Some(value)) = (text(0), text(1), text(2)) {
                common.scoped.insert((namespace, key), value);
            }
        } else if normalized.starts_with("delete from scoped_kv_store")
            && normalized.contains("and key")
            && params.len() == 2
        {
            if let (Some(namespace), Some(key)) = (text(0), text(1)) {
                common.scoped.remove(&(namespace, key));
            }
        } else if normalized.starts_with("delete from scoped_kv_store") && params.len() == 1 {
            if let Some(namespace) = text(0) {
                common
                    .scoped
                    .retain(|(stored_namespace, _), _| stored_namespace != &namespace);
            }
        } else if normalized.starts_with("insert or replace into kv_store") && params.len() == 2 {
            if let (Some(key), Some(value)) = (text(0), text(1)) {
                common.kv.insert(key, value);
            }
        } else if normalized.starts_with("delete from kv_store") && params.len() == 1 {
            if let Some(key) = text(0) {
                common.kv.remove(&key);
            }
        }
    }
}

#[cfg(target_family = "wasm")]
use read_cache::{clear as read_cache_clear, get as read_cache_get, put as read_cache_put};

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct KvpBootstrap {
    kv: Vec<(String, String)>,
    scoped: Vec<(String, String, String)>,
}

/// Prime synchronous preference reads from one server snapshot.
#[cfg(target_family = "wasm")]
pub fn prime_common_reads() -> Result<()> {
    let body = json!({
        "method": "Sql::bootstrap_kvp",
        "params": {}
    });
    let snapshot: KvpBootstrap =
        serde_json::from_value(http_sql(&body)?).context("parse KVP bootstrap")?;
    read_cache::prime(snapshot.kv, snapshot.scoped);
    Ok(())
}

#[cfg(target_family = "wasm")]
pub async fn prime_common_reads_async() -> Result<()> {
    let snapshot: KvpBootstrap =
        serde_json::from_value(async_call("Sql::bootstrap_kvp", request_params(json!({}))).await?)
            .context("parse asynchronous KVP bootstrap")?;
    read_cache::prime(snapshot.kv, snapshot.scoped);
    Ok(())
}

#[cfg(target_family = "wasm")]
fn is_pure_select(sql: &str) -> bool {
    let s = sql
        .trim_start()
        .trim_start_matches('(')
        .trim_start()
        .to_lowercase();
    s.starts_with("select") && !sql.contains(';')
}

#[cfg(target_family = "wasm")]
fn touches_common_store(sql: &str) -> bool {
    let sql = sql.to_ascii_lowercase();
    sql.contains("kv_store") || sql.contains("scoped_kv_store")
}

/// Execute a multi-statement script (migrations).
pub fn script(sql: &str) -> Result<()> {
    let body = json!({
        "method": "Sql::script",
        "params": { "sql": sql }
    });
    let _ = http_sql(&body)?;
    Ok(())
}

#[cfg(target_family = "wasm")]
pub async fn script_async(sql: &str) -> Result<()> {
    async_call("Sql::script", request_params(json!({ "sql": sql }))).await?;
    Ok(())
}

#[derive(Deserialize)]
pub struct MigrationDrift {
    pub index: usize,
    pub stored: String,
    pub proposed: String,
}

#[derive(Deserialize)]
struct MigrationResult {
    status: String,
    #[serde(default)]
    changes: Vec<MigrationDrift>,
}

/// Validate and apply one migration domain on the SQLite host.
pub fn migrate_domain(
    domain: &str,
    migrations: &[&str],
    allowed_changes: &[usize],
) -> Result<Vec<MigrationDrift>> {
    let body = json!({
        "method": "Sql::migrate",
        "params": {
            "domain": domain,
            "migrations": migrations,
            "allowed_changes": allowed_changes,
        }
    });
    let result: MigrationResult =
        serde_json::from_value(http_sql(&body)?).context("parse migration result")?;
    match result.status.as_str() {
        "ok" => Ok(Vec::new()),
        "drift" => Ok(result.changes),
        status => bail!("unknown migration result status: {status}"),
    }
}

#[cfg(target_family = "wasm")]
pub async fn migrate_domain_async(
    domain: &str,
    migrations: &[&str],
    allowed_changes: &[usize],
) -> Result<Vec<MigrationDrift>> {
    let result: MigrationResult = serde_json::from_value(
        async_call(
            "Sql::migrate",
            request_params(json!({
                "domain": domain,
                "migrations": migrations,
                "allowed_changes": allowed_changes,
            })),
        )
        .await?,
    )
    .context("parse asynchronous migration result")?;
    match result.status.as_str() {
        "ok" => Ok(Vec::new()),
        "drift" => Ok(result.changes),
        status => bail!("unknown migration result status: {status}"),
    }
}

/// Wipe server-side SQLite and reopen empty (migration history drift recovery).
pub fn reset_database() -> Result<()> {
    let body = json!({
        "method": "Sql::reset",
        "params": {}
    });
    let _ = http_sql(&body)?;
    Ok(())
}

#[cfg(target_family = "wasm")]
pub async fn reset_database_async() -> Result<()> {
    async_call("Sql::reset", request_params(json!({}))).await?;
    Ok(())
}

#[cfg(target_family = "wasm")]
fn request_params(params: Value) -> Value {
    request_with_client_id(&json!({ "params": params }))
        .get("params")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn parse_query_result(value: Value) -> Result<SqlQueryResult> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("sql result is not an object"))?;
    let columns = obj
        .get("columns")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let rows = obj
        .get("rows")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .map(|row| {
                    row.as_array()
                        .map(|cells| cells.iter().map(SqlCell::from_json).collect())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    let last_rowid = obj.get("last_rowid").and_then(|v| v.as_i64()).unwrap_or(0);
    let changes = obj.get("changes").and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(SqlQueryResult {
        columns,
        rows,
        last_rowid,
        changes,
    })
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
export function zedSqlWorkerRpcSync(endpoint, payload) {
    const buffers = self.__zedSqlRpcBuffers;
    if (!self.__zedSqlRpcBridge || !buffers) {
        throw new Error("sqlez RPC bridge was not prepared before Rust startup");
    }

    const control = new Int32Array(buffers.control);
    const requestData = new Uint8Array(buffers.requestData);
    const responseData = new Uint8Array(buffers.responseData);
    const request = JSON.parse(payload);
    request.endpoint = endpoint;
    const bytes = new TextEncoder().encode(JSON.stringify(request));
    if (bytes.length > requestData.length) {
        throw new Error("SQL RPC request exceeded 8 MiB");
    }

    requestData.set(bytes);
    Atomics.store(control, 2, bytes.length);
    Atomics.store(control, 4, 0);
    const sequence = Atomics.add(control, 0, 1) + 1;
    Atomics.notify(control, 0);

    const deadline = performance.now() + 30000;
    while (Atomics.load(control, 1) !== sequence) {
        const responseSequence = Atomics.load(control, 1);
        const remaining = Math.max(0, deadline - performance.now());
        if (Atomics.wait(control, 1, responseSequence, remaining) === "timed-out") {
            throw new Error("SQL RPC timed out");
        }
    }

    const state = Atomics.load(control, 4);
    const length = Atomics.load(control, 3);
    const text = new TextDecoder().decode(responseData.slice(0, length));
    if (state === -2) throw new Error("SQL RPC response exceeded 8 MiB");
    if (state < 0 && !text) throw new Error("SQL RPC worker failed");
    return text;
}
"#)]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(catch, js_name = zedSqlWorkerRpcSync)]
    fn worker_sql_rpc_sync(
        endpoint: &str,
        payload: &str,
    ) -> std::result::Result<String, wasm_bindgen::JsValue>;
}

#[cfg(target_family = "wasm")]
fn http_sql(body: &Value) -> Result<Value> {
    use web_sys::XmlHttpRequest;

    let payload =
        serde_json::to_string(&request_with_client_id(body)).context("serialize sql request")?;
    if wasm_thread::is_web_worker_thread()
        && let Some(endpoint) = rpc_endpoint()
    {
        let text = worker_sql_rpc_sync(endpoint, &payload)
            .map_err(|error| anyhow!("SQL worker RPC: {error:?}"))?;
        let envelope: HttpEnvelope = serde_json::from_str(&text)
            .with_context(|| format!("parse SQL RPC response: {text}"))?;
        if let Some(error) = envelope.error {
            bail!("remote sql RPC error: {error}");
        }
        return Ok(envelope.result.unwrap_or(Value::Null));
    }

    let xhr = XmlHttpRequest::new().map_err(|e| anyhow!("XmlHttpRequest: {e:?}"))?;
    // Synchronous request so sqlez's sync Connection/Statement API can block
    // until the server SQLite round-trip completes.
    xhr.open_with_async("POST", endpoint(), false)
        .map_err(|e| anyhow!("xhr open: {e:?}"))?;
    xhr.set_request_header("Content-Type", "application/json")
        .map_err(|e| anyhow!("xhr header: {e:?}"))?;

    xhr.send_with_opt_str(Some(&payload))
        .map_err(|e| anyhow!("xhr send: {e:?}"))?;

    let status = xhr.status().map_err(|e| anyhow!("xhr status: {e:?}"))?;
    let text = xhr
        .response_text()
        .map_err(|e| anyhow!("xhr response_text: {e:?}"))?
        .unwrap_or_default();

    let envelope: HttpEnvelope =
        serde_json::from_str(&text).with_context(|| format!("parse sql response: {text}"))?;

    if let Some(err) = envelope.error {
        bail!("remote sql error (http {status}): {err}");
    }
    // `Sql::script` and some void RPCs return JSON null — that is success.
    Ok(envelope.result.unwrap_or(Value::Null))
}

#[cfg(not(target_family = "wasm"))]
fn http_sql(_body: &Value) -> Result<Value> {
    bail!("remote sql HTTP bridge is only available on WASM")
}
