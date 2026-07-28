use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    sync::Mutex as StdMutex,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, UNIX_EPOCH},
};

#[cfg(test)]
use anyhow::Context as _;
use anyhow::{Result, bail};
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
#[cfg(test)]
use std::fs;
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
    processes: Arc<Mutex<crate::process_rpc::ProcessManager>>,
    terminals: crate::terminal_rpc::TerminalManager,
    agents: crate::agent_rpc::AgentManager,
    workspace_ui: WorkspaceUiState,
    watches: HashMap<u64, WatchHandle>,
    watch_groups: HashMap<WatchKey, WatchGroup>,
    next_subscription_id: u64,
    notifications: mpsc::UnboundedSender<Message>,
    notification_target: Arc<Mutex<NotificationTarget>>,
    notification_forwarder: JoinHandle<()>,
    event_forwarder: JoinHandle<()>,
}

#[derive(Default)]
struct WorkspaceUiState {
    sidebar_open: bool,
}

struct WatchHandle {
    owner_generation: u64,
    key: WatchKey,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WatchKey {
    path: PathBuf,
    latency: Duration,
}

struct WatchGroup {
    subscription_paths: Arc<StdMutex<HashMap<u64, PathBuf>>>,
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
            processes: Arc::new(Mutex::new(crate::process_rpc::ProcessManager::new(
                fs.clone(),
                notifications.clone(),
            ))),
            terminals: crate::terminal_rpc::TerminalManager::new(fs, notifications.clone()),
            agents: crate::agent_rpc::AgentManager::new(
                (*state.root).clone(),
                state.http.clone(),
                notifications.clone(),
            ),
            workspace_ui: WorkspaceUiState::default(),
            watches: HashMap::new(),
            watch_groups: HashMap::new(),
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

    fn remove_watch(&mut self, subscription_id: u64) {
        let Some(watch) = self.watches.remove(&subscription_id) else {
            return;
        };
        let remove_group = self.watch_groups.get(&watch.key).is_some_and(|group| {
            let mut subscription_paths = group
                .subscription_paths
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            subscription_paths.remove(&subscription_id);
            subscription_paths.is_empty()
        });
        if remove_group && let Some(group) = self.watch_groups.remove(&watch.key) {
            group.task.abort();
        }
    }

    fn covering_watch_key(&self, path: &Path) -> Option<WatchKey> {
        self.watch_groups
            .keys()
            .filter(|key| watch_covers_path(&key.path, path))
            .max_by_key(|key| key.path.components().count())
            .cloned()
    }

