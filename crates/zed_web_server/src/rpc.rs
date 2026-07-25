use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{AppState, fs_rpc::FsRpc};

pub async fn serve(socket: WebSocket, state: AppState) {
    let fs_rpc = match FsRpc::new((*state.root).clone(), state.restrict_paths) {
        Ok(fs_rpc) => Arc::new(fs_rpc),
        Err(error) => {
            tracing::error!(?error, "failed to initialize RPC connection");
            return;
        }
    };
    let sql = state.sql.clone();
    let mut sql_clients = HashSet::new();
    let (mut sender, mut receiver) = socket.split();
    let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });
    let heartbeat_outgoing = outgoing.clone();
    let heartbeat = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(20));
        interval.tick().await;
        loop {
            interval.tick().await;
            if heartbeat_outgoing.send(Message::Ping(Vec::new())).is_err() {
                break;
            }
        }
    });
    let mut event_receiver = state.events.subscribe();
    let event_outgoing = outgoing.clone();
    let event_forwarder = tokio::spawn(async move {
        loop {
            match event_receiver.recv().await {
                Ok(event) => {
                    if event_outgoing
                        .send(Message::Text(event.to_string()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "RPC client lagged behind host events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let mut watches: Vec<JoinHandle<()>> = Vec::new();
    let mut next_subscription_id = 1_u64;
    let mut processes = crate::process_rpc::ProcessManager::new(fs_rpc.clone(), outgoing.clone());
    let mut terminals = crate::terminal_rpc::TerminalManager::new(fs_rpc.clone(), outgoing.clone());
    let agents = crate::agent_rpc::AgentManager::new(
        (*state.root).clone(),
        state.http.clone(),
        outgoing.clone(),
    );
    tracing::info!("rpc client connected");

    while let Some(message) = receiver.next().await {
        let Ok(Message::Text(text)) = message else {
            continue;
        };
        let envelope = serde_json::from_str::<Value>(&text);
        if let Ok(envelope) = envelope.as_ref() {
            let method = envelope
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if crate::extension_rpc::handles_network(method) {
                let request_id = envelope.get("id").cloned().unwrap_or(Value::Null);
                let params = envelope.get("params").cloned().unwrap_or_else(|| json!({}));
                let method = method.to_string();
                let fs_rpc = fs_rpc.clone();
                let http = state.http.clone();
                let outgoing = outgoing.clone();
                let events = state.events.clone();
                tokio::spawn(async move {
                    let result = crate::extension_rpc::dispatch_network(
                        fs_rpc,
                        http,
                        method.clone(),
                        params,
                    )
                    .await;
                    let succeeded = result.is_ok();
                    let response = match result {
                        Ok(result) => {
                            json!({"id": request_id, "result": result, "error": null})
                        }
                        Err(error) => {
                            tracing::warn!(?error, %method, "rpc request failed");
                            json!({
                                "id": request_id,
                                "result": null,
                                "error": error.to_string()
                            })
                        }
                    };
                    let _ = outgoing.send(Message::Text(response.to_string()));
                    if succeeded && method == "Extensions::install" {
                        let _ = events.send(json!({
                            "method": "Host::extensions_changed",
                            "params": {}
                        }));
                    }
                });
                continue;
            }
        }
        let event_method = envelope.as_ref().ok().and_then(|envelope| {
            envelope
                .get("method")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
        let response = match envelope {
            Ok(envelope) => {
                let request_id = envelope.get("id").cloned().unwrap_or(Value::Null);
                let method = envelope
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let params = envelope.get("params").cloned().unwrap_or_else(|| json!({}));
                if method.starts_with("Sql::") {
                    sql_clients.insert(
                        params
                            .get("client_id")
                            .and_then(Value::as_str)
                            .unwrap_or("legacy")
                            .to_string(),
                    );
                }
                if crate::agent_rpc::handles(&method) {
                    match agents.dispatch(&method, &params).await {
                        Ok(result) => {
                            json!({"id": request_id, "result": result, "error": null})
                        }
                        Err(error) => {
                            tracing::warn!(?error, %method, "rpc request failed");
                            json!({
                                "id": request_id,
                                "result": null,
                                "error": error.to_string()
                            })
                        }
                    }
                } else if crate::terminal_rpc::handles(&method) {
                    match terminals.dispatch(&method, &params) {
                        Ok(result) => {
                            json!({"id": request_id, "result": result, "error": null})
                        }
                        Err(error) => {
                            tracing::warn!(?error, %method, "rpc request failed");
                            json!({
                                "id": request_id,
                                "result": null,
                                "error": error.to_string()
                            })
                        }
                    }
                } else if crate::process_rpc::handles_streaming(&method) {
                    match processes.dispatch(&method, &params).await {
                        Ok(result) => {
                            json!({"id": request_id, "result": result, "error": null})
                        }
                        Err(error) => {
                            tracing::warn!(?error, %method, "rpc request failed");
                            json!({
                                "id": request_id,
                                "result": null,
                                "error": error.to_string()
                            })
                        }
                    }
                } else if method == "Fs::watch" {
                    let subscription_id = next_subscription_id;
                    next_subscription_id += 1;
                    match start_watch(fs_rpc.clone(), &params, subscription_id, outgoing.clone()) {
                        Ok(watch) => {
                            watches.push(watch);
                            json!({
                                "id": request_id,
                                "result": {"subscription_id": subscription_id},
                                "error": null
                            })
                        }
                        Err(error) => {
                            tracing::warn!(?error, %method, "rpc request failed");
                            json!({
                                "id": request_id,
                                "result": null,
                                "error": error.to_string()
                            })
                        }
                    }
                } else {
                    match dispatch(fs_rpc.clone(), sql.clone(), method.clone(), params).await {
                        Ok(result) => json!({"id": request_id, "result": result, "error": null}),
                        Err(error) => {
                            tracing::warn!(?error, %method, "rpc request failed");
                            json!({"id": request_id, "result": null, "error": error.to_string()})
                        }
                    }
                }
            }
            Err(error) => {
                json!({"id": null, "result": null, "error": format!("invalid json: {error}")})
            }
        };

        if outgoing.send(Message::Text(response.to_string())).is_err() {
            break;
        }
        if response.get("error").is_some_and(Value::is_null)
            && event_method.as_deref().is_some_and(|method| {
                matches!(
                    method,
                    "Extensions::install"
                        | "Extensions::uninstall"
                        | "Extensions::install_dev"
                        | "Extensions::rebuild_dev"
                )
            })
        {
            if state
                .events
                .send(json!({
                    "method": "Host::extensions_changed",
                    "params": {}
                }))
                .is_err()
            {
                tracing::debug!("no RPC clients subscribed to extension changes");
            }
        }
    }
    for watch in watches {
        watch.abort();
    }
    for client_id in sql_clients {
        sql.release_client(&client_id);
    }
    drop(outgoing);
    heartbeat.abort();
    event_forwarder.abort();
    writer.await.ok();
    tracing::info!("rpc client disconnected");
}

async fn dispatch(
    fs_rpc: Arc<FsRpc>,
    sql: Arc<crate::sql_rpc::SqlRpc>,
    method: String,
    params: Value,
) -> Result<Value> {
    if FsRpc::handles(&method) {
        return tokio::task::spawn_blocking(move || fs_rpc.dispatch(&method, &params)).await?;
    }
    if crate::git_rpc::handles(&method) {
        return tokio::task::spawn_blocking(move || {
            crate::git_rpc::dispatch(&fs_rpc, &method, &params)
        })
        .await?;
    }
    if crate::process_rpc::handles(&method) {
        return tokio::task::spawn_blocking(move || {
            crate::process_rpc::dispatch(&fs_rpc, &method, &params)
        })
        .await?;
    }
    if crate::extension_rpc::handles(&method) {
        return tokio::task::spawn_blocking(move || {
            crate::extension_rpc::dispatch(&fs_rpc, &method, &params)
        })
        .await?;
    }
    if method == "Highlight::document" {
        return tokio::task::spawn_blocking(move || {
            crate::highlight_rpc::dispatch(&fs_rpc, &params)
        })
        .await?;
    }
    if method.starts_with("Sql::") {
        return tokio::task::spawn_blocking(move || sql.dispatch(&method, &params)).await?;
    }
    bail!("unknown method: {method}")
}

fn start_watch(
    fs_rpc: Arc<FsRpc>,
    params: &Value,
    subscription_id: u64,
    outgoing: mpsc::UnboundedSender<Message>,
) -> Result<JoinHandle<()>> {
    let path = fs_rpc.path(params.get("path").and_then(Value::as_str).unwrap_or("."))?;
    let latency = params
        .get("latency")
        .and_then(Value::as_f64)
        .unwrap_or(1.0)
        .clamp(0.05, 30.0);
    Ok(tokio::spawn(async move {
        let mut known: HashMap<PathBuf, (u64, u128)> = HashMap::new();
        loop {
            let snapshot_path = path.clone();
            let current = match tokio::task::spawn_blocking(move || watch_snapshot(&snapshot_path))
                .await
            {
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(error)) => {
                    tracing::warn!(?error, path = %path.display(), "filesystem watch scan failed");
                    tokio::time::sleep(Duration::from_secs_f64(latency)).await;
                    continue;
                }
                Err(error) => {
                    tracing::warn!(?error, path = %path.display(), "filesystem watch task failed");
                    break;
                }
            };
            let mut events = Vec::new();
            for (path, stamp) in &current {
                let kind = match known.get(path) {
                    None => Some("created"),
                    Some(old) if old != stamp => Some("changed"),
                    Some(_) => None,
                };
                if let Some(kind) = kind {
                    events.push(json!({"path": fs_rpc.virtualize(path), "kind": kind}));
                }
            }
            for path in known.keys() {
                if !current.contains_key(path) {
                    events.push(json!({"path": fs_rpc.virtualize(path), "kind": "removed"}));
                }
            }
            if !events.is_empty()
                && outgoing
                    .send(Message::Text(
                        json!({
                            "method": "Fs::watch_event",
                            "params": {
                                "subscription_id": subscription_id,
                                "events": events
                            }
                        })
                        .to_string(),
                    ))
                    .is_err()
            {
                break;
            }
            known = current;
            tokio::time::sleep(Duration::from_secs_f64(latency)).await;
        }
    }))
}

fn watch_snapshot(root: &Path) -> Result<HashMap<PathBuf, (u64, u128)>> {
    let mut snapshot = HashMap::new();
    if !root.exists() {
        return Ok(snapshot);
    }
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.with_context(|| format!("scanning {}", root.display()))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        snapshot.insert(entry.path().to_path_buf(), (metadata.len(), modified));
    }
    Ok(snapshot)
}
