use std::{
    collections::HashMap,
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
    Generic,
}

impl ProcessKind {
    fn from_environment(environment: &mut HashMap<String, String>) -> Self {
        match environment.remove(PROCESS_KIND_ENV).as_deref() {
            Some("acp-agent") => Self::AcpAgent,
            Some("language-server") => Self::LanguageServer,
            _ => Self::Generic,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProcessIdentity {
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
}

#[derive(Default)]
struct AcpActivity {
    input: Vec<u8>,
    output: Vec<u8>,
    prompts: HashMap<String, String>,
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
            if method != "session/prompt" && method != "session.prompt" {
                continue;
            }
            let Some(id) = value.get("id").map(Value::to_string) else {
                continue;
            };
            let session_id = value
                .pointer("/params/sessionId")
                .or_else(|| value.pointer("/params/session_id"))
                .and_then(Value::as_str);
            if let Some(session_id) = session_id {
                self.prompts.insert(id, session_id.to_string());
            }
        }
    }

    fn feed_output(&mut self, bytes: &[u8]) {
        self.output.extend_from_slice(bytes);
        for line in take_lines(&mut self.output) {
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if value.get("result").is_none() && value.get("error").is_none() {
                continue;
            }
            if let Some(id) = value.get("id").map(Value::to_string) {
                self.prompts.remove(&id);
            }
        }
    }
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
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(&rewrite)
            .map(|path| self.fs.path(&path))
            .transpose()?
            .filter(|path| path.is_dir())
            .unwrap_or(root);
        let identity = (kind == ProcessKind::LanguageServer).then(|| ProcessIdentity {
            program: program.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
        });
        if let Some(identity) = identity.as_ref() {
            self.replace_disconnected_language_server(identity);
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
                Some(acp_activity.clone()),
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
            }
        }
    }

    pub fn reap_orphaned_language_servers(&mut self, generation: u64) -> usize {
        let proc_ids = self
            .processes
            .iter()
            .filter_map(|(proc_id, entry)| {
                (entry.owner_generation == generation
                    && entry.disconnected
                    && entry.kind == ProcessKind::LanguageServer)
                    .then_some(*proc_id)
            })
            .collect::<Vec<_>>();
        let count = proc_ids.len();
        for proc_id in proc_ids {
            if let Some(entry) = self.processes.remove(&proc_id) {
                terminate_process(entry);
            }
        }
        count
    }

    fn replace_disconnected_language_server(&mut self, identity: &ProcessIdentity) {
        let proc_ids = self
            .processes
            .iter()
            .filter_map(|(proc_id, entry)| {
                (entry.disconnected && entry.identity.as_ref() == Some(identity))
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
    let mut buffer = [0_u8; 4096];
    let mut rewriter = rewrite.map(|(source, target)| FrameRewriter::new(source, target));
    loop {
        let Ok(count) = stream.read(&mut buffer).await else {
            break;
        };
        if count == 0 {
            break;
        }
        if let Some(activity) = &acp_activity
            && let Ok(mut activity) = activity.lock()
        {
            activity.feed_output(&buffer[..count]);
        }
        let chunks = rewriter
            .as_mut()
            .map(|rewriter| rewriter.feed(&buffer[..count]))
            .unwrap_or_else(|| vec![buffer[..count].to_vec()]);
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
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::{
        AcpActivity, FrameRewriter, ProcessManager, rewrite_process_value, sanitize_lldb_frame,
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

        activity.feed_output(br#"{"jsonrpc":"2.0","id":17,"result":{"stopReason":"end_turn"}}"#);
        activity.feed_output(b"\n");
        assert!(activity.prompts.is_empty());
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

        assert_eq!(processes.reap_orphaned_language_servers(1), 0);
        assert!(processes.processes.contains_key(&61));
        assert!(processes.processes.contains_key(&62));

        processes.detach_generation(2);
        assert_eq!(processes.reap_orphaned_language_servers(2), 1);
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
