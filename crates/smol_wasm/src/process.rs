//! WASM remote shim for `smol::process`.
//!
//! On native targets the real `async-process` implementation is used. On WASM
//! this module forwards `Command::output` / `Command::status` / `Command::spawn`
//! to a backend server over the JSON-RPC WebSocket transport provided by
//! `wasm_rpc`. Streaming stdio for spawned processes is delivered through
//! server-initiated notifications.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io;
use std::pin::Pin;
use std::process::{ExitStatus, Output};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

pub use std::process::Stdio;

#[cfg(target_family = "wasm")]
use base64::Engine as _;
#[cfg(target_family = "wasm")]
use futures::StreamExt;
#[cfg(target_family = "wasm")]
use futures::channel::{mpsc, oneshot};
#[cfg(target_family = "wasm")]
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
#[cfg(target_family = "wasm")]
use serde::{Deserialize, Serialize};
#[cfg(target_family = "wasm")]
use wasm_rpc::RpcClient;

#[cfg(target_family = "wasm")]
thread_local! {
    static REMOTE: std::cell::RefCell<Option<RemoteState>> = std::cell::RefCell::new(None);
}

#[cfg(target_family = "wasm")]
static NEXT_PROC_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(target_family = "wasm")]
thread_local! {
    // A reload creates a new WASM instance while the RPC session and its child
    // processes remain alive on the server. Namespace IDs per page instance so
    // the new process counter cannot replace an existing agent or language server.
    static PROC_NAMESPACE: u64 = (uuid::Uuid::new_v4().as_u128() as u64) & 0xffff_ffff;
}

#[cfg(target_family = "wasm")]
fn next_proc_id() -> u64 {
    let sequence = NEXT_PROC_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst) & 0x000f_ffff;
    PROC_NAMESPACE.with(|namespace| compose_process_id(*namespace, sequence))
}

fn compose_process_id(namespace: u64, sequence: u64) -> u64 {
    ((namespace & 0xffff_ffff) << 20) | (sequence & 0x000f_ffff)
}

#[cfg(target_family = "wasm")]
struct RemoteState {
    client: RpcClient,
    router: Arc<Mutex<ProcessRouter>>,
}

#[cfg(target_family = "wasm")]
#[derive(Default)]
struct ProcessRouter {
    by_id: std::collections::HashMap<u64, Arc<Mutex<ProcessIo>>>,
}

#[cfg(target_family = "wasm")]
struct ProcessIo {
    stdout_buf: VecDeque<u8>,
    stderr_buf: VecDeque<u8>,
    stdout_waker: Option<Waker>,
    stderr_waker: Option<Waker>,
    exit_waker: Option<Waker>,
    exit_status: Option<ExitStatus>,
    exit_tx: Option<oneshot::Sender<ExitStatus>>,
    stdin_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

#[cfg(target_family = "wasm")]
impl ProcessIo {
    fn new() -> Self {
        Self {
            stdout_buf: VecDeque::new(),
            stderr_buf: VecDeque::new(),
            stdout_waker: None,
            stderr_waker: None,
            exit_waker: None,
            exit_status: None,
            exit_tx: None,
            stdin_tx: None,
        }
    }
}

/// Set the global RPC client used by the remote process shim.
///
/// This should be called once during app initialization, before any code
/// spawns a subprocess.
#[cfg(target_family = "wasm")]
pub fn set_remote_client(client: RpcClient) {
    let router: Arc<Mutex<ProcessRouter>> = Arc::new(Mutex::new(ProcessRouter::default()));

    let router_stdout = router.clone();
    client.on_notification("Process::stdout", move |params| {
        let Ok(payload) = serde_json::from_value::<StdoutNotification>(params) else {
            return;
        };
        if let Some(io) = router_stdout.lock().unwrap().by_id.get(&payload.proc_id) {
            let mut io = io.lock().unwrap();
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&payload.data) {
                io.stdout_buf.extend(bytes);
                if let Some(waker) = io.stdout_waker.take() {
                    waker.wake();
                }
            }
        }
    });

