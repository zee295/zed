use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex as StdMutex},
};

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result, bail};
use axum::extract::ws::Message;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    process::{ChildStdin, Command as AsyncCommand},
    sync::{Mutex, mpsc},
    task::JoinHandle,
};

use crate::fs_rpc::FsRpc;

const PROCESS_KIND_ENV: &str = "ZED_WEB_PROCESS_KIND";
const PROCESS_IDENTITY_ENV: &str = "ZED_WEB_PROCESS_IDENTITY";
const MAX_DEFERRED_ACP_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ACP_OUTPUT_BATCH_BYTES: usize = 128 * 1024;
const ACP_OUTPUT_BATCH_DELAY: std::time::Duration = std::time::Duration::from_millis(4);

pub fn handles(method: &str) -> bool {
    matches!(method, "Process::output" | "Process::status")
}

pub fn handles_streaming(method: &str) -> bool {
    matches!(
        method,
        "Process::spawn"
            | "Process::write_stdin"
            | "Process::close_stdin"
            | "Process::kill"
            | "Process::attach"
            | "Process::running_sessions"
    )
}

pub struct ProcessManager {
    fs: Arc<FsRpc>,
    outgoing: mpsc::UnboundedSender<Message>,
    processes: HashMap<u64, ProcessEntry>,
    #[cfg(unix)]
    open_url_bridge: Option<OpenUrlBridge>,
}

struct ProcessEntry {
    owner_generation: u64,
    disconnected: bool,
    kind: ProcessKind,
    identity: Option<ProcessIdentity>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    stdin_rewriter: Mutex<FrameRewriter>,
    acp_activity: Arc<StdMutex<AcpActivity>>,
    sanitize_lldb: bool,
    #[cfg(target_os = "linux")]
    process_group_id: Option<u32>,
    #[cfg(target_os = "linux")]
    process_group_active: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessKind {
    AcpAgent,
    LanguageServer,
    McpServer,
    Generic,
}

impl ProcessKind {
    fn from_environment(environment: &mut HashMap<String, String>) -> Self {
        match environment.remove(PROCESS_KIND_ENV).as_deref() {
            Some("acp-agent") => Self::AcpAgent,
            Some("language-server") => Self::LanguageServer,
            Some("mcp-server") => Self::McpServer,
            _ => Self::Generic,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProcessIdentity {
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    logical_id: Option<String>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct OrphanedProcessReap {
    pub language_servers: usize,
    pub mcp_servers: usize,
    pub mcp_waiting_for_agent: bool,
}

#[derive(Default)]
struct AcpActivity {
    input: Vec<u8>,
    output: Vec<u8>,
    prompts: HashMap<String, String>,
    suspended_sessions: HashSet<String>,
    deferred_output: VecDeque<(String, Vec<u8>)>,
    deferred_output_bytes: usize,
}

impl AcpActivity {
    fn feed_input(&mut self, bytes: &[u8]) {
        self.input.extend_from_slice(bytes);
        for line in take_lines(&mut self.input) {
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            let method = value
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let session_id = value
                .pointer("/params/sessionId")
                .or_else(|| value.pointer("/params/session_id"))
                .and_then(Value::as_str);
            match method {
                "session/prompt" | "session.prompt" => {
                    let Some(id) = value.get("id").map(Value::to_string) else {
                        continue;
                    };
                    if let Some(session_id) = session_id {
                        self.prompts.insert(id, session_id.to_string());
                    }
                }
                "session/load" | "session.load" | "session/resume" | "session.resume" => {
                    if let Some(session_id) = session_id {
                        self.resume_session_from_history(session_id);
                    }
                }
                _ => {}
            }
        }
    }

    fn route_output(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.output.extend_from_slice(bytes);
        let mut forwarded = Vec::new();
        for line in take_lines(&mut self.output) {
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                forwarded.push(with_newline(line));
                continue;
            };
            if value.get("result").is_some() || value.get("error").is_some() {
                if let Some(id) = value.get("id").map(Value::to_string) {
                    self.prompts.remove(&id);
                }
            }

            let suspended_session = value
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|method| method == "session/update" || method == "session.update")
                .then(|| {
                    value
                        .pointer("/params/sessionId")
                        .or_else(|| value.pointer("/params/session_id"))
                        .and_then(Value::as_str)
                })
                .flatten()
                .filter(|session_id| self.suspended_sessions.contains(*session_id));
            if let Some(session_id) = suspended_session {
                self.defer_output(session_id.to_string(), with_newline(line));
            } else {
                forwarded.push(with_newline(line));
            }
        }
        forwarded
    }

    fn suspend_active_sessions(&mut self) {
        self.suspended_sessions
            .extend(self.prompts.values().cloned());
    }

    fn defer_output(&mut self, session_id: String, frame: Vec<u8>) {
        self.deferred_output_bytes += frame.len();
        self.deferred_output.push_back((session_id, frame));
        while self.deferred_output_bytes > MAX_DEFERRED_ACP_OUTPUT_BYTES {
            let Some((_, frame)) = self.deferred_output.pop_front() else {
                break;
            };
            self.deferred_output_bytes = self.deferred_output_bytes.saturating_sub(frame.len());
        }
    }

    fn resume_session_from_history(&mut self, session_id: &str) {
        // A fresh browser registers the thread before sending session/load. ACP
        // then replays its history, so forwarding these transient copies would
        // duplicate content that the load response is about to reconstruct.
        self.suspended_sessions.remove(session_id);
        let mut removed_bytes = 0;
        self.deferred_output.retain(|(deferred_session, frame)| {
            if deferred_session == session_id {
                removed_bytes += frame.len();
                false
            } else {
                true
            }
        });
        self.deferred_output_bytes = self.deferred_output_bytes.saturating_sub(removed_bytes);
    }

    fn resume_existing_client(&mut self) -> Vec<Vec<u8>> {
        // A transport reconnect keeps the existing WASM router and ACP thread,
        // so it needs the short outage buffer instead of a history reload.
        self.suspended_sessions.clear();
        self.deferred_output_bytes = 0;
        self.deferred_output
            .drain(..)
            .map(|(_, frame)| frame)
            .collect()
    }
}

fn with_newline(mut line: Vec<u8>) -> Vec<u8> {
    line.push(b'\n');
    line
}

fn take_lines(buffer: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in buffer.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(buffer[start..index].to_vec());
            start = index + 1;
        }
    }
    if start > 0 {
        buffer.drain(..start);
    }
    lines
}

impl ProcessManager {
    pub fn new(fs: Arc<FsRpc>, outgoing: mpsc::UnboundedSender<Message>) -> Self {
        #[cfg(unix)]
        let open_url_bridge = OpenUrlBridge::new(outgoing.clone())
            .map_err(|error| tracing::warn!(?error, "failed to initialize browser URL bridge"))
            .ok();
        Self {
            fs,
            outgoing,
            processes: HashMap::new(),
            #[cfg(unix)]
            open_url_bridge,
        }
    }

