use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex as StdMutex},
};

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
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    stdin_rewriter: Mutex<FrameRewriter>,
    acp_activity: Arc<StdMutex<AcpActivity>>,
    sanitize_lldb: bool,
    task: JoinHandle<()>,
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
        self.reap_disconnected();
        match method {
            "Process::spawn" => self.spawn(params, owner_generation).await,
            "Process::write_stdin" => self.write_stdin(params).await,
            "Process::close_stdin" => self.close_stdin(params).await,
            "Process::kill" => self.kill(params),
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
        let root_text = root.to_string_lossy().to_string();
        let rewrite = |value: &str| value.replace("/workspace", &root_text);
        let raw_args = params
            .get("args")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(rewrite)
            .collect::<Vec<_>>();
        let (program, args) = crate::debug_adapter::resolve(program, raw_args)?;
        let sanitize_lldb = program
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("lldb-dap"));
        let mut command = AsyncCommand::new(&program);
        command.kill_on_drop(true);
        command.args(args);
        command.current_dir(
            params
                .get("cwd")
                .and_then(Value::as_str)
                .map(rewrite)
                .map(|path| self.fs.path(&path))
                .transpose()?
                .filter(|path| path.is_dir())
                .unwrap_or(root),
        );
        let process_environment = environment(params);
        for (key, value) in &process_environment {
            command.env(key, rewrite(&value));
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
                Some((root_text.clone().into_bytes(), b"/workspace".to_vec())),
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
        let task = tokio::spawn(async move {
            let status = child
                .wait()
                .await
                .ok()
                .and_then(|status| status.code())
                .unwrap_or(-1);
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
                stdin,
                stdin_rewriter: Mutex::new(FrameRewriter::new(
                    b"/workspace".to_vec(),
                    root_text.as_bytes().to_vec(),
                )),
                acp_activity,
                sanitize_lldb,
                task,
            },
        ) {
            previous.task.abort();
        }
        Ok(json!({"proc_id": proc_id}))
    }

    pub fn detach_generation(&mut self, generation: u64) {
        for entry in self.processes.values_mut() {
            if entry.owner_generation == generation {
                entry.disconnected = true;
            }
        }
        self.reap_disconnected();
    }

    fn reap_disconnected(&mut self) {
        self.processes.retain(|_, entry| {
            let has_running_prompt = entry
                .acp_activity
                .lock()
                .is_ok_and(|activity| !activity.prompts.is_empty());
            let retain = !entry.disconnected || has_running_prompt;
            if !retain {
                entry.task.abort();
            }
            retain
        });
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
            entry.task.abort();
            notify(
                &self.outgoing,
                "Process::exit",
                json!({"proc_id": proc_id, "status": -1}),
            );
        }
        Ok(Value::Null)
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
    let rewrite = |value: &str| value.replace("/workspace", &root.to_string_lossy());
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
    let mut command = Command::new(program);
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
    use super::{AcpActivity, FrameRewriter, sanitize_lldb_frame};

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