    let router_stderr = router.clone();
    client.on_notification("Process::stderr", move |params| {
        let Ok(payload) = serde_json::from_value::<StderrNotification>(params) else {
            return;
        };
        if let Some(io) = router_stderr.lock().unwrap().by_id.get(&payload.proc_id) {
            let mut io = io.lock().unwrap();
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&payload.data) {
                io.stderr_buf.extend(bytes);
                if let Some(waker) = io.stderr_waker.take() {
                    waker.wake();
                }
            }
        }
    });

    let router_exit = router.clone();
    client.on_notification("Process::exit", move |params| {
        let Ok(payload) = serde_json::from_value::<ExitNotification>(params) else {
            return;
        };
        if let Some(io) = router_exit.lock().unwrap().by_id.get(&payload.proc_id) {
            let mut io = io.lock().unwrap();
            let status = ExitStatus::default();
            io.exit_status = Some(status);
            if let Some(tx) = io.exit_tx.take() {
                tx.send(status).ok();
            }
            if let Some(waker) = io.exit_waker.take() {
                waker.wake();
            }
            if let Some(waker) = io.stdout_waker.take() {
                waker.wake();
            }
            if let Some(waker) = io.stderr_waker.take() {
                waker.wake();
            }
        }
    });

    let mut reconnects = client.subscribe_reconnect();
    let reconnect_client = client.clone();
    let reconnect_router = router.clone();
    wasm_bindgen_futures::spawn_local(async move {
        while reconnects.next().await.is_some() {
            let proc_ids = reconnect_router
                .lock()
                .unwrap()
                .by_id
                .keys()
                .copied()
                .collect::<Vec<_>>();
            if proc_ids.is_empty() {
                continue;
            }
            let response = reconnect_client
                .call::<_, AttachProcessesResponse>(
                    "Process::attach",
                    &AttachProcessesRequest { proc_ids },
                )
                .await;
            let Ok(response) = response else {
                continue;
            };
            for proc_id in response.missing {
                if let Some(io) = reconnect_router.lock().unwrap().by_id.get(&proc_id) {
                    mark_process_exited(&mut io.lock().unwrap());
                }
            }
        }
    });

    REMOTE.with(|remote| {
        *remote.borrow_mut() = Some(RemoteState { client, router });
    });
}

#[cfg(target_family = "wasm")]
fn mark_process_exited(io: &mut ProcessIo) {
    let status = ExitStatus::default();
    io.exit_status = Some(status);
    if let Some(tx) = io.exit_tx.take() {
        tx.send(status).ok();
    }
    if let Some(waker) = io.exit_waker.take() {
        waker.wake();
    }
    if let Some(waker) = io.stdout_waker.take() {
        waker.wake();
    }
    if let Some(waker) = io.stderr_waker.take() {
        waker.wake();
    }
}

#[cfg(target_family = "wasm")]
fn remote_state() -> io::Result<(RpcClient, Arc<Mutex<ProcessRouter>>)> {
    REMOTE.with(|remote| {
        remote
            .borrow()
            .as_ref()
            .map(|s| (s.client.clone(), s.router.clone()))
            .ok_or_else(|| io_error("remote RPC client not initialized"))
    })
}

/// Shared RPC client for other smol WASM shims (fs, net).
#[cfg(target_family = "wasm")]
pub(crate) fn remote_rpc_client() -> io::Result<RpcClient> {
    remote_state().map(|(client, _)| client)
}

/// Checks the host-side process registry for an ACP prompt that outlived the
/// browser instance which submitted it.
#[cfg(target_family = "wasm")]
pub async fn remote_session_running(session_id: &str) -> io::Result<bool> {
    #[derive(serde::Deserialize)]
    struct RunningSessions {
        sessions: Vec<String>,
    }

    let (client, _) = remote_state()?;
    let response: RunningSessions = client
        .call("Process::running_sessions", &serde_json::json!({}))
        .await
        .map_err(|error| io_error(&error.to_string()))?;
    Ok(response
        .sessions
        .iter()
        .any(|running| running == session_id))
}

fn io_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, message)
}

fn unsupported<T>(message: &str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Unsupported, message))
}

#[derive(Debug)]
pub struct Command {
    program: std::ffi::OsString,
    args: Vec<std::ffi::OsString>,
    env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    cwd: Option<std::path::PathBuf>,
    stdin_cfg: Option<Stdio>,
    stdout_cfg: Option<Stdio>,
    stderr_cfg: Option<Stdio>,
    kill_on_drop: bool,
}

