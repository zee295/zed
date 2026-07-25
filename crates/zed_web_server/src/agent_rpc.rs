use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow, bail};
use axum::extract::ws::Message;
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    process::Command,
    sync::{Mutex, mpsc},
};

pub fn handles(method: &str) -> bool {
    method.starts_with("Agent::")
}

#[derive(Clone)]
pub struct AgentManager {
    workspace: Arc<PathBuf>,
    http: reqwest::Client,
    outgoing: mpsc::UnboundedSender<Message>,
    state: Arc<Mutex<AgentState>>,
    cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    sequence: Arc<AtomicU64>,
}

struct AgentState {
    threads: HashMap<String, AgentThread>,
    active_thread_id: String,
    selected_agent_id: String,
}

#[derive(Clone)]
struct AgentThread {
    id: String,
    title: String,
    agent_id: String,
    messages: Vec<Value>,
    created_at: f64,
    updated_at: f64,
}

#[derive(Clone)]
struct AgentConfig {
    provider: String,
    model: String,
    key: String,
    url: String,
}

#[derive(Clone)]
struct ExternalAgent {
    id: String,
    name: String,
    command: String,
    args: Vec<String>,
    kind: String,
    description: String,
    installed: bool,
}

impl AgentManager {
    pub fn new(
        workspace: PathBuf,
        http: reqwest::Client,
        outgoing: mpsc::UnboundedSender<Message>,
    ) -> Self {
        let thread = AgentThread::new("native", "New Thread");
        let active_thread_id = thread.id.clone();
        Self {
            workspace: Arc::new(workspace),
            http,
            outgoing,
            state: Arc::new(Mutex::new(AgentState {
                threads: HashMap::from([(thread.id.clone(), thread)]),
                active_thread_id,
                selected_agent_id: "native".into(),
            })),
            cancels: Arc::new(Mutex::new(HashMap::new())),
            sequence: Arc::new(AtomicU64::new(1)),
        }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "Agent::status" => self.status().await,
            "Agent::list_agents" => Ok(json!({"agents": self.agents()})),
            "Agent::set_agent" => self.set_agent(params).await,
            "Agent::list_threads" => self.list_threads().await,
            "Agent::new_thread" => self.new_thread(params).await,
            "Agent::open_thread" => self.open_thread(params).await,
            "Agent::delete_thread" => self.delete_thread(params).await,
            "Agent::chat" => self.chat(params).await,
            "Agent::cancel" => self.cancel(params).await,
            _ => bail!("unknown agent method: {method}"),
        }
    }

    fn agents(&self) -> Vec<Value> {
        let config = config();
        std::iter::once(json!({
            "id": "native", "name": "Zed Agent", "kind": "native",
            "command": "", "installed": true, "is_native": true,
            "description": "Host-side LLM (OpenAI / Anthropic). Keys stay on server.",
            "provider": config.provider,
            "model": if config.key.is_empty() { "" } else { &config.model },
            "has_key": !config.key.is_empty(),
        }))
        .chain(
            external_agents(&self.workspace)
                .into_iter()
                .map(|agent| agent.public()),
        )
        .collect()
    }

    async fn status(&self) -> Result<Value> {
        let config = config();
        let state = self.state.lock().await;
        Ok(json!({
            "provider": config.provider,
            "model": if config.key.is_empty() { "" } else { &config.model },
            "has_key": !config.key.is_empty(),
            "workspace": self.workspace,
            "selected_agent_id": state.selected_agent_id,
            "active_thread_id": state.active_thread_id,
            "agents": self.agents(),
            "thread_count": state.threads.len(),
        }))
    }

    async fn set_agent(&self, params: &Value) -> Result<Value> {
        let agent_id = text(params, "agent_id", "native").to_string();
        let mut state = self.state.lock().await;
        state.selected_agent_id = agent_id.clone();
        let active = state.active_thread_id.clone();
        if let Some(thread) = state.threads.get_mut(&active) {
            thread.agent_id = agent_id.clone();
        }
        Ok(json!({"selected_agent_id": agent_id, "agents": self.agents()}))
    }

    async fn list_threads(&self) -> Result<Value> {
        let state = self.state.lock().await;
        let mut threads = state.threads.values().collect::<Vec<_>>();
        threads.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at));
        Ok(json!({
            "threads": threads.into_iter().map(|thread| thread.public(false)).collect::<Vec<_>>(),
            "active_thread_id": state.active_thread_id,
            "selected_agent_id": state.selected_agent_id,
        }))
    }

    async fn new_thread(&self, params: &Value) -> Result<Value> {
        let mut state = self.state.lock().await;
        let agent_id = params
            .get("agent_id")
            .and_then(Value::as_str)
            .unwrap_or(&state.selected_agent_id)
            .to_string();
        let thread = AgentThread::new(&agent_id, text(params, "title", "New Thread"));
        let public = thread.public(true);
        state.active_thread_id = thread.id.clone();
        state.selected_agent_id = agent_id;
        state.threads.insert(thread.id.clone(), thread);
        Ok(public)
    }

    async fn open_thread(&self, params: &Value) -> Result<Value> {
        let id = text(params, "thread_id", "");
        let mut state = self.state.lock().await;
        let thread = state
            .threads
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown thread {id}"))?;
        state.active_thread_id = thread.id.clone();
        state.selected_agent_id = thread.agent_id.clone();
        Ok(thread.public(true))
    }

    async fn delete_thread(&self, params: &Value) -> Result<Value> {
        let id = text(params, "thread_id", "");
        let mut state = self.state.lock().await;
        state.threads.remove(id);
        if state.active_thread_id == id {
            state.active_thread_id = state.threads.keys().next().cloned().unwrap_or_default();
        }
        if state.active_thread_id.is_empty() {
            let selected = state.selected_agent_id.clone();
            let thread = AgentThread::new(&selected, "New Thread");
            state.active_thread_id = thread.id.clone();
            state.threads.insert(thread.id.clone(), thread);
        }
        drop(state);
        self.list_threads().await
    }

    async fn chat(&self, params: &Value) -> Result<Value> {
        let chat_id = params
            .get("chat_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("srv-{}", self.sequence.fetch_add(1, Ordering::Relaxed)));
        let messages = normalize_messages(params);
        let (thread_id, agent_id) = self.prepare_chat(params, &messages).await;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels
            .lock()
            .await
            .insert(chat_id.clone(), cancel.clone());
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(ToOwned::to_owned);
        let config = config();
        let response = json!({
            "chat_id": chat_id, "thread_id": thread_id, "agent_id": agent_id,
            "provider": config.provider,
            "model": if config.key.is_empty() { "" } else { model.as_deref().unwrap_or(&config.model) },
            "has_key": !config.key.is_empty(),
        });
        let manager = self.clone();
        tokio::spawn(async move {
            manager
                .run_chat(chat_id, thread_id, agent_id, messages, model, cancel)
                .await;
        });
        Ok(response)
    }

    async fn prepare_chat(&self, params: &Value, messages: &[Value]) -> (String, String) {
        let mut state = self.state.lock().await;
        let requested = params.get("thread_id").and_then(Value::as_str);
        let id = requested
            .filter(|id| state.threads.contains_key(*id))
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| state.active_thread_id.clone());
        let id = if state.threads.contains_key(&id) {
            id
        } else {
            let thread = AgentThread::new("native", "New Thread");
            let id = thread.id.clone();
            state.threads.insert(id.clone(), thread);
            id
        };
        let agent = params
            .get("agent_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| state.threads.get(&id).map(|thread| thread.agent_id.clone()))
            .unwrap_or_else(|| "native".into());
        let thread = state.threads.get_mut(&id).expect("thread exists");
        thread.agent_id = agent.clone();
        if !messages.is_empty() {
            thread.messages = messages.to_vec();
            if thread.title == "New Thread"
                && let Some(first) = messages
                    .iter()
                    .find(|message| message["role"] == "user")
                    .and_then(|message| message["content"].as_str())
            {
                let mut title = first.replace('\n', " ");
                title.truncate(floor_boundary(&title, 48));
                if title.len() < first.len() {
                    title.push('…');
                }
                thread.title = title;
            }
        }
        thread.updated_at = now();
        state.active_thread_id = id.clone();
        state.selected_agent_id = agent.clone();
        (id, agent)
    }

    async fn run_chat(
        &self,
        chat_id: String,
        thread_id: String,
        agent_id: String,
        messages: Vec<Value>,
        model: Option<String>,
        cancel: Arc<AtomicBool>,
    ) {
        let result = if agent_id == "native" {
            self.run_native(&chat_id, &messages, model, cancel).await
        } else {
            self.run_external(&chat_id, &agent_id, &messages, cancel)
                .await
        };
        let (status, error, output) = match result {
            Ok((status, output)) => (status, None, output),
            Err(error) => ("error".into(), Some(error.to_string()), String::new()),
        };
        if !output.is_empty() {
            let mut state = self.state.lock().await;
            if let Some(thread) = state.threads.get_mut(&thread_id) {
                thread
                    .messages
                    .push(json!({"role": "assistant", "content": output}));
                thread.updated_at = now();
            }
        }
        self.cancels.lock().await.remove(&chat_id);
        notify(
            &self.outgoing,
            &format!("Agent::done:{chat_id}"),
            json!({
                "chat_id": chat_id, "status": status, "error": error,
                "thread_id": thread_id, "agent_id": agent_id,
            }),
        );
    }

    async fn run_native(
        &self,
        chat_id: &str,
        messages: &[Value],
        model: Option<String>,
        cancel: Arc<AtomicBool>,
    ) -> Result<(String, String)> {
        let mut config = config();
        if let Some(model) = model {
            config.model = model;
        }
        if config.key.is_empty() {
            let preview = messages
                .last()
                .and_then(|message| message["content"].as_str())
                .unwrap_or_default();
            let preview = &preview[..floor_boundary(preview, preview.len().min(500))];
            let output = format!(
                "[agent] No API key on server.\nSet OPENAI_API_KEY, ANTHROPIC_API_KEY, or ZED_AGENT_API_KEY.\n\nEcho:\n{preview}"
            );
            self.chunk(chat_id, &output);
            return Ok(("no_api_key".into(), output));
        }
        let system = format!(
            "You are the Zed coding agent running on a remote host.\nWorkspace root: {}",
            self.workspace.display()
        );
        let (body, auth, value) = if config.provider == "anthropic" {
            (
                json!({
                    "model": config.model, "max_tokens": 8192, "system": system,
                    "messages": messages, "stream": true, "temperature": 0.2,
                }),
                "x-api-key",
                config.key.clone(),
            )
        } else {
            let mut api_messages = vec![json!({"role": "system", "content": system})];
            api_messages.extend_from_slice(messages);
            (
                json!({
                    "model": config.model, "messages": api_messages,
                    "stream": true, "temperature": 0.2,
                }),
                "authorization",
                format!("Bearer {}", config.key),
            )
        };
        let response = self
            .http
            .post(&config.url)
            .header(auth, value)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body)?)
            .send()
            .await?;
        if !response.status().is_success() {
            bail!(
                "provider HTTP {}: {}",
                response.status(),
                response.text().await?
            );
        }
        let mut stream = response.bytes_stream();
        let mut buffered = String::new();
        let mut output = String::new();
        while let Some(chunk) = stream.next().await {
            if cancel.load(Ordering::Relaxed) {
                return Ok(("cancelled".into(), output));
            }
            buffered.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(end) = buffered.find('\n') {
                let line = buffered.drain(..=end).collect::<String>();
                let data = line.trim().strip_prefix("data:").map(str::trim);
                let Some(data) = data.filter(|data| *data != "[DONE]") else {
                    continue;
                };
                let Ok(event) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let text = if config.provider == "anthropic" {
                    event.pointer("/delta/text")
                } else {
                    event.pointer("/choices/0/delta/content")
                }
                .and_then(Value::as_str);
                if let Some(text) = text.filter(|text| !text.is_empty()) {
                    output.push_str(text);
                    self.chunk(chat_id, text);
                }
            }
        }
        Ok(("ok".into(), output))
    }

    async fn run_external(
        &self,
        chat_id: &str,
        agent_id: &str,
        messages: &[Value],
        cancel: Arc<AtomicBool>,
    ) -> Result<(String, String)> {
        let agent = external_agents(&self.workspace)
            .into_iter()
            .find(|agent| agent.id == agent_id)
            .ok_or_else(|| anyhow!("unknown agent {agent_id}"))?;
        if !agent.installed {
            let output = format!(
                "[agent] External agent `{}` is not installed on the host.\nInstall `{}` on the server, or add it to PATH.\n",
                agent.name, agent.command
            );
            self.chunk(chat_id, &output);
            return Ok(("not_installed".into(), output));
        }
        let prompt = messages
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str())
            .unwrap_or_default();
        let mut child = Command::new(&agent.command)
            .args(&agent.args)
            .current_dir(&*self.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
        }
        let mut lines = BufReader::new(child.stdout.take().context("agent stdout")?).lines();
        let mut output = String::new();
        loop {
            tokio::select! {
                line = lines.next_line() => match line? {
                    Some(line) => {
                        output.push_str(&line);
                        output.push('\n');
                        self.chunk(chat_id, &(line + "\n"));
                    }
                    None => break,
                },
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    if cancel.load(Ordering::Relaxed) {
                        child.kill().await.ok();
                        return Ok(("cancelled".into(), output));
                    }
                }
            }
        }
        let status = child.wait().await?;
        Ok((if status.success() { "ok" } else { "error" }.into(), output))
    }

    fn chunk(&self, chat_id: &str, text: &str) {
        notify(
            &self.outgoing,
            &format!("Agent::chunk:{chat_id}"),
            json!({"chat_id": chat_id, "text": text}),
        );
    }

    async fn cancel(&self, params: &Value) -> Result<Value> {
        let chat_id = text(params, "chat_id", "");
        let cancelled = self
            .cancels
            .lock()
            .await
            .get(chat_id)
            .is_some_and(|cancel| {
                cancel.store(true, Ordering::Relaxed);
                true
            });
        Ok(json!({"cancelled": cancelled, "chat_id": chat_id}))
    }
}