    fn remove_watches_for_generation(&mut self, generation: u64) {
        let subscription_ids = self
            .watches
            .iter()
            .filter_map(|(subscription_id, watch)| {
                (watch.owner_generation == generation).then_some(*subscription_id)
            })
            .collect::<Vec<_>>();
        for subscription_id in subscription_ids {
            self.remove_watch(subscription_id);
        }
    }
}

impl Drop for RpcSession {
    fn drop(&mut self) {
        for (_, group) in self.watch_groups.drain() {
            group.task.abort();
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
    if outgoing
        .send(Message::Text(
            json!({
                "method": "Server::hello",
                "params": {
                    "instance_id": state.server_instance_id.as_str()
                }
            })
            .to_string(),
        ))
        .is_err()
    {
        return;
    }
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
        if handles_stateless(&method) {
            let (fs, sql) = {
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
                (session.fs.clone(), session.sql.clone())
            };
            let result = dispatch(fs, sql, method.clone(), params).await;
            let succeeded = result.is_ok();
            let response = match result {
                Ok(result) => json!({"id": request_id, "result": result, "error": null}),
                Err(error) => {
                    tracing::warn!(?error, %method, "rpc request failed");
                    json!({"id": request_id, "result": null, "error": error.to_string()})
                }
            };
            if outgoing.send(Message::Text(response.to_string())).is_err() {
                break;
            }
            if succeeded
                && matches!(
                    method.as_str(),
                    "Extensions::install"
                        | "Extensions::uninstall"
                        | "Extensions::install_dev"
                        | "Extensions::rebuild_dev"
                )
                && state
                    .events
                    .send(json!({
                        "method": "Host::extensions_changed",
                        "params": {}
                    }))
                    .is_err()
            {
                tracing::debug!("no RPC clients subscribed to extension changes");
            }
            continue;
        }
        if crate::agent_rpc::handles(&method) {
            let agents = session.lock().await.agents.clone();
            let result = agents.dispatch(&method, &params).await;
            let response = match result {
                Ok(result) => json!({"id": request_id, "result": result, "error": null}),
                Err(error) => {
                    tracing::warn!(?error, %method, "rpc request failed");
                    json!({"id": request_id, "result": null, "error": error.to_string()})
                }
            };
            if outgoing.send(Message::Text(response.to_string())).is_err() {
                break;
            }
            continue;
        }
        if method == "Browser::relay_localhost_callback" {
            let result = crate::auth_callback::relay(&params).await;
            let response = match result {
                Ok(result) => json!({"id": request_id, "result": result, "error": null}),
                Err(error) => {
                    tracing::warn!(?error, %method, "rpc request failed");
                    json!({"id": request_id, "result": null, "error": error.to_string()})
                }
            };
            if outgoing.send(Message::Text(response.to_string())).is_err() {
                break;
            }
            continue;
        }
        if crate::process_rpc::handles_streaming(&method) {
            let generation = bound_session
                .as_ref()
                .map(|(_, _, generation)| *generation)
                .unwrap_or_default();
            let processes = session.lock().await.processes.clone();
            let result = processes
                .lock()
                .await
                .dispatch(&method, &params, generation)
                .await;
            let response = match result {
                Ok(result) => json!({"id": request_id, "result": result, "error": null}),
                Err(error) => {
                    tracing::warn!(?error, %method, "rpc request failed");
                    json!({"id": request_id, "result": null, "error": error.to_string()})
                }
            };
            if outgoing.send(Message::Text(response.to_string())).is_err() {
                break;
            }
            continue;
        }
        let mut session = session.lock().await;
        let generation = bound_session
            .as_ref()
            .map(|(_, _, generation)| *generation)
            .unwrap_or_default();
        let result = if method == "Workspace::ui_state" {
            Ok(json!({"sidebar_open": session.workspace_ui.sidebar_open}))
        } else if method == "Workspace::set_sidebar_open" {
            session.workspace_ui.sidebar_open = params
                .get("open")
                .and_then(Value::as_bool)
                .unwrap_or_default();
            Ok(Value::Null)
        } else if crate::terminal_rpc::handles(&method) {
            session.terminals.dispatch(&method, &params)
        } else if method == "Fs::watch" {
            let subscription_id = session.next_subscription_id;
            session.next_subscription_id += 1;
            match watch_key(&session.fs, &params) {
                Ok(key) => {
                    let group_key = session.covering_watch_key(&key.path);
                    if let Some(group_key) = group_key.as_ref() {
                        let group = session
                            .watch_groups
                            .get(group_key)
                            .expect("covering watch group disappeared");
                        group
                            .subscription_paths
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert(subscription_id, key.path.clone());
                    } else {
                        let subscription_paths = Arc::new(StdMutex::new(HashMap::from([(
                            subscription_id,
                            key.path.clone(),
                        )])));
                        let task = start_watch(
                            session.fs.clone(),
                            key.clone(),
                            subscription_paths.clone(),
                            session.notifications.clone(),
                        );
                        session.watch_groups.insert(
                            key.clone(),
                            WatchGroup {
                                subscription_paths,
                                task,
                            },
                        );
                    }
                    session.watches.insert(
                        subscription_id,
                        WatchHandle {
                            owner_generation: generation,
                            key: group_key.unwrap_or(key),
                        },
                    );
                    Ok(json!({"subscription_id": subscription_id}))
                }
                Err(error) => Err(error),
            }
        } else if method == "Fs::attach_watches" {
            let subscription_ids = params
                .get("subscription_ids")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("missing filesystem watch subscription ids"));
            subscription_ids.map(|subscription_ids| {
                let mut attached = Vec::new();
                let mut missing = Vec::new();
                for subscription_id in subscription_ids.iter().filter_map(Value::as_u64) {
                    if let Some(watch) = session.watches.get_mut(&subscription_id) {
                        watch.owner_generation = generation;
                        attached.push(subscription_id);
                    } else {
                        missing.push(subscription_id);
                    }
                }
                json!({"attached": attached, "missing": missing})
            })
        } else if method == "Fs::unwatch" {
            let subscription_id = params
                .get("subscription_id")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("missing watch subscription id"));
            subscription_id.map(|subscription_id| {
                session.remove_watch(subscription_id);
                Value::Null
            })
        } else {
            Err(anyhow::anyhow!("unknown method: {method}"))
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
        let processes = {
            let session = session.lock().await;
            let notification_target = session.notification_target.clone();
            notification_target.lock().await.detach(generation);
            session.processes.clone()
        };
        processes.lock().await.detach_generation(generation);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            session
                .lock()
                .await
                .remove_watches_for_generation(generation);
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

fn handles_stateless(method: &str) -> bool {
    (FsRpc::handles(method)
        && !matches!(method, "Fs::watch" | "Fs::attach_watches" | "Fs::unwatch"))
        || crate::git_rpc::handles(method)
        || (crate::process_rpc::handles(method) && !crate::process_rpc::handles_streaming(method))
        || crate::extension_rpc::handles(method)
        || method == "Highlight::document"
        || method.starts_with("Sql::")
}

fn watch_key(fs_rpc: &FsRpc, params: &Value) -> Result<WatchKey> {
    let path = fs_rpc.path(params.get("path").and_then(Value::as_str).unwrap_or("."))?;
    let latency = params
        .get("latency")
        .and_then(Value::as_f64)
        .unwrap_or(1.0)
        .clamp(0.05, 30.0);
    Ok(WatchKey {
        path,
        latency: Duration::from_secs_f64(latency),
    })
}

fn start_watch(
    fs_rpc: Arc<FsRpc>,
    key: WatchKey,
    subscription_paths: Arc<StdMutex<HashMap<u64, PathBuf>>>,
    outgoing: mpsc::UnboundedSender<Message>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let path = key.path;
        let mut known: Option<HashMap<PathBuf, (u64, u128)>> = None;
        loop {
            let scan_started = std::time::Instant::now();
            let snapshot_path = path.clone();
            let current = match tokio::task::spawn_blocking(move || watch_snapshot(&snapshot_path))
                .await
            {
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(error)) => {
                    tracing::warn!(?error, path = %path.display(), "filesystem watch scan failed");
                    tokio::time::sleep(watch_delay(key.latency, scan_started.elapsed())).await;
                    continue;
                }
                Err(error) => {
                    tracing::warn!(?error, path = %path.display(), "filesystem watch task failed");
                    break;
                }
            };
            let Some(previous) = known.as_ref() else {
                known = Some(current);
                tokio::time::sleep(watch_delay(key.latency, scan_started.elapsed())).await;
                continue;
            };
            let mut events = Vec::new();
            for (path, stamp) in &current {
                let kind = match previous.get(path) {
                    None => Some("created"),
                    Some(old) if old != stamp => Some("changed"),
                    Some(_) => None,
                };
                if let Some(kind) = kind {
                    events.push((path.clone(), kind));
                }
            }
            for path in previous.keys() {
                if !current.contains_key(path) {
                    events.push((path.clone(), "removed"));
                }
            }
            if !events.is_empty() {
                let subscriptions = subscription_paths
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .iter()
                    .map(|(id, path)| (*id, path.clone()))
                    .collect::<Vec<_>>();
                for (subscription_id, subscription_path) in subscriptions {
                    let subscription_events = events
                        .iter()
                        .filter(|(path, _)| path.starts_with(&subscription_path))
                        .map(|(path, kind)| json!({"path": fs_rpc.virtualize(path), "kind": kind}))
                        .collect::<Vec<_>>();
                    if subscription_events.is_empty() {
                        continue;
                    }
                    if outgoing
                        .send(Message::Text(
                            json!({
                                "method": "Fs::watch_event",
                                "params": {
                                    "subscription_id": subscription_id,
                                    "events": subscription_events
                                }
                            })
                            .to_string(),
                        ))
                        .is_err()
                    {
                        return;
                    }
                }
            }
            known = Some(current);
            tokio::time::sleep(watch_delay(key.latency, scan_started.elapsed())).await;
        }
    })
}

