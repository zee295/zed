//! Full paint-only agent panel for wasm.
//!
//! Desktop `agent_ui` now *compiles* for wasm (extension_host/gpui_tokio/jsonschema
//! gated), but it still expects native services at runtime (agent server processes,
//! local thread DB, model providers). This panel remains the active agent surface:
//! it mirrors desktop chrome (toolbar, agent picker, new thread, options menu,
//! threads) and talks to host `Agent::*` RPC only — same pattern as Terminal / LSP.
//! `agent_ui` is linked into the binary so its actions/types resolve; swapping the
//! panel for the real `AgentPanel` is the follow-up once host-side backing is decided.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use agent_settings::AgentSettings;
use editor::Editor;
use gpui::{
    Action, Anchor, AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
    Focusable, SharedString, Task, WeakEntity, Window, div, px,
};
use serde::Deserialize;
use serde_json::json;
use settings::{Settings, update_settings_file};
use ui::{Button, ButtonStyle, ContextMenu, IconButton, PopoverMenu, Tooltip, prelude::*};
use wasm_remote::RemoteClient;
use workspace::dock::{DockPosition, PanelEvent};
use workspace::{Panel, Workspace};
use zed_actions::assistant::ToggleFocus;

static REMOTE_CLIENT: OnceLock<RemoteClient> = OnceLock::new();
static CHAT_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn set_remote_client(client: RemoteClient) {
    let _ = REMOTE_CLIENT.set(client);
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WebAgentPanel>(window, cx);
        });
    })
    .detach();
}

fn remote_client() -> Option<RemoteClient> {
    REMOTE_CLIENT.get().cloned()
}

const AGENT_PANEL_KEY: &str = "AgentPanel";

// --- wire types -----------------------------------------------------------