impl Command {
    pub fn new<S: AsRef<OsStr>>(program: S) -> Command {
        Command {
            program: program.as_ref().to_os_string(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            stdin_cfg: None,
            stdout_cfg: None,
            stderr_cfg: None,
            kill_on_drop: false,
        }
    }

    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Command {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.args.push(arg.as_ref().to_os_string());
        }
        self
    }

    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Command
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.env
            .push((key.as_ref().to_os_string(), val.as_ref().to_os_string()));
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Command
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, val) in vars {
            self.env
                .push((key.as_ref().to_os_string(), val.as_ref().to_os_string()));
        }
        self
    }

    pub fn current_dir<P: AsRef<std::path::Path>>(&mut self, dir: P) -> &mut Command {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Command {
        self.env.retain(|(k, _)| k != key.as_ref());
        self
    }

    pub fn env_clear(&mut self) -> &mut Command {
        self.env.clear();
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(|arg| arg.as_os_str())
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn stdin<T: Into<std::process::Stdio>>(&mut self, cfg: T) -> &mut Command {
        self.stdin_cfg = Some(cfg.into());
        self
    }

    pub fn stdout<T: Into<std::process::Stdio>>(&mut self, cfg: T) -> &mut Command {
        self.stdout_cfg = Some(cfg.into());
        self
    }

    pub fn stderr<T: Into<std::process::Stdio>>(&mut self, cfg: T) -> &mut Command {
        self.stderr_cfg = Some(cfg.into());
        self
    }

    pub fn kill_on_drop(&mut self, kill: bool) -> &mut Command {
        self.kill_on_drop = kill;
        self
    }

    /// Spawn a child process on the remote host and proxy its stdio through
    /// server-initiated notifications.
    pub fn spawn(&mut self) -> io::Result<Child> {
        #[cfg(target_family = "wasm")]
        {
            let (client, router) = remote_state()?;
            let proc_id = next_proc_id();
            let request = SpawnRequest {
                proc_id,
                program: self.program.to_string_lossy().to_string(),
                args: self
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().to_string())
                    .collect(),
                env: self
                    .env
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().to_string(),
                            v.to_string_lossy().to_string(),
                        )
                    })
                    .collect(),
                cwd: self.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
                stdin_pipe: self.stdin_cfg.is_some(),
                stdout_pipe: self.stdout_cfg.is_some(),
                stderr_pipe: self.stderr_cfg.is_some(),
            };

            let io = Arc::new(Mutex::new(ProcessIo::new()));
            router.lock().unwrap().by_id.insert(proc_id, io.clone());

            let (spawn_ready_tx, spawn_ready_rx) = oneshot::channel();

            // Send the spawn request without blocking. If the server fails to
            // spawn, the exit notification will mark the process as done.
            let client_for_spawn = client.clone();
            let router_for_spawn = router.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result: Result<SpawnResponse, _> =
                    client_for_spawn.call("Process::spawn", &request).await;
                let _ = spawn_ready_tx.send(result.is_ok());
                if result.is_err() {
                    if let Some(io) = router_for_spawn.lock().unwrap().by_id.get(&proc_id) {
                        let mut guard = io.lock().unwrap();
                        guard.exit_status = Some(ExitStatus::default());
                        if let Some(tx) = guard.exit_tx.take() {
                            tx.send(ExitStatus::default()).ok();
                        }
                        if let Some(waker) = guard.exit_waker.take() {
                            waker.wake();
                        }
                        if let Some(waker) = guard.stdout_waker.take() {
                            waker.wake();
                        }
                        if let Some(waker) = guard.stderr_waker.take() {
                            waker.wake();
                        }
                    }
                }
            });

            let stdin = if self.stdin_cfg.is_some() {
                let (tx, mut rx) = mpsc::unbounded::<Vec<u8>>();
                io.lock().unwrap().stdin_tx = Some(tx.clone());
                let client_for_stdin = client.clone();
                let proc_id_for_stdin = proc_id;
                wasm_bindgen_futures::spawn_local(async move {
                    if !spawn_ready_rx.await.unwrap_or(false) {
                        return;
                    }
                    while let Some(chunk) = rx.next().await {
                        let _ = client_for_stdin
                            .call_void(
                                "Process::write_stdin",
                                &StdinRequest {
                                    proc_id: proc_id_for_stdin,
                                    data: base64::engine::general_purpose::STANDARD.encode(&chunk),
                                },
                            )
                            .await;
                    }
                    let _ = client_for_stdin
                        .call_void(
                            "Process::close_stdin",
                            &ProcIdRequest {
                                proc_id: proc_id_for_stdin,
                            },
                        )
                        .await;
                });
                Some(ChildStdin { tx: Some(tx) })
            } else {
                None
            };

            let stdout = if self.stdout_cfg.is_some() {
                Some(ChildStdout { io: io.clone() })
            } else {
                None
            };

            let stderr = if self.stderr_cfg.is_some() {
                Some(ChildStderr { io: io.clone() })
            } else {
                None
            };

            Ok(Child {
                io,
                proc_id,
                client,
                stdin,
                stdout,
                stderr,
                kill_on_drop: self.kill_on_drop,
            })
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = self;
            unsupported("spawn() called on native target through wasm shim")
        }
    }

    /// Run the command to completion on the remote server and return its
    /// captured output.
    pub async fn output(&mut self) -> io::Result<Output> {
        #[cfg(target_family = "wasm")]
        {
            let (client, _) = remote_state()?;
            let request = OutputRequest {
                program: self.program.to_string_lossy().to_string(),
                args: self
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().to_string())
                    .collect(),
                env: self
                    .env
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().to_string(),
                            v.to_string_lossy().to_string(),
                        )
                    })
                    .collect(),
                cwd: self.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
                stdin_pipe: self.stdin_cfg.is_some(),
                stdout_pipe: self.stdout_cfg.is_some(),
                stderr_pipe: self.stderr_cfg.is_some(),
            };
            let response: OutputResponse = client
                .call("Process::output", &request)
                .await
                .map_err(|e| io_error(&e.to_string()))?;

            let stdout = base64::engine::general_purpose::STANDARD
                .decode(response.stdout)
                .map_err(|e| io_error(&e.to_string()))?;
            let stderr = base64::engine::general_purpose::STANDARD
                .decode(response.stderr)
                .map_err(|e| io_error(&e.to_string()))?;

            Ok(Output {
                status: ExitStatus::default(),
                stdout,
                stderr,
            })
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = self;
            unsupported("output() called on native target through wasm shim")
        }
    }

    /// Run the command to completion on the remote server and return its exit
    /// status.
    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        #[cfg(target_family = "wasm")]
        {
            let (client, _) = remote_state()?;
            let request = StatusRequest {
                program: self.program.to_string_lossy().to_string(),
                args: self
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().to_string())
                    .collect(),
                env: self
                    .env
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().to_string(),
                            v.to_string_lossy().to_string(),
                        )
                    })
                    .collect(),
                cwd: self.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
            };

            let _response: StatusResponse = client
                .call("Process::status", &request)
                .await
                .map_err(|e| io_error(&e.to_string()))?;

            Ok(ExitStatus::default())
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = self;
            unsupported("status() called on native target through wasm shim")
        }
    }
}

