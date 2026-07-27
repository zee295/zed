use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};

use crate::{AppState, fs_rpc::FsRpc};

#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, Arc<Mutex<RpcSession>>>>,
    next_legacy_id: AtomicU64,
}

impl SessionRegistry {
    async fn get_or_create(
        &self,
        session_id: &str,
        state: &AppState,
    ) -> Result<Arc<Mutex<RpcSession>>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(session_id) {
            return Ok(session.clone());
        }
        let session = Arc::new(Mutex::new(RpcSession::new(state)?));
        sessions.insert(session_id.to_string(), session.clone());
        Ok(session)
    }

    fn legacy_id(&self) -> String {
        format!(
            "legacy-{}",
            self.next_legacy_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    pub async fn shutdown(&self) {
        let sessions = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .drain()
                .map(|(_, session)| session)
                .collect::<Vec<_>>()
        };
        for session in sessions {
            session.lock().await.shutdown().await;
        }
    }
}

struct NotificationTarget {
    senders: HashMap<u64, mpsc::UnboundedSender<Message>>,
    next_generation: u64,
    backlog: VecDeque<Message>,
}

impl NotificationTarget {
    fn forward(&mut self, message: Message) {
        self.senders
            .retain(|_, sender| sender.send(message.clone()).is_ok());
        if !self.senders.is_empty() {
            return;
        }
        if self.backlog.len() == 2048 {
            self.backlog.pop_front();
        }
        self.backlog.push_back(message);
    }

    fn attach(&mut self, sender: mpsc::UnboundedSender<Message>) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        while let Some(message) = self.backlog.pop_front() {
            if sender.send(message.clone()).is_err() {
                self.backlog.push_front(message);
                return generation;
            }
        }
        self.senders.insert(generation, sender);
        generation
    }

    fn detach(&mut self, generation: u64) {
        self.senders.remove(&generation);
    }
}

struct RpcSession {
    fs: Arc<FsRpc>,
    sql: Arc<crate::sql_rpc::SqlRpc>,
    sql_clients: HashSet<String>,
    processes: crate::process_rpc::ProcessManager,
    terminals: crate::terminal_rpc::TerminalManager,
    agents: crate::agent_rpc::AgentManager,
    watches: HashMap<u64, WatchHandle>,
    next_subscription_id: u64,
    notifications: mpsc::UnboundedSender<Message>,
    notification_target: Arc<Mutex<NotificationTarget>>,
    notification_forwarder: JoinHandle<()>,
    event_forwarder: JoinHandle<()>,
}

struct WatchHandle {
    owner_generation: u64,
    task: JoinHandle<()>,
}