impl AgentThread {
    fn new(agent: &str, title: &str) -> Self {
        let timestamp = now();
        Self {
            id: new_id(),
            title: title.into(),
            agent_id: agent.into(),
            messages: Vec::new(),
            created_at: timestamp,
            updated_at: timestamp,
        }
    }

    fn public(&self, messages: bool) -> Value {
        let mut value = json!({
            "id": self.id, "title": self.title, "agent_id": self.agent_id,
            "created_at": self.created_at, "updated_at": self.updated_at,
            "message_count": self.messages.len(),
        });
        if messages {
            value["messages"] = Value::Array(self.messages.clone());
        }
        value
    }
}

impl ExternalAgent {
    fn public(self) -> Value {
        json!({
            "id": self.id, "name": self.name, "kind": self.kind,
            "command": self.command, "installed": self.installed,
            "description": self.description, "is_native": false,
        })
    }
}

fn config() -> AgentConfig {
    let forced = std::env::var("ZED_AGENT_PROVIDER")
        .unwrap_or_else(|_| "auto".into())
        .to_ascii_lowercase();
    let openai = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let anthropic = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let generic = std::env::var("ZED_AGENT_API_KEY").unwrap_or_default();
    if forced == "anthropic" || (forced == "auto" && openai.is_empty() && !anthropic.is_empty()) {
        AgentConfig {
            provider: if anthropic.is_empty() && generic.is_empty() {
                "none"
            } else {
                "anthropic"
            }
            .into(),
            model: std::env::var("ZED_AGENT_MODEL")
                .or_else(|_| std::env::var("ANTHROPIC_MODEL"))
                .unwrap_or_else(|_| "claude-sonnet-4-20250514".into()),
            key: if anthropic.is_empty() {
                generic
            } else {
                anthropic
            },
            url: std::env::var("ZED_AGENT_BASE_URL")
                .or_else(|_| std::env::var("ANTHROPIC_BASE_URL"))
                .unwrap_or_else(|_| "https://api.anthropic.com/v1/messages".into()),
        }
    } else {
        let key = if openai.is_empty() { generic } else { openai };
        AgentConfig {
            provider: if key.is_empty() { "none" } else { "openai" }.into(),
            model: std::env::var("ZED_AGENT_MODEL")
                .or_else(|_| std::env::var("OPENAI_MODEL"))
                .unwrap_or_else(|_| "gpt-4o-mini".into()),
            key,
            url: std::env::var("ZED_AGENT_BASE_URL")
                .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                .unwrap_or_else(|_| "https://api.openai.com/v1/chat/completions".into()),
        }
    }
}