#[derive(Clone, Debug, Deserialize, Default)]
struct AgentInfo {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    installed: bool,
    #[serde(default)]
    description: String,
    #[serde(default)]
    is_native: bool,
    #[serde(default)]
    has_key: bool,
    #[serde(default)]
    model: String,
    #[serde(default)]
    provider: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct ThreadInfo {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    message_count: u32,
    #[serde(default)]
    messages: Vec<ApiMessage>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct ApiMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: String,
}

#[derive(Deserialize, Default)]
struct StatusResponse {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    has_key: bool,
    #[serde(default)]
    selected_agent_id: String,
    #[serde(default)]
    active_thread_id: String,
    #[serde(default)]
    agents: Vec<AgentInfo>,
}

#[derive(Deserialize)]
struct ListAgentsResponse {
    #[serde(default)]
    agents: Vec<AgentInfo>,
}

#[derive(Deserialize)]
struct ListThreadsResponse {
    #[serde(default)]
    threads: Vec<ThreadInfo>,
    #[serde(default)]
    active_thread_id: String,
    #[serde(default)]
    selected_agent_id: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    chat_id: String,
    #[serde(default)]
    thread_id: String,
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    has_key: bool,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct RunningChatInfo {
    #[serde(default)]
    chat_id: String,
    #[serde(default)]
    thread_id: String,
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    output: String,
}

#[derive(Deserialize, Default)]
struct RunningChatResponse {
    #[serde(default)]
    running: Option<RunningChatInfo>,
}

// --- UI state -------------------------------------------------------------

#[derive(Clone, Debug)]
struct ChatMessage {
    role: MessageRole,
    text: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageRole {
    User,
    Assistant,
    System,
}

pub struct WebAgentPanel {
    focus_handle: FocusHandle,
    fs: Arc<dyn fs::Fs>,
    input: Entity<Editor>,
    messages: Vec<ChatMessage>,
    status: SharedString,
    provider_label: SharedString,
    busy: bool,
    active_chat_id: Option<String>,
    agents: Vec<AgentInfo>,
    threads: Vec<ThreadInfo>,
    selected_agent_id: String,
    active_thread_id: String,
    show_thread_sidebar: bool,
}

impl WebAgentPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: gpui::AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                cx.new(|cx| Self::new(workspace, window, cx))
            })
        })
    }

    fn new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let input = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 6, window, cx);
            editor.set_placeholder_text("Message the agent (host)…", window, cx);
            editor
        });

        let this = Self {
            focus_handle,
            fs: workspace.app_state().fs.clone(),
            input,
            messages: vec![ChatMessage {
                role: MessageRole::System,
                text: "Host agent panel — execution on server via Agent::* RPC. \
                       Use + for new thread, agent menu for external agents, ⋯ for options."
                    .into(),
            }],
            status: "Connecting…".into(),
            provider_label: "host".into(),
            busy: false,
            active_chat_id: None,
            agents: Vec::new(),
            threads: Vec::new(),
            selected_agent_id: "native".into(),
            active_thread_id: String::new(),
            show_thread_sidebar: true,
        };

        this.bootstrap(cx);
        this
    }

    fn bootstrap(&self, cx: &mut Context<Self>) {
        let Some(client) = remote_client() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(st) = client
                .call::<_, StatusResponse>("Agent::status", &json!({}))
                .await
            {
                let _ = this.update(cx, |this, cx| {
                    this.apply_status(st);
                    cx.notify();
                });
            }
            if let Ok(lt) = client
                .call::<_, ListThreadsResponse>("Agent::list_threads", &json!({}))
                .await
            {
                let _ = this.update(cx, |this, cx| {
                    this.threads = lt.threads;
                    if !lt.active_thread_id.is_empty() {
                        this.active_thread_id = lt.active_thread_id;
                    }
                    if !lt.selected_agent_id.is_empty() {
                        this.selected_agent_id = lt.selected_agent_id;
                    }
                    cx.notify();
                });
            }
            // Load active thread messages
            let tid = this
                .read_with(cx, |this, _| this.active_thread_id.clone())
                .unwrap_or_default();
            if !tid.is_empty() {
                if let Ok(th) = client
                    .call::<_, ThreadInfo>("Agent::open_thread", &json!({ "thread_id": tid }))
                    .await
                {
                    let _ = this.update(cx, |this, cx| {
                        this.load_thread(th);
                        cx.notify();
                    });
                }
            }
            let running = client
                .call::<_, RunningChatResponse>("Agent::running", &json!({ "thread_id": tid }))
                .await
                .ok()
                .and_then(|response| response.running);
            if let Some(running) = running {
                let assistant_index = this
                    .update(cx, |this, cx| {
                        this.active_chat_id = Some(running.chat_id.clone());
                        this.active_thread_id = running.thread_id.clone();
                        this.selected_agent_id = running.agent_id.clone();
                        this.busy = true;
                        this.status = "Streaming…".into();
                        this.messages.push(ChatMessage {
                            role: MessageRole::Assistant,
                            text: running.output.clone().into(),
                        });
                        cx.notify();
                        this.messages.len() - 1
                    })
                    .ok();
                let Some(assistant_index) = assistant_index else {
                    return;
                };
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(100))
                        .await;
                    let current = client
                        .call::<_, RunningChatResponse>(
                            "Agent::running",
                            &json!({ "thread_id": running.thread_id }),
                        )
                        .await
                        .ok()
                        .and_then(|response| response.running);
                    let Some(current) = current else {
                        if let Ok(thread) = client
                            .call::<_, ThreadInfo>(
                                "Agent::open_thread",
                                &json!({ "thread_id": running.thread_id }),
                            )
                            .await
                        {
                            let _ = this.update(cx, |this, cx| {
                                this.load_thread(thread);
                                this.active_chat_id = None;
                                this.busy = false;
                                this.status = "Ready".into();
                                cx.notify();
                            });
                        }
                        break;
                    };
                    let _ = this.update(cx, |this, cx| {
                        if let Some(message) = this.messages.get_mut(assistant_index) {
                            message.text = current.output.into();
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn apply_status(&mut self, st: StatusResponse) {
        self.agents = st.agents;
        if !st.selected_agent_id.is_empty() {
            self.selected_agent_id = st.selected_agent_id;
        }
        if !st.active_thread_id.is_empty() {
            self.active_thread_id = st.active_thread_id;
        }
        if st.has_key {
            self.provider_label = format!("{} · {}", st.provider, st.model).into();
            self.status = "Ready".into();
        } else {
            self.provider_label = "no API key".into();
            self.status = "No API key on server".into();
        }
    }

    fn load_thread(&mut self, th: ThreadInfo) {
        self.active_thread_id = th.id.clone();
        self.selected_agent_id = th.agent_id.clone();
        self.messages.clear();
        if th.messages.is_empty() {
            self.messages.push(ChatMessage {
                role: MessageRole::System,
                text: format!("Thread “{}” — host agent `{}`.", th.title, th.agent_id).into(),
            });
        } else {
            for m in th.messages {
                let role = match m.role.as_str() {
                    "assistant" => MessageRole::Assistant,
                    "system" => MessageRole::System,
                    _ => MessageRole::User,
                };
                self.messages.push(ChatMessage {
                    role,
                    text: m.content.into(),
                });
            }
        }
        // refresh threads list entry
        if let Some(t) = self.threads.iter_mut().find(|t| t.id == th.id) {
            *t = ThreadInfo {
                messages: Vec::new(),
                ..th
            };
        } else {
            self.threads.insert(
                0,
                ThreadInfo {
                    messages: Vec::new(),
                    ..th
                },
            );
        }
    }

    fn selected_agent_label(&self) -> SharedString {
        self.agents
            .iter()
            .find(|a| a.id == self.selected_agent_id)
            .map(|a| {
                if a.is_native && !a.model.is_empty() {
                    format!("{} ({})", a.name, a.model).into()
                } else {
                    a.name.clone().into()
                }
            })
            .unwrap_or_else(|| self.selected_agent_id.clone().into())
    }

    fn history_for_api(&self) -> Vec<serde_json::Value> {
        self.messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::User | MessageRole::Assistant))
            .filter(|m| !m.text.is_empty())
            .map(|m| {
                let role = match m.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => "system",
                };
                json!({ "role": role, "content": m.text.as_ref() })
            })
            .collect()
    }

    fn new_thread(&mut self, cx: &mut Context<Self>) {
        let Some(client) = remote_client() else {
            return;
        };
        let agent_id = self.selected_agent_id.clone();
        self.status = "New thread…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            match client
                .call::<_, ThreadInfo>("Agent::new_thread", &json!({ "agent_id": agent_id }))
                .await
            {
                Ok(th) => {
                    let _ = this.update(cx, |this, cx| {
                        this.load_thread(th);
                        this.status = "Ready".into();
                        cx.notify();
                    });
                    // refresh list
                    if let Ok(lt) = client
                        .call::<_, ListThreadsResponse>("Agent::list_threads", &json!({}))
                        .await
                    {
                        let _ = this.update(cx, |this, cx| {
                            this.threads = lt.threads;
                            cx.notify();
                        });
                    }
                }
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        this.status = format!("new thread: {err:#}").into();
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn open_thread(&mut self, thread_id: String, cx: &mut Context<Self>) {
        let Some(client) = remote_client() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            match client
                .call::<_, ThreadInfo>("Agent::open_thread", &json!({ "thread_id": thread_id }))
                .await
            {
                Ok(th) => {
                    let _ = this.update(cx, |this, cx| {
                        this.load_thread(th);
                        this.status = "Ready".into();
                        cx.notify();
                    });
                }
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        this.status = format!("open: {err:#}").into();
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn delete_thread(&mut self, thread_id: String, cx: &mut Context<Self>) {
        let Some(client) = remote_client() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(lt) = client
                .call::<_, ListThreadsResponse>(
                    "Agent::delete_thread",
                    &json!({ "thread_id": thread_id }),
                )
                .await
            {
                let _ = this.update(cx, |this, cx| {
                    this.threads = lt.threads.clone();
                    if !lt.active_thread_id.is_empty() {
                        this.active_thread_id = lt.active_thread_id;
                    }
                    cx.notify();
                });
            }
            // reopen active
            let tid = this
                .read_with(cx, |this, _| this.active_thread_id.clone())
                .unwrap_or_default();
            if !tid.is_empty() {
                if let Ok(th) = client
                    .call::<_, ThreadInfo>("Agent::open_thread", &json!({ "thread_id": tid }))
                    .await
                {
                    let _ = this.update(cx, |this, cx| {
                        this.load_thread(th);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn select_agent(&mut self, agent_id: String, cx: &mut Context<Self>) {
        let Some(client) = remote_client() else {
            return;
        };
        self.selected_agent_id = agent_id.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            if let Ok(resp) = client
                .call::<_, serde_json::Value>("Agent::set_agent", &json!({ "agent_id": agent_id }))
                .await
            {
                let _ = this.update(cx, |this, cx| {
                    if let Some(agents) = resp.get("agents") {
                        if let Ok(list) = serde_json::from_value::<Vec<AgentInfo>>(agents.clone()) {
                            this.agents = list;
                        }
                    }
                    if let Some(id) = resp.get("selected_agent_id").and_then(|v| v.as_str()) {
                        this.selected_agent_id = id.to_string();
                    }
                    this.status = format!("Agent: {}", this.selected_agent_id).into();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn refresh_agents(&mut self, cx: &mut Context<Self>) {
        let Some(client) = remote_client() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(resp) = client
                .call::<_, ListAgentsResponse>("Agent::list_agents", &json!({}))
                .await
            {
                let _ = this.update(cx, |this, cx| {
                    this.agents = resp.agents;
                    this.status = format!("{} agents", this.agents.len()).into();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let prompt = self.input.read(cx).text(cx).trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let Some(client) = remote_client() else {
            self.status = "No remote client".into();
            cx.notify();
            return;
        };

        self.messages.push(ChatMessage {
            role: MessageRole::User,
            text: prompt.clone().into(),
        });
        self.input.update(cx, |editor, cx| editor.clear(window, cx));

        let chat_id = format!("web-{}", CHAT_SEQ.fetch_add(1, Ordering::SeqCst));
        self.active_chat_id = Some(chat_id.clone());
        self.busy = true;
        self.status = "Thinking…".into();
        self.messages.push(ChatMessage {
            role: MessageRole::Assistant,
            text: "".into(),
        });
        let assistant_index = self.messages.len() - 1;
        let api_messages = self.history_for_api();
        let thread_id = self.active_thread_id.clone();
        let agent_id = self.selected_agent_id.clone();
        cx.notify();

        let chunk_method = format!("Agent::chunk:{chat_id}");
        let done_method = format!("Agent::done:{chat_id}");
        let stream: Arc<Mutex<Vec<StreamEvent>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let stream = stream.clone();
            client.on_notification(&chunk_method, move |params| {
                let text = params
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !text.is_empty() {
                    stream.lock().unwrap().push(StreamEvent::Chunk(text));
                }
            });
        }
        {
            let stream = stream.clone();
            client.on_notification(&done_method, move |params| {
                stream.lock().unwrap().push(StreamEvent::Done {
                    status: params
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("ok")
                        .to_string(),
                    provider: params
                        .get("provider")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    model: params
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    thread: params.get("thread").cloned(),
                });
            });
        }

        cx.spawn_in(window, async move |this, cx| {
            let call_result = client
                .call::<_, ChatResponse>(
                    "Agent::chat",
                    &json!({
                        "chat_id": chat_id,
                        "thread_id": thread_id,
                        "agent_id": agent_id,
                        "messages": api_messages,
                        "prompt": prompt,
                    }),
                )
                .await;

            if let Ok(resp) = &call_result {
                let _ = this.update(cx, |this, cx| {
                    if !resp.thread_id.is_empty() {
                        this.active_thread_id = resp.thread_id.clone();
                    }
                    if !resp.agent_id.is_empty() {
                        this.selected_agent_id = resp.agent_id.clone();
                    }
                    if resp.has_key && !resp.provider.is_empty() {
                        this.provider_label = format!("{} · {}", resp.provider, resp.model).into();
                    }
                    cx.notify();
                });
            }
            if let Err(err) = call_result {
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    this.active_chat_id = None;
                    this.status = format!("Error: {err:#}").into();
                    if let Some(msg) = this.messages.get_mut(assistant_index) {
                        msg.text = format!("[error] {err:#}").into();
                    }
                    cx.notify();
                });
                return;
            }

            loop {
                let events = {
                    let mut g = stream.lock().unwrap();
                    std::mem::take(&mut *g)
                };
                let mut finished = false;
                for event in events {
                    match event {
                        StreamEvent::Chunk(text) => {
                            let _ = this.update(cx, |this, cx| {
                                if let Some(msg) = this.messages.get_mut(assistant_index) {
                                    let mut s = msg.text.to_string();
                                    s.push_str(&text);
                                    msg.text = s.into();
                                }
                                this.status = "Streaming…".into();
                                cx.notify();
                            });
                        }
                        StreamEvent::Done {
                            status,
                            provider,
                            model,
                            thread,
                        } => {
                            let _ = this.update(cx, |this, cx| {
                                this.busy = false;
                                this.active_chat_id = None;
                                if !provider.is_empty() {
                                    this.provider_label = if model.is_empty() {
                                        provider.into()
                                    } else {
                                        format!("{provider} · {model}").into()
                                    };
                                }
                                this.status = match status.as_str() {
                                    "ok" => "Ready".into(),
                                    "no_api_key" => "No API key on server".into(),
                                    "cancelled" => "Cancelled".into(),
                                    "not_installed" => "Agent not installed on host".into(),
                                    "error" => "Error".into(),
                                    other => format!("Done ({other})").into(),
                                };
                                if let Some(msg) = this.messages.get_mut(assistant_index) {
                                    if msg.text.is_empty() {
                                        msg.text = match status.as_str() {
                                            "cancelled" => "[cancelled]".into(),
                                            "error" => "[error]".into(),
                                            "not_installed" => {
                                                "[external agent not installed on host]".into()
                                            }
                                            _ => "[empty response]".into(),
                                        };
                                    }
                                }
                                if let Some(th_val) = thread {
                                    if let Ok(th) = serde_json::from_value::<ThreadInfo>(th_val) {
                                        // Keep streamed messages; just sync meta
                                        this.active_thread_id = th.id.clone();
                                        if let Some(t) =
                                            this.threads.iter_mut().find(|t| t.id == th.id)
                                        {
                                            t.title = th.title;
                                            t.message_count = th.message_count;
                                            t.agent_id = th.agent_id;
                                        }
                                    }
                                }
                                cx.notify();
                            });
                            finished = true;
                        }
                    }
                }
                if finished {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(30))
                    .await;
            }
        })
        .detach();
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat_id) = self.active_chat_id.clone() else {
            return;
        };
        let Some(client) = remote_client() else {
            return;
        };
        self.status = "Cancelling…".into();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let _ = client
                .call_void("Agent::cancel", &json!({ "chat_id": chat_id }))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.active_chat_id = None;
                this.status = "Cancelled".into();
                cx.notify();
            });
        })
        .detach();
    }

    fn clear_thread_local(&mut self, cx: &mut Context<Self>) {
        self.messages.clear();
        self.messages.push(ChatMessage {
            role: MessageRole::System,
            text: "Local view cleared (host thread unchanged). Use ⋯ → New Thread for a fresh host thread.".into(),
        });
        self.status = "Ready".into();
        self.busy = false;
        self.active_chat_id = None;
        cx.notify();
    }

    fn open_extensions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Dispatch workspace Extensions action so web_extensions page opens.
        window.dispatch_action(zed_actions::Extensions::default().boxed_clone(), cx);
    }
}

enum StreamEvent {
    Chunk(String),
    Done {
        status: String,
        provider: String,
        model: String,
        thread: Option<serde_json::Value>,
    },
}

impl EventEmitter<PanelEvent> for WebAgentPanel {}

impl Focusable for WebAgentPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WebAgentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.busy;
        let agent_label = self.selected_agent_label();
        let agents = self.agents.clone();
        let selected_agent = self.selected_agent_id.clone();
        let threads = self.threads.clone();
        let active_tid = self.active_thread_id.clone();
        let show_sidebar = self.show_thread_sidebar;

        // --- messages ---
        let messages: Vec<AnyElement> = self
            .messages
            .iter()
            .enumerate()
            .map(|(ix, msg)| {
                let (label, color) = match msg.role {
                    MessageRole::User => ("You", Color::Accent),
                    MessageRole::Assistant => ("Agent", Color::Default),
                    MessageRole::System => ("System", Color::Muted),
                };
                let streaming_tail = busy
                    && msg.role == MessageRole::Assistant
                    && ix + 1 == self.messages.len()
                    && msg.text.is_empty();
                let body = if streaming_tail {
                    SharedString::from("…")
                } else {
                    msg.text.clone()
                };
                v_flex()
                    .w_full()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        Label::new(label)
                            .size(LabelSize::XSmall)
                            .weight(gpui::FontWeight::MEDIUM)
                            .color(color),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().colors().text)
                            .child(body),
                    )
                    .into_any_element()
            })
            .collect();

        // --- toolbar (desktop-like) ---
        let toolbar = h_flex()
            .w_full()
            .h(px(36.))
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Icon::new(IconName::ZedAssistant)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
            // New thread
            .child(
                IconButton::new("agent-new", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("New Thread"))
                    .on_click(cx.listener(|this, _, _w, cx| this.new_thread(cx))),
            )
            // Agent picker (host agents + external)
            .child({
                let agents = agents.clone();
                let selected = selected_agent.clone();
                PopoverMenu::new("agent-picker")
                    .trigger(
                        Button::new("agent-picker-btn", agent_label)
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small),
                    )
                    .menu({
                        let entity = cx.entity().downgrade();
                        move |window, cx| {
                            let agents = agents.clone();
                            let selected = selected.clone();
                            let entity = entity.clone();
                            ContextMenu::build(window, cx, move |mut menu, _, _| {
                                menu = menu.header("Host agents");
                                for a in agents {
                                    let id = a.id.clone();
                                    let is_sel = a.id == selected;
                                    let label = if a.is_native {
                                        format!(
                                            "Zed Agent{}",
                                            if a.model.is_empty() {
                                                String::new()
                                            } else {
                                                format!(" ({})", a.model)
                                            }
                                        )
                                    } else if a.installed {
                                        a.name.clone()
                                    } else {
                                        format!("{} (not on PATH)", a.name)
                                    };
                                    let entity = entity.clone();
                                    menu = menu.custom_entry(
                                        {
                                            let label = label.clone();
                                            move |_, _| {
                                                h_flex()
                                                    .gap_2()
                                                    .child(Label::new(label.clone()))
                                                    .when(is_sel, |t| {
                                                        t.child(
                                                            Icon::new(IconName::Check)
                                                                .size(IconSize::Small)
                                                                .color(Color::Accent),
                                                        )
                                                    })
                                                    .into_any_element()
                                            }
                                        },
                                        move |_, cx| {
                                            let id = id.clone();
                                            let _ = entity.update(cx, |this, cx| {
                                                this.select_agent(id, cx);
                                            });
                                        },
                                    );
                                }
                                menu.separator().entry("Refresh agents", None, {
                                    let entity = entity.clone();
                                    move |_, cx| {
                                        let _ = entity.update(cx, |this, cx| {
                                            this.refresh_agents(cx);
                                        });
                                    }
                                })
                            })
                            .into()
                        }
                    })
            })
            .child(div().flex_1())
            .child(
                Label::new(self.provider_label.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(self.status.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            // Toggle thread list
            .child(
                IconButton::new("agent-threads-toggle", IconName::ListTree)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Toggle thread list"))
                    .on_click(cx.listener(|this, _, _w, cx| {
                        this.show_thread_sidebar = !this.show_thread_sidebar;
                        cx.notify();
                    })),
            )
            // Options menu (⋯)
            .child({
                let entity = cx.entity().downgrade();
                PopoverMenu::new("agent-options")
                    .anchor(Anchor::TopRight)
                    .trigger(
                        IconButton::new("agent-options-btn", IconName::Ellipsis)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Agent menu")),
                    )
                    .menu(move |window, cx| {
                        let entity = entity.clone();
                        ContextMenu::build(window, cx, move |menu, _, _| {
                            menu.header("Thread")
                                .entry("New Thread", None, {
                                    let entity = entity.clone();
                                    move |_, cx| {
                                        let _ = entity.update(cx, |this, cx| this.new_thread(cx));
                                    }
                                })
                                .entry("Clear view", None, {
                                    let entity = entity.clone();
                                    move |_, cx| {
                                        let _ = entity
                                            .update(cx, |this, cx| this.clear_thread_local(cx));
                                    }
                                })
                                .separator()
                                .header("Host")
                                .entry("Refresh agents", None, {
                                    let entity = entity.clone();
                                    move |_, cx| {
                                        let _ =
                                            entity.update(cx, |this, cx| this.refresh_agents(cx));
                                    }
                                })
                                .entry("Extensions…", None, {
                                    let entity = entity.clone();
                                    move |window, cx| {
                                        let _ = entity.update(cx, |this, cx| {
                                            this.open_extensions(window, cx);
                                        });
                                    }
                                })
                                .separator()
                                .entry("Open settings.json", None, |window, cx| {
                                    window.dispatch_action(
                                        zed_actions::OpenSettings.boxed_clone(),
                                        cx,
                                    );
                                })
                        })
                        .into()
                    })
            });

        // --- thread sidebar ---
        let sidebar = if show_sidebar {
            let entity = cx.entity().downgrade();
            let items: Vec<AnyElement> = threads
                .iter()
                .map(|t| {
                    let id = t.id.clone();
                    let active = t.id == active_tid;
                    let title = if t.title.is_empty() {
                        "New Thread".to_string()
                    } else {
                        t.title.clone()
                    };
                    let entity = entity.clone();
                    let del_id = id.clone();
                    h_flex()
                        .w_full()
                        .gap_1()
                        .px_1()
                        .py_0p5()
                        .rounded_md()
                        .when(active, |this| this.bg(cx.theme().colors().element_selected))
                        .child(
                            Button::new(SharedString::from(format!("th-{id}")), title)
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::XSmall)
                                .full_width()
                                .on_click({
                                    let entity = entity.clone();
                                    let id = id.clone();
                                    move |_, _w, cx| {
                                        let _ = entity.update(cx, |this, cx| {
                                            this.open_thread(id.clone(), cx);
                                        });
                                    }
                                }),
                        )
                        .child(
                            IconButton::new(
                                SharedString::from(format!("th-del-{del_id}")),
                                IconName::Close,
                            )
                            .icon_size(IconSize::XSmall)
                            .on_click({
                                let entity = entity.clone();
                                move |_, _w, cx| {
                                    let _ = entity.update(cx, |this, cx| {
                                        this.delete_thread(del_id.clone(), cx);
                                    });
                                }
                            }),
                        )
                        .into_any_element()
                })
                .collect();
            Some(
                v_flex()
                    .w(px(160.))
                    .h_full()
                    .border_r_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .child(
                                Label::new("Threads")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(div().flex_1())
                            .child(
                                IconButton::new("new-th-side", IconName::Plus)
                                    .icon_size(IconSize::XSmall)
                                    .on_click(cx.listener(|this, _, _w, cx| this.new_thread(cx))),
                            ),
                    )
                    .child(
                        div()
                            .id("thread-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_1()
                            .gap_0p5()
                            .flex()
                            .flex_col()
                            .children(items),
                    ),
            )
        } else {
            None
        };

        v_flex()
            .key_context("AgentPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .children(sidebar)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(
                                div()
                                    .id("agent-messages")
                                    .flex_1()
                                    .w_full()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .p_2()
                                    .gap_2()
                                    .flex()
                                    .flex_col()
                                    .children(messages),
                            )
                            .child(
                                v_flex()
                                    .w_full()
                                    .p_2()
                                    .gap_2()
                                    .border_t_1()
                                    .border_color(cx.theme().colors().border)
                                    .child(
                                        div()
                                            .w_full()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(cx.theme().colors().border)
                                            .bg(cx.theme().colors().editor_background)
                                            .p_1()
                                            .child(self.input.clone()),
                                    )
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .justify_between()
                                            .gap_2()
                                            .child(
                                                Label::new(format!(
                                                    "agent: {} · host RPC",
                                                    self.selected_agent_id
                                                ))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                            )
                                            .child(
                                                h_flex()
                                                    .gap_2()
                                                    .when(busy, |this| {
                                                        this.child(
                                                            Button::new("agent-cancel", "Cancel")
                                                                .style(ButtonStyle::Subtle)
                                                                .on_click(cx.listener(
                                                                    |this, _, window, cx| {
                                                                        this.cancel(window, cx);
                                                                    },
                                                                )),
                                                        )
                                                    })
                                                    .child(
                                                        Button::new(
                                                            "agent-send",
                                                            if busy {
                                                                "Sending…"
                                                            } else {
                                                                "Send"
                                                            },
                                                        )
                                                        .style(ButtonStyle::Filled)
                                                        .disabled(busy)
                                                        .on_click(cx.listener(
                                                            |this, _, window, cx| {
                                                                this.send(window, cx);
                                                            },
                                                        )),
                                                    ),
                                            ),
                                    ),
                            ),
                    ),
            )
    }
}

impl Panel for WebAgentPanel {
    fn persistent_name() -> &'static str {
        "AgentPanel"
    }

    fn panel_key() -> &'static str {
        AGENT_PANEL_KEY
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        AgentSettings::get_global(cx).dock.into()
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position != DockPosition::Bottom
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings
                .agent
                .get_or_insert_default()
                .set_dock(position.into());
        });
    }

    fn default_size(&self, window: &Window, cx: &App) -> gpui::Pixels {
        let settings = AgentSettings::get_global(cx);
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => settings.default_width,
            DockPosition::Bottom => settings.default_height,
        }
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        (AgentSettings::get_global(cx).enabled(cx) && AgentSettings::get_global(cx).button)
            .then_some(IconName::ZedAssistant)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Agent Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        0
    }

    fn enabled(&self, cx: &App) -> bool {
        AgentSettings::get_global(cx).enabled(cx)
    }

    fn is_agent_panel(&self) -> bool {
        true
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        false
    }

    fn set_active(&mut self, _active: bool, _window: &mut Window, _cx: &mut Context<Self>) {}
}