    pub async fn dispatch(
        &mut self,
        method: &str,
        params: &Value,
        owner_generation: u64,
    ) -> Result<Value> {
        match method {
            "Process::spawn" => self.spawn(params, owner_generation).await,
            "Process::write_stdin" => self.write_stdin(params).await,
            "Process::close_stdin" => self.close_stdin(params).await,
            "Process::kill" => self.kill(params),
            "Process::attach" => self.attach(params, owner_generation),
            "Process::running_sessions" => self.running_sessions(),
            _ => bail!("unknown streaming process method: {method}"),
        }
    }

    async fn spawn(&mut self, params: &Value, owner_generation: u64) -> Result<Value> {
        let proc_id = params
            .get("proc_id")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("missing process id"))?;
        let program = params
            .get("program")
            .and_then(Value::as_str)
            .filter(|program| !program.is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing process program"))?;
        let root = self.fs.path("/workspace")?;
        let rewrite_fs = self.fs.clone();
        let rewrite = move |value: &str| rewrite_process_value(&rewrite_fs, value);
        let program = rewrite(program);
        let raw_args = params
            .get("args")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(&rewrite)
            .collect::<Vec<_>>();
        let (program, args) = crate::debug_adapter::resolve(&program, raw_args)?;
        let sanitize_lldb = program
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("lldb-dap"));
        let mut process_environment = environment(params);
        let kind = ProcessKind::from_environment(&mut process_environment);
        let logical_id = process_environment.remove(PROCESS_IDENTITY_ENV);
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(&rewrite)
            .map(|path| self.fs.path(&path))
            .transpose()?
            .filter(|path| path.is_dir())
            .unwrap_or(root);
        let identity = (kind != ProcessKind::Generic).then(|| ProcessIdentity {
            program: program.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            logical_id,
        });
        if let Some(identity) = identity.as_ref() {
            match kind {
                ProcessKind::LanguageServer => self.replace_disconnected_language_server(identity),
                ProcessKind::AcpAgent => {
                    if let Some(proc_id) = self.reattach_disconnected_process(
                        ProcessKind::AcpAgent,
                        identity,
                        owner_generation,
                    ) {
                        return Ok(json!({"proc_id": proc_id}));
                    }
                }
                ProcessKind::McpServer => {
                    if let Some(proc_id) = self.reattach_disconnected_process(
                        ProcessKind::McpServer,
                        identity,
                        owner_generation,
                    ) {
                        return Ok(json!({"proc_id": proc_id}));
                    }
                }
                ProcessKind::Generic => {}
            }
        }

        let mut command = AsyncCommand::new(&program);
        command.kill_on_drop(true);
        command.args(args);
        command.current_dir(cwd);
        for (key, value) in &process_environment {
            command.env(key, rewrite(&value));
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.as_std_mut().process_group(0);
        }
        #[cfg(unix)]
        if let Some(bridge) = &self.open_url_bridge {
            bridge.configure_command(&mut command, process_environment.get("PATH"))?;
        }
        if bool_param(params, "stdin_pipe") {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        if bool_param(params, "stdout_pipe") {
            command.stdout(Stdio::piped());
        } else {
            command.stdout(Stdio::null());
        }
        if bool_param(params, "stderr_pipe") {
            command.stderr(Stdio::piped());
        } else {
            command.stderr(Stdio::null());
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("spawning {}", program.display()))?;
        #[cfg(target_os = "linux")]
        let process_group_id = child.id();
        #[cfg(target_os = "linux")]
        let process_group_active = Arc::new(AtomicBool::new(process_group_id.is_some()));
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let acp_activity = Arc::new(StdMutex::new(AcpActivity::default()));
        let outgoing = self.outgoing.clone();
        let stdout_task = stdout.map(|stdout| {
            tokio::spawn(pump_output(
                stdout,
                proc_id,
                "Process::stdout",
                outgoing.clone(),
                None,
                (kind == ProcessKind::AcpAgent).then(|| acp_activity.clone()),
            ))
        });
        let stderr_task = stderr.map(|stderr| {
            tokio::spawn(pump_output(
                stderr,
                proc_id,
                "Process::stderr",
                outgoing.clone(),
                None,
                None,
            ))
        });
        let completed_activity = acp_activity.clone();
        #[cfg(target_os = "linux")]
        let completed_process_group_active = process_group_active.clone();
        let task = tokio::spawn(async move {
            let status = child
                .wait()
                .await
                .ok()
                .and_then(|status| status.code())
                .unwrap_or(-1);
            #[cfg(target_os = "linux")]
            terminate_process_group(process_group_id, &completed_process_group_active);
            if let Some(task) = stdout_task {
                task.await.ok();
            }
            if let Some(task) = stderr_task {
                task.await.ok();
            }
            if let Ok(mut activity) = completed_activity.lock() {
                activity.prompts.clear();
            }
            notify(
                &outgoing,
                "Process::exit",
                json!({"proc_id": proc_id, "status": status}),
            );
        });
        if let Some(previous) = self.processes.insert(
            proc_id,
            ProcessEntry {
                owner_generation,
                disconnected: false,
                kind,
                identity,
                stdin,
                stdin_rewriter: Mutex::new(FrameRewriter::new(Vec::new(), Vec::new())),
                acp_activity,
                sanitize_lldb,
                #[cfg(target_os = "linux")]
                process_group_id,
                #[cfg(target_os = "linux")]
                process_group_active,
                task,
            },
        ) {
            terminate_process(previous);
        }
        Ok(json!({"proc_id": proc_id}))
    }