pub struct Child {
    io: Arc<Mutex<ProcessIo>>,
    proc_id: u64,
    client: RpcClient,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    kill_on_drop: bool,
}

impl Child {
    pub fn status(&mut self) -> impl std::future::Future<Output = io::Result<ExitStatus>> + Send {
        let io = self.io.clone();
        async move {
            {
                let guard = io.lock().unwrap();
                if guard.exit_status.is_some() {
                    return Ok(ExitStatus::default());
                }
            }
            let (tx, rx) = oneshot::channel();
            {
                let mut guard = io.lock().unwrap();
                guard.exit_tx = Some(tx);
            }
            rx.await.ok();
            Ok(ExitStatus::default())
        }
    }

    pub fn output(mut self) -> impl std::future::Future<Output = io::Result<Output>> + Send {
        async move {
            let mut stdout = Vec::new();
            if let Some(mut s) = self.stdout.take() {
                s.read_to_end(&mut stdout).await?;
            }
            let mut stderr = Vec::new();
            if let Some(mut s) = self.stderr.take() {
                s.read_to_end(&mut stderr).await?;
            }
            self.status().await.ok();
            Ok(Output {
                status: ExitStatus::default(),
                stdout,
                stderr,
            })
        }
    }

    pub fn try_status(&mut self) -> io::Result<Option<ExitStatus>> {
        let guard = self.io.lock().unwrap();
        Ok(guard.exit_status)
    }

