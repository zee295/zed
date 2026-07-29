use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::Engine as _;
use fs::{
    CopyOptions, CreateOptions, FileHandle, Fs, JobEvent, JobEventReceiver, MTime, Metadata,
    PathEvent, PathEventKind, RemoveOptions, RenameOptions, TrashId, TrashRestoreError, Watcher,
};
use futures::{Stream, StreamExt, channel::mpsc, io::AsyncReadExt, stream};
use gpui::BackgroundExecutor;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io;
use std::pin::Pin;
#[cfg(target_family = "wasm")]
use std::sync::TryLockError;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::git::RemoteGitRepository;
use crate::transport::RemoteClient;

fn lock_shared<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    #[cfg(target_family = "wasm")]
    loop {
        match mutex.try_lock() {
            Ok(guard) => return guard,
            Err(TryLockError::Poisoned(error)) => return error.into_inner(),
            Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    {
        mutex.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// A remote implementation of `fs::Fs` that forwards every operation over a
/// WebSocket JSON-RPC channel to a backend `zed-server`.
pub struct RemoteFs {
    client: RemoteClient,
    executor: BackgroundExecutor,
    watch_subscriptions: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Vec<PathEvent>>>>>,
    prefetched_metadata: Arc<Mutex<HashMap<std::path::PathBuf, MetadataResponse>>>,
}

impl RemoteFs {
    pub fn new(client: RemoteClient, executor: BackgroundExecutor) -> Self {
        let subscriptions: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Vec<PathEvent>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let prefetched_metadata = Arc::new(Mutex::new(HashMap::new()));

        // Register a single notification handler that routes remote file-system
        // events to the active watch subscriptions.
        let subs_for_handler = subscriptions.clone();
        let metadata_for_handler = prefetched_metadata.clone();
        client.on_notification("Fs::watch_event", move |params| {
            let Ok(payload) = serde_json::from_value::<WatchNotification>(params) else {
                return;
            };
            lock_shared(&metadata_for_handler).clear();
            let events: Vec<PathEvent> = payload
                .events
                .into_iter()
                .filter_map(|event| {
                    let kind = match event.kind.as_deref() {
                        Some("created") => Some(PathEventKind::Created),
                        Some("changed") => Some(PathEventKind::Changed),
                        Some("removed") => Some(PathEventKind::Removed),
                        _ => None,
                    };
                    Some(PathEvent {
                        path: std::path::PathBuf::from(event.path),
                        kind,
                    })
                })
                .collect();
            if let Some(tx) = lock_shared(&subs_for_handler).get(&payload.subscription_id) {
                tx.unbounded_send(events).ok();
            }
        });

        let mut reconnects = client.subscribe_reconnect();
        let reconnect_client = client.clone();
        let reconnect_subscriptions = subscriptions.clone();
        executor
            .spawn(async move {
                while reconnects.next().await.is_some() {
                    let subscription_ids = lock_shared(&reconnect_subscriptions)
                        .keys()
                        .copied()
                        .collect::<Vec<_>>();
                    if subscription_ids.is_empty() {
                        continue;
                    }
                    reconnect_client
                        .call_void(
                            "Fs::attach_watches",
                            &json!({ "subscription_ids": subscription_ids }),
                        )
                        .await
                        .ok();
                }
            })
            .detach();

        Self {
            client,
            executor,
            watch_subscriptions: subscriptions,
            prefetched_metadata,
        }
    }
}

#[derive(Clone, Deserialize)]
struct MetadataResponse {
    inode: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
    is_symlink: bool,
    is_dir: bool,
    len: u64,
    is_fifo: bool,
    is_executable: bool,
    is_writable: bool,
}

#[derive(Deserialize)]
struct ReadDirResponse {
    entries: Vec<String>,
    #[serde(default)]
    metadata: HashMap<String, MetadataResponse>,
}

#[derive(Deserialize)]
struct ReadDirWithTypesResponse {
    entries: Vec<ReadDirEntryResponse>,
}

#[derive(Deserialize)]
struct ReadDirEntryResponse {
    path: String,
    is_dir: bool,
}

#[derive(Deserialize)]
struct RestoreResponse {
    ok: bool,
    path: Option<String>,
    error: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct WatchNotification {
    subscription_id: u64,
    events: Vec<WatchEvent>,
}

#[derive(Deserialize)]
struct WatchEvent {
    path: String,
    kind: Option<String>,
}

#[derive(Debug)]
struct RemoteFileHandle {
    path: std::path::PathBuf,
}

impl FileHandle for RemoteFileHandle {
    fn current_path(&self, _fs: &Arc<dyn Fs>) -> Result<std::path::PathBuf> {
        Ok(self.path.clone())
    }
}

#[derive(Deserialize)]
struct WatchResponse {
    subscription_id: u64,
}

struct RemoteWatcher {
    subscriptions: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Vec<PathEvent>>>>>,
    subscription_id: u64,
    client: RemoteClient,
    executor: BackgroundExecutor,
}

impl Watcher for RemoteWatcher {
    fn add(&self, _path: &std::path::Path) -> Result<()> {
        Ok(())
    }

    fn remove(&self, _path: &std::path::Path) -> Result<()> {
        Ok(())
    }
}

impl Drop for RemoteWatcher {
    fn drop(&mut self) {
        lock_shared(&self.subscriptions).remove(&self.subscription_id);
        let client = self.client.clone();
        let subscription_id = self.subscription_id;
        self.executor
            .spawn(async move {
                client
                    .call_void(
                        "Fs::unwatch",
                        &json!({ "subscription_id": subscription_id }),
                    )
                    .await
                    .ok();
            })
            .detach();
    }
}

fn path_arg(path: &std::path::Path) -> String {
    path.to_string_lossy().to_string()
}

fn decode_base64(text: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|e| anyhow!("base64 decode error: {}", e))
}

fn metadata_from_response(metadata: MetadataResponse) -> Metadata {
    Metadata {
        inode: metadata.inode,
        mtime: MTime::from_seconds_and_nanos(metadata.mtime_secs, metadata.mtime_nanos),
        is_symlink: metadata.is_symlink,
        is_dir: metadata.is_dir,
        len: metadata.len,
        is_fifo: metadata.is_fifo,
        is_executable: metadata.is_executable,
        is_writable: metadata.is_writable,
    }
}

#[async_trait]
impl Fs for RemoteFs {
    async fn create_dir(&self, path: &std::path::Path) -> Result<()> {
        self.client
            .call_void("Fs::create_dir", &json!({ "path": path_arg(path) }))
            .await
    }

    async fn create_symlink(
        &self,
        path: &std::path::Path,
        target: std::path::PathBuf,
    ) -> Result<()> {
        self.client
            .call_void(
                "Fs::create_symlink",
                &json!({
                    "path": path_arg(path),
                    "target": target.to_string_lossy(),
                }),
            )
            .await
    }

    async fn create_file(&self, path: &std::path::Path, options: CreateOptions) -> Result<()> {
        self.client
            .call_void(
                "Fs::create_file",
                &json!({
                    "path": path_arg(path),
                    "overwrite": options.overwrite,
                    "ignore_if_exists": options.ignore_if_exists,
                }),
            )
            .await
    }

    async fn create_file_with(
        &self,
        path: &std::path::Path,
        mut content: Pin<&mut (dyn futures::io::AsyncRead + Send)>,
    ) -> Result<()> {
        let mut bytes = Vec::new();
        content.read_to_end(&mut bytes).await?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        self.client
            .call_void(
                "Fs::create_file_with",
                &json!({
                    "path": path_arg(path),
                    "content": encoded,
                }),
            )
            .await
    }

    async fn copy_file(
        &self,
        source: &std::path::Path,
        target: &std::path::Path,
        options: CopyOptions,
    ) -> Result<()> {
        self.client
            .call_void(
                "Fs::copy_file",
                &json!({
                    "source": path_arg(source),
                    "target": path_arg(target),
                    "overwrite": options.overwrite,
                    "ignore_if_exists": options.ignore_if_exists,
                }),
            )
            .await
    }

    async fn rename(
        &self,
        source: &std::path::Path,
        target: &std::path::Path,
        options: RenameOptions,
    ) -> Result<()> {
        self.client
            .call_void(
                "Fs::rename",
                &json!({
                    "source": path_arg(source),
                    "target": path_arg(target),
                    "overwrite": options.overwrite,
                    "ignore_if_exists": options.ignore_if_exists,
                    "create_parents": options.create_parents,
                }),
            )
            .await
    }

    async fn remove_dir(&self, path: &std::path::Path, options: RemoveOptions) -> Result<()> {
        self.client
            .call_void(
                "Fs::remove_dir",
                &json!({
                    "path": path_arg(path),
                    "recursive": options.recursive,
                    "ignore_if_not_exists": options.ignore_if_not_exists,
                }),
            )
            .await
    }

    async fn trash(&self, path: &std::path::Path, options: RemoveOptions) -> Result<TrashId> {
        let trash_id: u64 = self
            .client
            .call(
                "Fs::trash",
                &json!({
                    "path": path_arg(path),
                    "recursive": options.recursive,
                    "ignore_if_not_exists": options.ignore_if_not_exists,
                }),
            )
            .await?;
        Ok(TrashId::from_proto(trash_id))
    }

    async fn remove_file(&self, path: &std::path::Path, options: RemoveOptions) -> Result<()> {
        self.client
            .call_void(
                "Fs::remove_file",
                &json!({
                    "path": path_arg(path),
                    "recursive": options.recursive,
                    "ignore_if_not_exists": options.ignore_if_not_exists,
                }),
            )
            .await
    }

    async fn open_handle(&self, path: &std::path::Path) -> Result<Arc<dyn FileHandle>> {
        let path_string: String = self
            .client
            .call("Fs::open_handle", &json!({ "path": path_arg(path) }))
            .await?;
        Ok(Arc::new(RemoteFileHandle {
            path: std::path::PathBuf::from(path_string),
        }))
    }

    async fn open_sync(&self, path: &std::path::Path) -> Result<Box<dyn io::Read + Send + Sync>> {
        let encoded: String = self
            .client
            .call("Fs::open_sync", &json!({ "path": path_arg(path) }))
            .await?;
        let bytes = decode_base64(&encoded)?;
        Ok(Box::new(io::Cursor::new(bytes)))
    }

    async fn load_bytes(&self, path: &std::path::Path) -> Result<Vec<u8>> {
        let encoded: String = self
            .client
            .call("Fs::load_bytes", &json!({ "path": path_arg(path) }))
            .await?;
        decode_base64(&encoded)
    }

    async fn atomic_write(&self, path: std::path::PathBuf, text: String) -> Result<()> {
        self.client
            .call_void(
                "Fs::atomic_write",
                &json!({
                    "path": path_arg(&path),
                    "text": text,
                }),
            )
            .await
    }

    async fn save(
        &self,
        path: &std::path::Path,
        text: &rope::Rope,
        line_ending: text::LineEnding,
    ) -> Result<()> {
        self.client
            .call_void(
                "Fs::save",
                &json!({
                    "path": path_arg(path),
                    "text": text.to_string(),
                    "line_ending": line_ending.as_str(),
                }),
            )
            .await
    }

    async fn write(&self, path: &std::path::Path, content: &[u8]) -> Result<()> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(content);
        self.client
            .call_void(
                "Fs::write",
                &json!({
                    "path": path_arg(path),
                    "content": encoded,
                }),
            )
            .await
    }

    async fn canonicalize(&self, path: &std::path::Path) -> Result<std::path::PathBuf> {
        let result: String = self
            .client
            .call("Fs::canonicalize", &json!({ "path": path_arg(path) }))
            .await?;
        Ok(std::path::PathBuf::from(result))
    }

    async fn is_file(&self, path: &std::path::Path) -> bool {
        self.client
            .call("Fs::is_file", &json!({ "path": path_arg(path) }))
            .await
            .unwrap_or(false)
    }

    async fn is_dir(&self, path: &std::path::Path) -> bool {
        self.client
            .call("Fs::is_dir", &json!({ "path": path_arg(path) }))
            .await
            .unwrap_or(false)
    }

    async fn metadata(&self, path: &std::path::Path) -> Result<Option<Metadata>> {
        let response = lock_shared(&self.prefetched_metadata).remove(path);
        let response = match response {
            Some(response) => Some(response),
            None => {
                self.client
                    .call("Fs::metadata", &json!({ "path": path_arg(path) }))
                    .await?
            }
        };
        Ok(response.map(metadata_from_response))
    }

    async fn read_link(&self, path: &std::path::Path) -> Result<std::path::PathBuf> {
        let result: String = self
            .client
            .call("Fs::read_link", &json!({ "path": path_arg(path) }))
            .await?;
        Ok(std::path::PathBuf::from(result))
    }

    async fn read_dir(
        &self,
        path: &std::path::Path,
    ) -> Result<Pin<Box<dyn Send + Stream<Item = Result<std::path::PathBuf>>>>> {
        let response: ReadDirResponse = self
            .client
            .call("Fs::read_dir", &json!({ "path": path_arg(path) }))
            .await?;
        lock_shared(&self.prefetched_metadata).extend(
            response
                .metadata
                .into_iter()
                .map(|(path, metadata)| (std::path::PathBuf::from(path), metadata)),
        );
        let entries = response
            .entries
            .into_iter()
            .map(|s| Ok(std::path::PathBuf::from(s)))
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(entries)))
    }

    async fn read_dir_with_types(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<(std::path::PathBuf, bool)>> {
        let response: ReadDirWithTypesResponse = self
            .client
            .call(
                "Fs::read_dir_with_types",
                &json!({ "path": path_arg(path) }),
            )
            .await?;
        Ok(response
            .entries
            .into_iter()
            .map(|entry| (std::path::PathBuf::from(entry.path), entry.is_dir))
            .collect())
    }

    async fn watch(
        &self,
        path: &std::path::Path,
        latency: Duration,
    ) -> (
        Pin<Box<dyn Send + Stream<Item = Vec<PathEvent>>>>,
        Arc<dyn Watcher>,
    ) {
        let response: WatchResponse = self
            .client
            .call(
                "Fs::watch",
                &json!({
                    "path": path_arg(path),
                    "latency": latency.as_secs_f64(),
                }),
            )
            .await
            .unwrap_or(WatchResponse { subscription_id: 0 });

        let (tx, rx) = mpsc::unbounded::<Vec<PathEvent>>();
        lock_shared(&self.watch_subscriptions).insert(response.subscription_id, tx);

        let subscriptions = self.watch_subscriptions.clone();
        let subscription_id = response.subscription_id;
        let stream = stream::unfold(rx, move |mut rx| async move {
            let item = rx.next().await;
            item.map(|events| (events, rx))
        });

        let watcher = Arc::new(RemoteWatcher {
            subscriptions,
            subscription_id,
            client: self.client.clone(),
            executor: self.executor.clone(),
        });

        (
            Box::pin(stream) as Pin<Box<dyn Send + Stream<Item = Vec<PathEvent>>>>,
            watcher,
        )
    }

    fn open_repo(
        &self,
        abs_dot_git: &std::path::Path,
        _system_git_binary_path: Option<&std::path::Path>,
    ) -> Result<Arc<dyn git::repository::GitRepository>> {
        Ok(Arc::new(RemoteGitRepository::new(
            self.client.clone(),
            abs_dot_git.to_path_buf(),
            self.executor.clone(),
        )))
    }

    async fn git_init(
        &self,
        abs_work_directory: &std::path::Path,
        fallback_branch_name: String,
    ) -> Result<()> {
        self.client
            .call_void(
                "Fs::git_init",
                &json!({
                    "abs_work_directory": path_arg(abs_work_directory),
                    "fallback_branch_name": fallback_branch_name,
                }),
            )
            .await
    }

    async fn git_clone(&self, abs_work_directory: &std::path::Path, repo_url: &str) -> Result<()> {
        self.client
            .call_void(
                "Fs::git_clone",
                &json!({
                    "abs_work_directory": path_arg(abs_work_directory),
                    "repo_url": repo_url,
                }),
            )
            .await
    }

    async fn git_config(
        &self,
        abs_work_directory: &std::path::Path,
        args: Vec<String>,
    ) -> Result<String> {
        self.client
            .call(
                "Fs::git_config",
                &json!({
                    "abs_work_directory": path_arg(abs_work_directory),
                    "args": args,
                }),
            )
            .await
    }

    fn is_fake(&self) -> bool {
        false
    }

    async fn is_case_sensitive(&self) -> bool {
        self.client
            .call("Fs::is_case_sensitive", &json!({}))
            .await
            .unwrap_or(false)
    }

    fn subscribe_to_jobs(&self) -> JobEventReceiver {
        let (_, rx) = futures::channel::mpsc::unbounded::<JobEvent>();
        rx
    }

    async fn restore(
        &self,
        item: TrashId,
    ) -> std::result::Result<std::path::PathBuf, TrashRestoreError> {
        let response: RestoreResponse = self
            .client
            .call("Fs::restore", &json!({ "trash_id": item.to_proto() }))
            .await
            .map_err(|error| TrashRestoreError::Unknown {
                description: error.to_string(),
            })?;

        if response.ok {
            return response.path.map(std::path::PathBuf::from).ok_or_else(|| {
                TrashRestoreError::Unknown {
                    description: "remote restore omitted the restored path".to_string(),
                }
            });
        }

        let path = response
            .path
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        match response.error.as_deref() {
            Some("not_found") => Err(TrashRestoreError::NotFound { path }),
            Some("collision") => Err(TrashRestoreError::Collision { path }),
            Some("already_restored") => Err(TrashRestoreError::AlreadyRestored),
            _ => Err(TrashRestoreError::Unknown {
                description: response
                    .description
                    .unwrap_or_else(|| "remote restore failed".to_string()),
            }),
        }
    }
}