impl RpcSession {
    fn new(state: &AppState) -> Result<Self> {
        let fs = Arc::new(FsRpc::new((*state.root).clone(), state.restrict_paths)?);
        let (notifications, mut notification_receiver) = mpsc::unbounded_channel();
        let notification_target = Arc::new(Mutex::new(NotificationTarget {
            senders: HashMap::new(),
            next_generation: 0,
            backlog: VecDeque::new(),
        }));
        let notification_forwarder = tokio::spawn({
            let notification_target = notification_target.clone();
            async move {
                while let Some(message) = notification_receiver.recv().await {
                    notification_target.lock().await.forward(message);
                }
            }
        });
        let mut event_receiver = state.events.subscribe();
        let event_notifications = notifications.clone();
        let event_forwarder = tokio::spawn(async move {
            loop {
                match event_receiver.recv().await {
                    Ok(event) => {
                        if event_notifications
                            .send(Message::Text(event.to_string()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "RPC session lagged behind host events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(Self {
            fs: fs.clone(),
            sql: state.sql.clone(),
            sql_clients: HashSet::new(),
            processes: crate::process_rpc::ProcessManager::new(fs.clone(), notifications.clone()),
            terminals: crate::terminal_rpc::TerminalManager::new(fs, notifications.clone()),
            agents: crate::agent_rpc::AgentManager::new(
                (*state.root).clone(),
                state.http.clone(),
                notifications.clone(),
            ),
            watches: HashMap::new(),
            next_subscription_id: 1,
            notifications,
            notification_target,
            notification_forwarder,
            event_forwarder,
        })
    }

    async fn shutdown(&mut self) {
        self.agents.shutdown().await;
    }
}

impl Drop for RpcSession {
    fn drop(&mut self) {
        for (_, watch) in self.watches.drain() {
            watch.task.abort();
        }
        for client_id in self.sql_clients.drain() {
            self.sql.release_client(&client_id);
        }
        self.event_forwarder.abort();
        self.notification_forwarder.abort();
    }
}

pub async fn serve(socket: WebSocket, state: AppState) {
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
    let mut shutdown = state.shutdown.subscribe();
    let mut bound_session: Option<(String, Arc<Mutex<RpcSession>>, u64)> = None;
    tracing::info!("rpc client connected");

    loop {
        let message = tokio::select! {
            _ = shutdown.recv() => break,
            message = receiver.next() => {
                let Some(message) = message else {
                    break;
                };
                message
            }
        };
        let Ok(Message::Text(text)) = message else {
            continue;
        };
        let envelope = match serde_json::from_str::<Value>(&text) {
            Ok(envelope) => envelope,
            Err(error) => {
                let response =
                    json!({"id": null, "result": null, "error": format!("invalid json: {error}")});
                if outgoing.send(Message::Text(response.to_string())).is_err() {
                    break;
                }
                continue;
            }
        };
        let requested_session_id = envelope
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let session = if let Some((session_id, session, _)) = bound_session.as_ref() {
            if requested_session_id
                .as_deref()
                .is_some_and(|requested| requested != session_id)
            {
                let response = json!({
                    "id": envelope.get("id").cloned().unwrap_or(Value::Null),
                    "result": null,
                    "error": "RPC session id changed on an active connection"
                });
                if outgoing.send(Message::Text(response.to_string())).is_err() {
                    break;
                }
                continue;
            }
            session.clone()
        } else {
            let session_id = requested_session_id.unwrap_or_else(|| state.rpc_sessions.legacy_id());
            let session = match state.rpc_sessions.get_or_create(&session_id, &state).await {
                Ok(session) => session,
                Err(error) => {
                    tracing::error!(?error, "failed to initialize RPC session");
                    break;
                }
            };
            let notification_target = session.lock().await.notification_target.clone();
            let generation = notification_target.lock().await.attach(outgoing.clone());
            bound_session = Some((session_id, session.clone(), generation));
            session
        };

        let method = envelope
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let request_id = envelope.get("id").cloned().unwrap_or(Value::Null);
        let params = envelope.get("params").cloned().unwrap_or_else(|| json!({}));
        if crate::extension_rpc::handles_network(&method) {
            let fs = session.lock().await.fs.clone();
            let http = state.http.clone();
            let outgoing = outgoing.clone();
            let events = state.events.clone();
            tokio::spawn(async move {
                let result =
                    crate::extension_rpc::dispatch_network(fs, http, method.clone(), params).await;
                let succeeded = result.is_ok();
                let response = match result {
                    Ok(result) => json!({"id": request_id, "result": result, "error": null}),
                    Err(error) => {
                        tracing::warn!(?error, %method, "rpc request failed");
                        json!({
                            "id": request_id,
                            "result": null,
                            "error": error.to_string()
                        })
                    }
                };
                if outgoing.send(Message::Text(response.to_string())).is_err() {
                    tracing::debug!(%method, "RPC connection closed before response");
                }
                if succeeded
                    && method == "Extensions::install"
                    && events
                        .send(json!({
                            "method": "Host::extensions_changed",
                            "params": {}
                        }))
                        .is_err()
                {
                    tracing::debug!("no RPC sessions subscribed to extension changes");
                }
            });
            continue;
        }
        let mut session = session.lock().await;
        if method.starts_with("Sql::") {
            session.sql_clients.insert(
                params
                    .get("client_id")
                    .and_then(Value::as_str)
                    .unwrap_or("legacy")
                    .to_string(),
            );
        }
        let generation = bound_session
            .as_ref()
            .map(|(_, _, generation)| *generation)
            .unwrap_or_default();
        let result = if crate::agent_rpc::handles(&method) {
            session.agents.dispatch(&method, &params).await
        } else if method == "Browser::relay_localhost_callback" {
            crate::auth_callback::relay(&params).await
        } else if crate::terminal_rpc::handles(&method) {
            session.terminals.dispatch(&method, &params)
        } else if crate::process_rpc::handles_streaming(&method) {
            session
                .processes
                .dispatch(&method, &params, generation)
                .await
        } else if method == "Fs::watch" {
            let subscription_id = session.next_subscription_id;
            session.next_subscription_id += 1;
            let watch = start_watch(
                session.fs.clone(),
                &params,
                subscription_id,
                session.notifications.clone(),
            );
            match watch {
                Ok(watch) => {
                    session.watches.insert(
                        subscription_id,
                        WatchHandle {
                            owner_generation: generation,
                            task: watch,
                        },
                    );
                    Ok(json!({"subscription_id": subscription_id}))
                }
                Err(error) => Err(error),
            }
        } else if method == "Fs::unwatch" {
            let subscription_id = params
                .get("subscription_id")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("missing watch subscription id"));
            subscription_id.map(|subscription_id| {
                if let Some(watch) = session.watches.remove(&subscription_id) {
                    watch.task.abort();
                }
                Value::Null
            })
        } else {
            dispatch(
                session.fs.clone(),
                session.sql.clone(),
                method.clone(),
                params,
            )
            .await
        };
        let response = match result {
            Ok(result) => json!({"id": request_id, "result": result, "error": null}),
            Err(error) => {
                tracing::warn!(?error, %method, "rpc request failed");
                json!({"id": request_id, "result": null, "error": error.to_string()})
            }
        };
        drop(session);

        if outgoing.send(Message::Text(response.to_string())).is_err() {
            break;
        }
        if response.get("error").is_some_and(Value::is_null)
            && matches!(
                method.as_str(),
                "Extensions::install"
                    | "Extensions::uninstall"
                    | "Extensions::install_dev"
                    | "Extensions::rebuild_dev"
            )
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
    if let Some((_, session, generation)) = bound_session {
        let mut session = session.lock().await;
        let notification_target = session.notification_target.clone();
        notification_target.lock().await.detach(generation);
        session.processes.detach_generation(generation);
        session.watches.retain(|_, watch| {
            if watch.owner_generation == generation {
                watch.task.abort();
                false
            } else {
                true
            }
        });
    }
    drop(outgoing);
    heartbeat.abort();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reconnect_replays_notifications_and_ignores_stale_detach() {
        let mut target = NotificationTarget {
            senders: HashMap::new(),
            next_generation: 0,
            backlog: VecDeque::new(),
        };
        target.forward(Message::Text("queued".into()));

        let (first_sender, mut first_receiver) = mpsc::unbounded_channel();
        let first_generation = target.attach(first_sender);
        assert_eq!(
            first_receiver.recv().await,
            Some(Message::Text("queued".into()))
        );

        let (second_sender, mut second_receiver) = mpsc::unbounded_channel();
        let second_generation = target.attach(second_sender);
        target.detach(first_generation);
        target.forward(Message::Text("current".into()));
        assert_eq!(
            second_receiver.recv().await,
            Some(Message::Text("current".into()))
        );

        target.detach(second_generation);
        target.forward(Message::Text("replayed".into()));
        let (third_sender, mut third_receiver) = mpsc::unbounded_channel();
        target.attach(third_sender);
        assert_eq!(
            third_receiver.recv().await,
            Some(Message::Text("replayed".into()))
        );
    }

    #[tokio::test]
    async fn broadcasts_notifications_to_all_workspace_connections() {
        let mut target = NotificationTarget {
            senders: HashMap::new(),
            next_generation: 0,
            backlog: VecDeque::new(),
        };
        let (first_sender, mut first_receiver) = mpsc::unbounded_channel();
        let (second_sender, mut second_receiver) = mpsc::unbounded_channel();
        target.attach(first_sender);
        target.attach(second_sender);

        target.forward(Message::Text("shared".into()));

        assert_eq!(
            first_receiver.recv().await,
            Some(Message::Text("shared".into()))
        );
        assert_eq!(
            second_receiver.recv().await,
            Some(Message::Text("shared".into()))
        );
    }
}