    pub fn stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    pub fn stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    pub fn stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        #[cfg(target_family = "wasm")]
        {
            let client = self.client.clone();
            let proc_id = self.proc_id;
            wasm_bindgen_futures::spawn_local(async move {
                let _ = client
                    .call_void("Process::kill", &ProcIdRequest { proc_id })
                    .await;
            });
            Ok(())
        }
        #[cfg(not(target_family = "wasm"))]
        {
            unsupported("kill() called on native target through wasm shim")
        }
    }

    pub fn id(&self) -> u32 {
        self.proc_id as u32
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop {
            let _ = self.kill();
        }
    }
}

pub struct ChildStdin {
    tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

pub struct ChildStdout {
    io: Arc<Mutex<ProcessIo>>,
}

pub struct ChildStderr {
    io: Arc<Mutex<ProcessIo>>,
}

impl AsyncWrite for ChildStdin {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if let Some(tx) = self.tx.as_ref() {
            tx.unbounded_send(buf.to_vec())
                .map_err(|_| io_error("stdin channel closed"))?;
            Poll::Ready(Ok(buf.len()))
        } else {
            Poll::Ready(Err(io_error("stdin not available")))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.tx.take();
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for ChildStdout {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut io = self.io.lock().unwrap();
        if io.stdout_buf.is_empty() {
            if io.exit_status.is_some() {
                return Poll::Ready(Ok(0));
            }
            io.stdout_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let len = io.stdout_buf.len().min(buf.len());
        let bytes: Vec<u8> = io.stdout_buf.drain(..len).collect();
        buf[..len].copy_from_slice(&bytes);
        Poll::Ready(Ok(len))
    }
}

impl AsyncRead for ChildStderr {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut io = self.io.lock().unwrap();
        if io.stderr_buf.is_empty() {
            if io.exit_status.is_some() {
                return Poll::Ready(Ok(0));
            }
            io.stderr_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let len = io.stderr_buf.len().min(buf.len());
        let bytes: Vec<u8> = io.stderr_buf.drain(..len).collect();
        buf[..len].copy_from_slice(&bytes);
        Poll::Ready(Ok(len))
    }
}

#[cfg(target_family = "wasm")]
#[derive(Serialize)]
struct SpawnRequest {
    proc_id: u64,
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
    stdin_pipe: bool,
    stdout_pipe: bool,
    stderr_pipe: bool,
}

#[cfg(target_family = "wasm")]
#[derive(Serialize)]
struct AttachProcessesRequest {
    proc_ids: Vec<u64>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct AttachProcessesResponse {
    missing: Vec<u64>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct SpawnResponse {
    proc_id: u64,
}

#[cfg(target_family = "wasm")]
#[derive(Serialize)]
struct StdinRequest {
    proc_id: u64,
    data: String,
}

#[cfg(target_family = "wasm")]
#[derive(Serialize)]
struct ProcIdRequest {
    proc_id: u64,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct StdoutNotification {
    proc_id: u64,
    data: String,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct StderrNotification {
    proc_id: u64,
    data: String,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
struct ExitNotification {
    proc_id: u64,
    #[allow(dead_code)]
    status: i32,
}

#[cfg(target_family = "wasm")]
#[derive(Serialize)]
#[allow(dead_code)]
struct OutputRequest {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
    stdin_pipe: bool,
    stdout_pipe: bool,
    stderr_pipe: bool,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
#[allow(dead_code)]
struct OutputResponse {
    status_code: i32,
    stdout: String,
    stderr: String,
}

#[cfg(target_family = "wasm")]
#[derive(Serialize)]
#[allow(dead_code)]
struct StatusRequest {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
}

#[cfg(target_family = "wasm")]
#[derive(Deserialize)]
#[allow(dead_code)]
struct StatusResponse {
    status_code: i32,
}

#[cfg(test)]
mod tests {
    use super::compose_process_id;

    #[test]
    fn process_ids_are_page_namespaced_and_javascript_safe() {
        let first_page = compose_process_id(0x1234_5678, 1);
        let reloaded_page = compose_process_id(0x8765_4321, 1);

        assert_ne!(first_page, reloaded_page);
        assert!(first_page <= (1_u64 << 53) - 1);
        assert!(reloaded_page <= (1_u64 << 53) - 1);
    }
}