fn external_agents(workspace: &Path) -> Vec<ExternalAgent> {
    let mut agents = Vec::new();
    let mut seen = HashSet::new();
    for (id, name, command, description) in [
        (
            "claude-code",
            "Claude Code",
            "claude",
            "Anthropic Claude Code CLI",
        ),
        ("gemini", "Gemini CLI", "gemini", "Google Gemini CLI"),
        ("codex", "Codex CLI", "codex", "OpenAI Codex CLI"),
        ("aider", "Aider", "aider", "Aider coding agent"),
        ("opencode", "OpenCode", "opencode", "OpenCode agent"),
    ] {
        add_agent(
            &mut agents,
            &mut seen,
            ExternalAgent {
                id: id.into(),
                name: name.into(),
                command: command.into(),
                args: Vec::new(),
                kind: "shell".into(),
                description: description.into(),
                installed: false,
            },
        );
    }
    let file = std::fs::read_to_string(workspace.join(".zed/external_agents.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let configured = file
        .as_ref()
        .and_then(|value| value.as_array().or_else(|| value["agents"].as_array()));
    for value in configured.into_iter().flatten() {
        if let Some(agent) = parse_agent(value) {
            add_agent(&mut agents, &mut seen, agent);
        }
    }
    if let Ok(raw) = std::env::var("ZED_EXTERNAL_AGENTS")
        && let Ok(values) = serde_json::from_str::<Vec<Value>>(&raw)
    {
        for value in &values {
            if let Some(agent) = parse_agent(value) {
                add_agent(&mut agents, &mut seen, agent);
            }
        }
    }
    agents
}

fn parse_agent(value: &Value) -> Option<ExternalAgent> {
    let id = value.get("id")?.as_str()?.to_string();
    Some(ExternalAgent {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .into(),
        command: value
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .into(),
        id,
        args: value
            .get("args")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        kind: value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("shell")
            .into(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        installed: false,
    })
}

fn add_agent(
    agents: &mut Vec<ExternalAgent>,
    seen: &mut HashSet<String>,
    mut agent: ExternalAgent,
) {
    if seen.insert(agent.id.clone()) {
        agent.installed = command_exists(&agent.command);
        agents.push(agent);
    }
}

fn command_exists(command: &str) -> bool {
    if command.contains('/') {
        return Path::new(command).is_file();
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .any(|directory| directory.join(command).is_file())
}

fn normalize_messages(params: &Value) -> Vec<Value> {
    let mut messages = params
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| {
            let role = match message.get("role").and_then(Value::as_str) {
                Some("assistant") => "assistant",
                _ => "user",
            };
            let content = message
                .get("content")
                .or_else(|| message.get("text"))
                .and_then(Value::as_str)?
                .trim();
            (!content.is_empty()).then(|| json!({"role": role, "content": content}))
        })
        .collect::<Vec<_>>();
    let prompt = text(params, "prompt", "").trim();
    if messages.is_empty() && !prompt.is_empty() {
        messages.push(json!({"role": "user", "content": prompt}));
    }
    messages
}

fn notify(outgoing: &mpsc::UnboundedSender<Message>, method: &str, params: Value) {
    outgoing
        .send(Message::Text(
            json!({"method": method, "params": params}).to_string(),
        ))
        .ok();
}

fn text<'a>(value: &'a Value, key: &str, default: &'a str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or(default)
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

fn new_id() -> String {
    use rand::RngCore as _;
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&bytes[..4]),
        hex::encode(&bytes[4..6]),
        hex::encode(&bytes[6..8]),
        hex::encode(&bytes[8..10]),
        hex::encode(&bytes[10..]),
    )
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}