fn watch_delay(latency: Duration, scan_duration: Duration) -> Duration {
    latency.max(scan_duration.saturating_mul(2))
}

const WATCH_EXCLUDED_DIRECTORIES: &[&str] = &[
    "node_modules",
    ".git",
    "vendor",
    ".venv",
    "venv",
    ".next",
    "target",
    "dist",
    ".cache",
    "__pycache__",
];

fn watch_covers_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    if relative
        .ancestors()
        .any(|ancestor| ancestor.ends_with(Path::new(".config/zed/node/cache")))
    {
        return false;
    }
    !relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| WATCH_EXCLUDED_DIRECTORIES.contains(&name))
    })
}

fn watch_entry_is_excluded(root: &Path, entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return false;
    }
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| WATCH_EXCLUDED_DIRECTORIES.contains(&name))
        || entry
            .path()
            .strip_prefix(root)
            .is_ok_and(|path| path.ends_with(Path::new(".config/zed/node/cache")))
}

fn watch_snapshot(root: &Path) -> Result<HashMap<PathBuf, (u64, u128)>> {
    let mut snapshot = HashMap::new();
    if !root.exists() {
        return Ok(snapshot);
    }
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !watch_entry_is_excluded(root, entry));
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(?error, root = %root.display(), "skipping unreadable watch entry");
                continue;
            }
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::debug!(
                    ?error,
                    path = %entry.path().display(),
                    "skipping vanished watch entry"
                );
                continue;
            }
        };
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

    #[tokio::test]
    async fn filesystem_watch_uses_existing_files_as_a_silent_baseline() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join("existing.txt"), "existing")?;
        let fs_rpc = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let key = watch_key(&fs_rpc, &json!({"path": "/workspace", "latency": 0.05}))?;
        let subscription_paths = Arc::new(StdMutex::new(HashMap::from([(7, key.path.clone())])));
        let (outgoing, mut notifications) = mpsc::unbounded_channel();
        let task = start_watch(fs_rpc, key, subscription_paths, outgoing);

        assert!(
            tokio::time::timeout(Duration::from_millis(150), notifications.recv())
                .await
                .is_err(),
            "the initial snapshot must not report every existing file as created"
        );

        fs::write(root.path().join("created.txt"), "created")?;
        let message = tokio::time::timeout(Duration::from_secs(1), notifications.recv())
            .await?
            .context("filesystem watch notification channel closed")?;
        let Message::Text(message) = message else {
            panic!("expected a text notification");
        };
        let message: Value = serde_json::from_str(&message)?;
        assert_eq!(message["params"]["subscription_id"], 7);
        assert!(
            message["params"]["events"]
                .as_array()
                .is_some_and(|events| events.iter().any(|event| {
                    event["kind"] == "created"
                        && event["path"]
                            .as_str()
                            .is_some_and(|path| path.ends_with("/created.txt"))
                }))
        );
        task.abort();
        Ok(())
    }

    #[test]
    fn filesystem_watch_snapshot_skips_generated_directories() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("src"))?;
        fs::create_dir_all(root.path().join("node_modules/package"))?;
        fs::create_dir_all(root.path().join(".git/objects"))?;
        fs::create_dir_all(root.path().join(".config/zed/node/cache/package"))?;
        fs::write(root.path().join("src/main.rs"), "fn main() {}")?;
        fs::write(
            root.path().join("node_modules/package/index.js"),
            "module.exports = {}",
        )?;
        fs::write(root.path().join(".git/objects/object"), "object")?;
        fs::write(
            root.path().join(".config/zed/node/cache/package/index.js"),
            "cache",
        )?;

        let snapshot = watch_snapshot(root.path())?;

        assert!(snapshot.contains_key(&root.path().join("src/main.rs")));
        assert!(!snapshot.contains_key(&root.path().join("node_modules")));
        assert!(!snapshot.contains_key(&root.path().join(".git")));
        assert!(!snapshot.contains_key(&root.path().join(".config/zed/node/cache")));
        Ok(())
    }

    #[test]
    fn excluded_descendant_requires_its_own_watch() {
        let root = Path::new("/workspace");
        assert!(watch_covers_path(root, Path::new("/workspace/src")));
        assert!(!watch_covers_path(
            root,
            Path::new("/workspace/node_modules/package")
        ));
        assert!(!watch_covers_path(
            root,
            Path::new("/workspace/.config/zed/node/cache/package")
        ));
    }

    #[tokio::test]
    async fn ancestor_watch_filters_events_for_descendant_subscriptions() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("project"))?;
        let fs_rpc = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let key = watch_key(&fs_rpc, &json!({"path": "/workspace", "latency": 0.05}))?;
        let subscription_paths = Arc::new(StdMutex::new(HashMap::from([(
            9,
            key.path.join("project"),
        )])));
        let (outgoing, mut notifications) = mpsc::unbounded_channel();
        let task = start_watch(fs_rpc, key, subscription_paths, outgoing);

        tokio::time::sleep(Duration::from_millis(100)).await;
        fs::write(root.path().join("outside.txt"), "outside")?;
        fs::write(root.path().join("project/inside.txt"), "inside")?;

        let message = tokio::time::timeout(Duration::from_secs(1), notifications.recv())
            .await?
            .context("filesystem watch notification channel closed")?;
        let Message::Text(message) = message else {
            panic!("expected a text notification");
        };
        let message: Value = serde_json::from_str(&message)?;
        let events = message["params"]["events"]
            .as_array()
            .context("watch events were not an array")?;

        assert!(
            events.iter().any(|event| {
                event["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("/project/inside.txt"))
            }),
            "descendant subscription did not receive its event"
        );
        assert!(
            events.iter().all(|event| {
                !event["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("/outside.txt"))
            }),
            "descendant subscription received an event outside its path"
        );
        task.abort();
        Ok(())
    }
}