    pub fn detach_generation(&mut self, generation: u64) {
        for entry in self.processes.values_mut() {
            if entry.owner_generation == generation {
                entry.disconnected = true;
                if entry.kind == ProcessKind::AcpAgent
                    && let Ok(mut activity) = entry.acp_activity.lock()
                {
                    activity.suspend_active_sessions();
                }
            }
        }
    }

    pub fn reap_orphaned_processes(&mut self, generation: u64) -> OrphanedProcessReap {
        let language_server_ids = self
            .processes
            .iter()
            .filter_map(|(proc_id, entry)| {
                (entry.owner_generation == generation
                    && entry.disconnected
                    && entry.kind == ProcessKind::LanguageServer)
                    .then_some(*proc_id)
            })
            .collect::<Vec<_>>();
        let language_servers = language_server_ids.len();
        for proc_id in language_server_ids {
            if let Some(entry) = self.processes.remove(&proc_id) {
                terminate_process(entry);
            }
        }

        let mcp_server_ids = self
            .processes
            .iter()
            .filter_map(|(proc_id, entry)| {
                (entry.owner_generation == generation
                    && entry.disconnected
                    && entry.kind == ProcessKind::McpServer)
                    .then_some(*proc_id)
            })
            .collect::<Vec<_>>();
        let has_running_agent = self
            .processes
            .values()
            .any(|entry| entry.kind == ProcessKind::AcpAgent && !entry.task.is_finished());
        let mcp_waiting_for_agent = has_running_agent && !mcp_server_ids.is_empty();
        let mcp_servers = if has_running_agent {
            0
        } else {
            let count = mcp_server_ids.len();
            for proc_id in mcp_server_ids {
                if let Some(entry) = self.processes.remove(&proc_id) {
                    terminate_process(entry);
                }
            }
            count
        };

        OrphanedProcessReap {
            language_servers,
            mcp_servers,
            mcp_waiting_for_agent,
        }
    }

    fn replace_disconnected_language_server(&mut self, identity: &ProcessIdentity) {
        let proc_ids = self
            .processes
            .iter()
            .filter_map(|(proc_id, entry)| {
                (entry.disconnected
                    && entry.kind == ProcessKind::LanguageServer
                    && entry.identity.as_ref() == Some(identity))
                .then_some(*proc_id)
            })
            .collect::<Vec<_>>();
        for proc_id in proc_ids {
            if let Some(entry) = self.processes.remove(&proc_id) {
                tracing::info!(
                    proc_id,
                    cwd = %identity.cwd.display(),
                    "replacing orphaned language server"
                );
                terminate_process(entry);
            }
        }
    }

    fn reattach_disconnected_process(
        &mut self,
        kind: ProcessKind,
        identity: &ProcessIdentity,
        owner_generation: u64,
    ) -> Option<u64> {
        let proc_id = self.processes.iter().find_map(|(proc_id, entry)| {
            (entry.disconnected
                && !entry.task.is_finished()
                && entry.kind == kind
                && entry.identity.as_ref() == Some(identity))
            .then_some(*proc_id)
        })?;
        let entry = self.processes.get_mut(&proc_id)?;
        entry.owner_generation = owner_generation;
        entry.disconnected = false;
        tracing::info!(
            proc_id,
            ?kind,
            cwd = %identity.cwd.display(),
            "reattached disconnected process"
        );
        Some(proc_id)
    }

    fn attach(&mut self, params: &Value, owner_generation: u64) -> Result<Value> {
        let proc_ids = params
            .get("proc_ids")
            .and_then(Value::as_array)
            .context("missing process ids")?;
        let mut attached = Vec::new();
        let mut missing = Vec::new();
        for proc_id in proc_ids.iter().filter_map(Value::as_u64) {
            if let Some(entry) = self.processes.get_mut(&proc_id) {
                entry.owner_generation = owner_generation;
                entry.disconnected = false;
                if entry.kind == ProcessKind::AcpAgent
                    && let Ok(mut activity) = entry.acp_activity.lock()
                {
                    for frame in activity.resume_existing_client() {
                        notify(
                            &self.outgoing,
                            "Process::stdout",
                            json!({"proc_id": proc_id, "data": BASE64.encode(frame)}),
                        );
                    }
                }
                attached.push(proc_id);
            } else {
                missing.push(proc_id);
            }
        }
        Ok(json!({"attached": attached, "missing": missing}))
    }

    async fn write_stdin(&self, params: &Value) -> Result<Value> {
        let proc_id = process_id(params)?;
        let entry = self
            .processes
            .get(&proc_id)
            .ok_or_else(|| anyhow::anyhow!("process not found"))?;
        let data = BASE64.decode(
            params
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )?;
        if let Ok(mut activity) = entry.acp_activity.lock() {
            activity.feed_input(&data);
        }
        let mut rewriter = entry.stdin_rewriter.lock().await;
        let chunks = rewriter.feed(&data);
        let mut stdin = entry.stdin.lock().await;
        let stdin = stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("process stdin is closed"))?;
        for mut chunk in chunks {
            if entry.sanitize_lldb {
                chunk = sanitize_lldb_frame(chunk);
            }
            stdin.write_all(&chunk).await?;
        }
        Ok(Value::Null)
    }

    fn running_sessions(&self) -> Result<Value> {
        let mut sessions = self
            .processes
            .values()
            .filter_map(|entry| entry.acp_activity.lock().ok())
            .flat_map(|activity| activity.prompts.values().cloned().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        sessions.sort();
        sessions.dedup();
        Ok(json!({ "sessions": sessions }))
    }

    async fn close_stdin(&self, params: &Value) -> Result<Value> {
        let proc_id = process_id(params)?;
        if let Some(entry) = self.processes.get(&proc_id) {
            let tail = {
                let mut rewriter = entry.stdin_rewriter.lock().await;
                std::mem::replace(&mut *rewriter, FrameRewriter::new(Vec::new(), Vec::new()))
                    .flush()
            };
            if let Some(mut stdin) = entry.stdin.lock().await.take() {
                if let Some(mut tail) = tail {
                    if entry.sanitize_lldb {
                        tail = sanitize_lldb_frame(tail);
                    }
                    stdin.write_all(&tail).await.ok();
                }
                stdin.shutdown().await.ok();
            }
        }
        Ok(Value::Null)
    }

    fn kill(&mut self, params: &Value) -> Result<Value> {
        let proc_id = process_id(params)?;
        if let Some(entry) = self.processes.remove(&proc_id) {
            terminate_process(entry);
            notify(
                &self.outgoing,
                "Process::exit",
                json!({"proc_id": proc_id, "status": -1}),
            );
        }
        Ok(Value::Null)
    }
}

fn terminate_process(entry: ProcessEntry) {
    #[cfg(target_os = "linux")]
    terminate_process_group(entry.process_group_id, &entry.process_group_active);
    entry.task.abort();
}

#[cfg(target_os = "linux")]
fn terminate_process_group(process_group_id: Option<u32>, active: &AtomicBool) {
    let Some(process_group_id) = process_group_id else {
        return;
    };
    if active.swap(false, Ordering::AcqRel) {
        unsafe {
            libc::kill(-(process_group_id as i32), libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
pub(crate) struct OpenUrlBridge {
    _directory: tempfile::TempDir,
    shim_directory: PathBuf,
    socket_path: PathBuf,
    task: JoinHandle<()>,
}

#[cfg(unix)]
impl OpenUrlBridge {
    pub(crate) fn new(outgoing: mpsc::UnboundedSender<Message>) -> Result<Self> {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let shim_directory = directory.path().join("bin");
        std::fs::create_dir(&shim_directory)?;
        let executable = env::current_exe()?;
        symlink(&executable, shim_directory.join("open"))?;
        symlink(&executable, shim_directory.join("xdg-open"))?;

        let socket_path = directory.path().join("open-url.sock");
        let socket = tokio::net::UnixDatagram::bind(&socket_path)?;
        let task = tokio::spawn(async move {
            let mut buffer = vec![0_u8; 16 * 1024];
            loop {
                let Ok(count) = socket.recv(&mut buffer).await else {
                    break;
                };
                let Ok(url) = std::str::from_utf8(&buffer[..count]) else {
                    continue;
                };
                if !is_allowed_external_url(url) {
                    tracing::warn!(%url, "browser URL bridge rejected URL");
                    continue;
                }
                notify(&outgoing, "Browser::open_url", json!({"url": url}));
            }
        });

        Ok(Self {
            _directory: directory,
            shim_directory,
            socket_path,
            task,
        })
    }

    fn configure_command(
        &self,
        command: &mut AsyncCommand,
        configured_path: Option<&String>,
    ) -> Result<()> {
        let inherited_path = configured_path
            .map(std::ffi::OsString::from)
            .or_else(|| env::var_os("PATH"))
            .unwrap_or_default();
        let path = env::join_paths(
            std::iter::once(self.shim_directory.clone()).chain(env::split_paths(&inherited_path)),
        )?;
        command.env("PATH", path);
        command.env("ZED_WEB_OPEN_URL_SOCKET", &self.socket_path);
        Ok(())
    }

    pub(crate) fn configure_pty_command(
        &self,
        command: &mut portable_pty::CommandBuilder,
    ) -> Result<()> {
        let inherited_path = command
            .get_env("PATH")
            .map(std::ffi::OsString::from)
            .or_else(|| env::var_os("PATH"))
            .unwrap_or_default();
        let path = env::join_paths(
            std::iter::once(self.shim_directory.clone()).chain(env::split_paths(&inherited_path)),
        )?;
        command.env("PATH", path);
        command.env("ZED_WEB_OPEN_URL_SOCKET", &self.socket_path);
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for OpenUrlBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(unix)]
pub fn run_open_url_shim(args: &[String]) -> Result<()> {
    use std::os::unix::net::UnixDatagram;

    let url = args
        .iter()
        .rev()
        .find(|argument| is_allowed_external_url(argument))
        .context("browser opener received no supported URL")?;
    let socket_path =
        env::var_os("ZED_WEB_OPEN_URL_SOCKET").context("browser URL socket is not configured")?;
    let socket = UnixDatagram::unbound()?;
    socket.send_to(url.as_bytes(), socket_path)?;
    Ok(())
}

fn is_allowed_external_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:")
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        for (_, process) in self.processes.drain() {
            process.task.abort();
        }
    }
}

pub fn dispatch(fs: &FsRpc, method: &str, params: &Value) -> Result<Value> {
    let program = params
        .get("program")
        .and_then(Value::as_str)
        .filter(|program| !program.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing process program"))?;
    let root = fs.path("/workspace")?;
    let rewrite = |value: &str| rewrite_process_value(fs, value);
    let program = rewrite(program);
    let args = params
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(rewrite)
        .collect::<Vec<_>>();
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(rewrite)
        .map(|path| fs.path(&path))
        .transpose()?
        .unwrap_or_else(|| root.clone());
    let mut command = Command::new(&program);
    command.args(args).current_dir(&cwd);
    for (key, value) in environment(params) {
        command.env(key, rewrite(&value));
    }
    match method {
        "Process::output" => {
            let output = command
                .output()
                .with_context(|| format!("running {program} in {}", cwd.display()))?;
            Ok(json!({
                "status_code": output.status.code().unwrap_or(-1),
                "stdout": BASE64.encode(output.stdout),
                "stderr": BASE64.encode(output.stderr),
            }))
        }
        "Process::status" => {
            let status = command
                .status()
                .with_context(|| format!("running {program} in {}", cwd.display()))?;
            Ok(json!({"status_code": status.code().unwrap_or(-1)}))
        }
        _ => bail!("unknown process method: {method}"),
    }
}

fn rewrite_process_value(fs: &FsRpc, value: &str) -> String {
    const LEGACY_ROOT: &str = "/workspace";
    let physical_root = fs.rewrite_legacy_workspace_path(LEGACY_ROOT);
    if physical_root == LEGACY_ROOT || !value.contains(LEGACY_ROOT) {
        value.to_string()
    } else {
        let mut rewritten = String::with_capacity(value.len() + physical_root.len());
        let mut cursor = 0;
        while let Some(relative_index) = value[cursor..].find(LEGACY_ROOT) {
            let index = cursor + relative_index;
            let end = index + LEGACY_ROOT.len();
            let before = value[..index].chars().next_back();
            let after = value[end..].chars().next();
            let starts_path_token = before.is_none_or(|character| {
                character.is_whitespace()
                    || matches!(character, '\'' | '"' | '=' | ':' | '(' | '[' | '{')
            });
            let ends_root_token = after.is_none_or(|character| {
                character == '/'
                    || character.is_whitespace()
                    || matches!(character, '\'' | '"' | ',' | ')' | ']' | '}')
            });

            rewritten.push_str(&value[cursor..index]);
            if starts_path_token && ends_root_token {
                rewritten.push_str(&physical_root);
            } else {
                rewritten.push_str(LEGACY_ROOT);
            }
            cursor = end;
        }
        rewritten.push_str(&value[cursor..]);
        rewritten
    }
}

fn environment(params: &Value) -> HashMap<String, String> {
    match params.get("env") {
        Some(Value::Object(values)) => values
            .iter()
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
            .collect(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(|pair| {
                let pair = pair.as_array()?;
                Some((
                    pair.first()?.as_str()?.to_string(),
                    pair.get(1)?.as_str()?.to_string(),
                ))
            })
            .collect(),
        _ => HashMap::new(),
    }
}

fn bool_param(params: &Value, key: &str) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn process_id(params: &Value) -> Result<u64> {
    params
        .get("proc_id")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("missing process id"))
}

async fn pump_output(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    proc_id: u64,
    method: &'static str,
    outgoing: mpsc::UnboundedSender<Message>,
    rewrite: Option<(Vec<u8>, Vec<u8>)>,
    acp_activity: Option<Arc<StdMutex<AcpActivity>>>,
) {
    if let Some(activity) = acp_activity {
        pump_acp_output(&mut stream, proc_id, method, outgoing, activity).await;
        return;
    }

    let mut buffer = [0_u8; 4096];
    let mut rewriter = rewrite.map(|(source, target)| FrameRewriter::new(source, target));
    loop {
        let Ok(count) = stream.read(&mut buffer).await else {
            break;
        };
        if count == 0 {
            break;
        }
        let chunk = buffer[..count].to_vec();
        let chunks = rewriter
            .as_mut()
            .map(|rewriter| rewriter.feed(&chunk))
            .unwrap_or_else(|| vec![chunk]);
        for chunk in chunks {
            notify(
                &outgoing,
                method,
                json!({"proc_id": proc_id, "data": BASE64.encode(chunk)}),
            );
        }
    }
    if let Some(rewriter) = rewriter
        && let Some(chunk) = rewriter.flush()
    {
        notify(
            &outgoing,
            method,
            json!({"proc_id": proc_id, "data": BASE64.encode(chunk)}),
        );
    }
}

async fn pump_acp_output(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    proc_id: u64,
    method: &'static str,
    outgoing: mpsc::UnboundedSender<Message>,
    activity: Arc<StdMutex<AcpActivity>>,
) {
    let mut read_buffer = [0_u8; 4096];
    let mut output_batch = Vec::new();

    loop {
        let read = if output_batch.is_empty() {
            stream.read(&mut read_buffer).await
        } else {
            match tokio::time::timeout(ACP_OUTPUT_BATCH_DELAY, stream.read(&mut read_buffer)).await
            {
                Ok(read) => read,
                Err(_) => {
                    flush_acp_output(&outgoing, proc_id, method, &mut output_batch);
                    continue;
                }
            }
        };

        let Ok(count) = read else {
            break;
        };
        if count == 0 {
            break;
        }

        let routed = activity
            .lock()
            .map(|mut activity| activity.route_output(&read_buffer[..count]))
            .unwrap_or_default();
        for chunk in routed {
            output_batch.extend_from_slice(&chunk);
            if output_batch.len() >= MAX_ACP_OUTPUT_BATCH_BYTES {
                flush_acp_output(&outgoing, proc_id, method, &mut output_batch);
            }
        }
    }

    flush_acp_output(&outgoing, proc_id, method, &mut output_batch);
}

fn flush_acp_output(
    outgoing: &mpsc::UnboundedSender<Message>,
    proc_id: u64,
    method: &'static str,
    output_batch: &mut Vec<u8>,
) {
    if output_batch.is_empty() {
        return;
    }
    let data = std::mem::take(output_batch);
    notify(
        outgoing,
        method,
        json!({"proc_id": proc_id, "data": BASE64.encode(data)}),
    );
}

fn notify(outgoing: &mpsc::UnboundedSender<Message>, method: &str, params: Value) {
    outgoing
        .send(Message::Text(
            json!({"method": method, "params": params}).to_string(),
        ))
        .ok();
}

struct FrameRewriter {
    source: Vec<u8>,
    target: Vec<u8>,
    buffer: Vec<u8>,
}

impl FrameRewriter {
    fn new(source: Vec<u8>, target: Vec<u8>) -> Self {
        Self {
            source,
            target,
            buffer: Vec::new(),
        }
    }

    fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(data);
        let mut output = Vec::new();
        loop {
            let Some(separator) = find_bytes(&self.buffer, b"\r\n\r\n") else {
                if !find_bytes(&self.buffer, b"Content-Length:").is_some() {
                    output.push(replace_bytes(&self.buffer, &self.source, &self.target));
                    self.buffer.clear();
                }
                break;
            };
            let header = &self.buffer[..separator];
            let Some(length_position) = find_bytes(header, b"Content-Length:") else {
                let end = separator + 4;
                output.push(replace_bytes(
                    &self.buffer[..end],
                    &self.source,
                    &self.target,
                ));
                self.buffer.drain(..end);
                continue;
            };
            let length_text =
                String::from_utf8_lossy(&header[length_position + b"Content-Length:".len()..]);
            let Ok(length) = length_text.trim().parse::<usize>() else {
                break;
            };
            let end = separator + 4 + length;
            if self.buffer.len() < end {
                break;
            }
            let body = replace_bytes(&self.buffer[separator + 4..end], &self.source, &self.target);
            let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
            frame.extend(body);
            output.push(frame);
            self.buffer.drain(..end);
        }
        output
    }

    fn flush(self) -> Option<Vec<u8>> {
        (!self.buffer.is_empty()).then(|| replace_bytes(&self.buffer, &self.source, &self.target))
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn replace_bytes(data: &[u8], source: &[u8], target: &[u8]) -> Vec<u8> {
    if source.is_empty() {
        return data.to_vec();
    }
    let mut output = Vec::with_capacity(data.len());
    let mut offset = 0;
    while let Some(position) = find_bytes(&data[offset..], source) {
        let position = offset + position;
        output.extend_from_slice(&data[offset..position]);
        output.extend_from_slice(target);
        offset = position + source.len();
    }
    output.extend_from_slice(&data[offset..]);
    output
}

fn sanitize_lldb_frame(frame: Vec<u8>) -> Vec<u8> {
    let Some(separator) = find_bytes(&frame, b"\r\n\r\n") else {
        return frame;
    };
    let Ok(mut message) = serde_json::from_slice::<Value>(&frame[separator + 4..]) else {
        return frame;
    };
    if message.get("type").and_then(Value::as_str) != Some("request")
        || message.get("command").and_then(Value::as_str) != Some("initialize")
    {
        return frame;
    }
    let Some(arguments) = message.get_mut("arguments").and_then(Value::as_object_mut) else {
        return frame;
    };
    arguments.retain(|key, value| !(key.starts_with("supports") && value.as_bool() == Some(false)));
    let Ok(body) = serde_json::to_vec(&message) else {
        return frame;
    };
    let mut output = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    output.extend(body);
    output
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use axum::extract::ws::Message;
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use serde_json::json;
    use tokio::{io::AsyncWriteExt as _, sync::mpsc};

    use super::{
        AcpActivity, FrameRewriter, OrphanedProcessReap, ProcessManager, pump_output,
        rewrite_process_value, sanitize_lldb_frame,
    };
    use crate::fs_rpc::FsRpc;

    #[test]
    fn tracks_acp_prompt_until_response() {
        let mut activity = AcpActivity::default();
        activity.feed_input(
            br#"{"jsonrpc":"2.0","id":17,"method":"session/prompt","params":{"sessionId":"claude-session"}}"#,
        );
        activity.feed_input(b"\n");
        assert_eq!(
            activity.prompts.values().cloned().collect::<Vec<_>>(),
            ["claude-session"]
        );

        activity.route_output(br#"{"jsonrpc":"2.0","id":17,"result":{"stopReason":"end_turn"}}"#);
        activity.route_output(b"\n");
        assert!(activity.prompts.is_empty());
    }

    #[test]
    fn reload_defers_active_session_updates_until_session_load() {
        let mut activity = AcpActivity::default();
        activity.feed_input(
            br#"{"jsonrpc":"2.0","id":17,"method":"session/prompt","params":{"sessionId":"claude-session"}}
"#,
        );
        activity.suspend_active_sessions();

        let missed = activity.route_output(
            br#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"claude-session","update":{"sessionUpdate":"agent_message_chunk"}}}
"#,
        );
        assert!(missed.is_empty());
        assert_eq!(activity.deferred_output.len(), 1);

        activity.feed_input(
            br#"{"jsonrpc":"2.0","id":18,"method":"session/load","params":{"sessionId":"claude-session"}}
"#,
        );
        assert!(activity.deferred_output.is_empty());
        let resumed = activity.route_output(
            br#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"claude-session","update":{"sessionUpdate":"agent_message_chunk"}}}
"#,
        );
        assert_eq!(resumed.len(), 1);
        assert!(resumed[0].ends_with(b"\n"));
    }

    #[test]
    fn reconnect_replays_deferred_updates_to_existing_client() {
        let mut activity = AcpActivity::default();
        activity.feed_input(
            br#"{"jsonrpc":"2.0","id":17,"method":"session/prompt","params":{"sessionId":"claude-session"}}
"#,
        );
        activity.suspend_active_sessions();
        assert!(
            activity
                .route_output(
                    br#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"claude-session","update":{"sessionUpdate":"agent_message_chunk"}}}
"#,
                )
                .is_empty()
        );

        let replay = activity.resume_existing_client();
        assert_eq!(replay.len(), 1);
        assert!(activity.suspended_sessions.is_empty());
        assert!(activity.deferred_output.is_empty());

        let live = activity.route_output(
            br#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"claude-session","update":{"sessionUpdate":"agent_message_chunk"}}}
"#,
        );
        assert_eq!(live.len(), 1);
    }

    #[tokio::test]
    async fn batches_bursty_acp_output_without_changing_the_stream() -> anyhow::Result<()> {
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let (outgoing, mut notifications) = mpsc::unbounded_channel::<Message>();
        let activity = Arc::new(std::sync::Mutex::new(AcpActivity::default()));
        let proc_id = 73;

        let pump = tokio::spawn(pump_output(
            reader,
            proc_id,
            "Process::stdout",
            outgoing,
            None,
            Some(activity),
        ));
        let mut expected = Vec::new();
        for index in 0..64 {
            expected.extend(serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "claude-session",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": index.to_string() },
                    },
                },
            }))?);
            expected.push(b'\n');
        }
        writer.write_all(&expected).await?;
        let notification =
            tokio::time::timeout(std::time::Duration::from_millis(100), notifications.recv())
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("ACP output channel closed before the idle flush")
                })?;
        let Message::Text(notification) = notification else {
            anyhow::bail!("expected ACP stdout notification");
        };
        let notification: serde_json::Value = serde_json::from_str(&notification)?;
        assert_eq!(notification["method"], "Process::stdout");
        assert_eq!(notification["params"]["proc_id"], proc_id);
        let output = BASE64.decode(
            notification["params"]["data"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing ACP output data"))?,
        )?;
        assert_eq!(output, expected);

        writer.shutdown().await?;
        pump.await?;
        assert!(notifications.try_recv().is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconnect_reattaches_streaming_process_without_stopping_it() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel::<Message>();
        let mut processes = ProcessManager::new(fs, outgoing);
        let proc_id = 41;

        processes
            .dispatch(
                "Process::spawn",
                &json!({
                    "proc_id": proc_id,
                    "program": "/bin/cat",
                    "stdin_pipe": true,
                    "stdout_pipe": true,
                    "stderr_pipe": true,
                }),
                1,
            )
            .await?;
        processes.detach_generation(1);
        let detached = processes.processes.get(&proc_id).unwrap();
        assert!(detached.disconnected);
        assert!(!detached.task.is_finished());

        let attached = processes
            .dispatch(
                "Process::attach",
                &json!({"proc_ids": [proc_id, proc_id + 1]}),
                2,
            )
            .await?;
        assert_eq!(attached["attached"], json!([proc_id]));
        assert_eq!(attached["missing"], json!([proc_id + 1]));
        let reattached = processes.processes.get(&proc_id).unwrap();
        assert!(!reattached.disconnected);
        assert_eq!(reattached.owner_generation, 2);
        assert!(!reattached.task.is_finished());

        processes
            .dispatch("Process::kill", &json!({"proc_id": proc_id}), 2)
            .await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reload_replaces_disconnected_language_server() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel::<Message>();
        let mut processes = ProcessManager::new(fs, outgoing);
        let spawn = |proc_id| {
            json!({
                "proc_id": proc_id,
                "program": "/bin/cat",
                "cwd": "/workspace",
                "env": [["ZED_WEB_PROCESS_KIND", "language-server"]],
                "stdin_pipe": true,
                "stdout_pipe": true,
                "stderr_pipe": true,
            })
        };

        processes.dispatch("Process::spawn", &spawn(51), 1).await?;
        processes.detach_generation(1);
        processes.dispatch("Process::spawn", &spawn(52), 2).await?;

        assert!(!processes.processes.contains_key(&51));
        assert!(processes.processes.contains_key(&52));
        processes
            .dispatch("Process::kill", &json!({"proc_id": 52}), 2)
            .await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reload_reattaches_disconnected_agent() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel::<Message>();
        let mut processes = ProcessManager::new(fs, outgoing);
        let spawn = |proc_id| {
            json!({
                "proc_id": proc_id,
                "program": "/bin/cat",
                "cwd": "/workspace",
                "env": [["ZED_WEB_PROCESS_KIND", "acp-agent"]],
                "stdin_pipe": true,
                "stdout_pipe": true,
                "stderr_pipe": true,
            })
        };

        processes.dispatch("Process::spawn", &spawn(53), 1).await?;
        processes.detach_generation(1);
        let response = processes.dispatch("Process::spawn", &spawn(54), 2).await?;

        assert_eq!(response["proc_id"], 53);
        assert!(processes.processes.contains_key(&53));
        assert!(!processes.processes.contains_key(&54));
        let reattached = processes.processes.get(&53).unwrap();
        assert_eq!(reattached.owner_generation, 2);
        assert!(!reattached.disconnected);
        processes
            .dispatch("Process::kill", &json!({"proc_id": 53}), 2)
            .await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reload_reattaches_disconnected_mcp_server_by_id() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel::<Message>();
        let mut processes = ProcessManager::new(fs, outgoing);
        let spawn = |proc_id, server_id| {
            json!({
                "proc_id": proc_id,
                "program": "/bin/cat",
                "cwd": "/workspace",
                "env": [
                    ["ZED_WEB_PROCESS_KIND", "mcp-server"],
                    ["ZED_WEB_PROCESS_IDENTITY", server_id],
                ],
                "stdin_pipe": true,
                "stdout_pipe": true,
                "stderr_pipe": true,
            })
        };

        processes
            .dispatch("Process::spawn", &spawn(57, "filesystem"), 1)
            .await?;
        processes.detach_generation(1);
        let response = processes
            .dispatch("Process::spawn", &spawn(58, "filesystem"), 2)
            .await?;

        assert_eq!(response["proc_id"], 57);
        assert!(processes.processes.contains_key(&57));
        assert!(!processes.processes.contains_key(&58));

        let response = processes
            .dispatch("Process::spawn", &spawn(59, "another-server"), 2)
            .await?;
        assert_eq!(response["proc_id"], 59);
        assert!(processes.processes.contains_key(&59));

        processes
            .dispatch("Process::kill", &json!({"proc_id": 57}), 2)
            .await?;
        processes
            .dispatch("Process::kill", &json!({"proc_id": 59}), 2)
            .await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn orphan_reaper_keeps_mcp_until_acp_agent_stops() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel::<Message>();
        let mut processes = ProcessManager::new(fs, outgoing);

        for (proc_id, kind) in [(60, "mcp-server"), (61, "acp-agent")] {
            processes
                .dispatch(
                    "Process::spawn",
                    &json!({
                        "proc_id": proc_id,
                        "program": "/bin/cat",
                        "env": [["ZED_WEB_PROCESS_KIND", kind]],
                        "stdin_pipe": true,
                        "stdout_pipe": true,
                        "stderr_pipe": true,
                    }),
                    1,
                )
                .await?;
        }
        processes.detach_generation(1);

        assert_eq!(
            processes.reap_orphaned_processes(1),
            OrphanedProcessReap {
                language_servers: 0,
                mcp_servers: 0,
                mcp_waiting_for_agent: true,
            }
        );
        assert!(processes.processes.contains_key(&60));

        processes
            .dispatch("Process::kill", &json!({"proc_id": 61}), 1)
            .await?;
        assert_eq!(
            processes.reap_orphaned_processes(1),
            OrphanedProcessReap {
                language_servers: 0,
                mcp_servers: 1,
                mcp_waiting_for_agent: false,
            }
        );
        assert!(!processes.processes.contains_key(&60));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reload_reattaches_agent_with_active_prompt() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, mut notifications) = mpsc::unbounded_channel::<Message>();
        let mut processes = ProcessManager::new(fs, outgoing);
        let spawn = |proc_id| {
            json!({
                "proc_id": proc_id,
                "program": "/bin/cat",
                "cwd": "/workspace",
                "env": [["ZED_WEB_PROCESS_KIND", "acp-agent"]],
                "stdin_pipe": true,
                "stdout_pipe": true,
                "stderr_pipe": true,
            })
        };

        processes.dispatch("Process::spawn", &spawn(55), 1).await?;
        processes
            .processes
            .get(&55)
            .unwrap()
            .acp_activity
            .lock()
            .unwrap()
            .prompts
            .insert("request-1".into(), "session-1".into());
        processes.detach_generation(1);
        let response = processes.dispatch("Process::spawn", &spawn(56), 2).await?;

        assert_eq!(response["proc_id"], 55);
        assert!(processes.processes.contains_key(&55));
        assert!(!processes.processes.contains_key(&56));
        let reattached = processes.processes.get(&55).unwrap();
        assert_eq!(reattached.owner_generation, 2);
        assert!(!reattached.disconnected);
        assert_eq!(
            processes.running_sessions()?,
            json!({"sessions": ["session-1"]})
        );

        let frame = b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\"}\n";
        processes
            .dispatch(
                "Process::write_stdin",
                &json!({"proc_id": 55, "data": BASE64.encode(frame)}),
                2,
            )
            .await?;
        let notification =
            tokio::time::timeout(std::time::Duration::from_secs(2), notifications.recv())
                .await?
                .ok_or_else(|| anyhow::anyhow!("ACP output notification channel closed"))?;
        let Message::Text(notification) = notification else {
            anyhow::bail!("expected ACP stdout text notification");
        };
        let notification: serde_json::Value = serde_json::from_str(&notification)?;
        assert_eq!(notification["method"], "Process::stdout");
        assert_eq!(notification["params"]["proc_id"], 55);
        let output = BASE64.decode(
            notification["params"]["data"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing ACP stdout data"))?,
        )?;
        assert_eq!(output, frame);

        processes
            .dispatch("Process::kill", &json!({"proc_id": 55}), 1)
            .await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn orphan_reaper_preserves_agents_and_reconnected_language_servers() -> anyhow::Result<()>
    {
        let root = tempfile::tempdir()?;
        let fs = Arc::new(FsRpc::new(root.path().to_path_buf(), false)?);
        let (outgoing, _notifications) = mpsc::unbounded_channel::<Message>();
        let mut processes = ProcessManager::new(fs, outgoing);

        for (proc_id, kind) in [(61, "language-server"), (62, "acp-agent")] {
            processes
                .dispatch(
                    "Process::spawn",
                    &json!({
                        "proc_id": proc_id,
                        "program": "/bin/cat",
                        "env": [["ZED_WEB_PROCESS_KIND", kind]],
                        "stdin_pipe": true,
                        "stdout_pipe": true,
                        "stderr_pipe": true,
                    }),
                    1,
                )
                .await?;
        }
        processes.detach_generation(1);
        processes
            .dispatch("Process::attach", &json!({"proc_ids": [61]}), 2)
            .await?;

        assert_eq!(
            processes.reap_orphaned_processes(1),
            OrphanedProcessReap {
                language_servers: 0,
                mcp_servers: 0,
                mcp_waiting_for_agent: false,
            }
        );
        assert!(processes.processes.contains_key(&61));
        assert!(processes.processes.contains_key(&62));

        processes.detach_generation(2);
        assert_eq!(
            processes.reap_orphaned_processes(2),
            OrphanedProcessReap {
                language_servers: 1,
                mcp_servers: 0,
                mcp_waiting_for_agent: false,
            }
        );
        assert!(!processes.processes.contains_key(&61));
        assert!(processes.processes.contains_key(&62));
        processes
            .dispatch("Process::kill", &json!({"proc_id": 62}), 1)
            .await?;
        Ok(())
    }

    #[test]
    fn rewrites_virtual_paths_in_plain_agent_messages() {
        let mut rewriter = FrameRewriter::new(b"/workspace".to_vec(), b"/srv/project".to_vec());
        let chunks = rewriter.feed(br#"{"cwd":"/workspace","path":"/workspace/src"}\n"#);
        assert_eq!(
            chunks.concat(),
            br#"{"cwd":"/srv/project","path":"/srv/project/src"}\n"#
        );
    }

    #[test]
    fn rewrites_virtual_process_paths_inside_option_assignments() -> Result<()> {
        let root = tempfile::tempdir()?;
        let fs = FsRpc::new(root.path().to_path_buf(), false)?;
        let physical_root = root.path().canonicalize()?;

        assert_eq!(
            rewrite_process_value(&fs, "--cache=/workspace/.config/zed/node/cache"),
            format!("--cache={}/.config/zed/node/cache", physical_root.display())
        );
        assert_eq!(
            rewrite_process_value(&fs, "/workspace/.config/zed/node"),
            format!("{}/.config/zed/node", physical_root.display())
        );
        assert_eq!(
            rewrite_process_value(&fs, "/home/dev/web/workspace"),
            "/home/dev/web/workspace"
        );
        assert_eq!(
            rewrite_process_value(
                &fs,
                "'npm' '--prefix' '/workspace/.config/zed/external_agents/registry'"
            ),
            format!(
                "'npm' '--prefix' '{}/.config/zed/external_agents/registry'",
                physical_root.display()
            )
        );
        Ok(())
    }

    #[test]
    fn rewrites_and_reframes_lsp_messages() {
        let body = br#"{"rootUri":"file:///workspace"}"#;
        let frame = [
            format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes(),
            body,
        ]
        .concat();
        let mut rewriter = FrameRewriter::new(b"/workspace".to_vec(), b"/srv/project".to_vec());
        let output = rewriter.feed(&frame).concat();
        let separator = output
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        let declared = std::str::from_utf8(&output[16..separator])
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap();
        assert_eq!(declared, output.len() - separator - 4);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("file:///srv/project")
        );
    }

    #[test]
    fn removes_false_lldb_capabilities() {
        let body = br#"{"type":"request","command":"initialize","arguments":{"supportsFoo":false,"supportsBar":true}}"#;
        let frame = [
            format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes(),
            body,
        ]
        .concat();
        let output = String::from_utf8(sanitize_lldb_frame(frame)).unwrap();
        assert!(!output.contains("supportsFoo"));
        assert!(output.contains("supportsBar"));
    }
}
