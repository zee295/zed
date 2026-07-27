//! Server-backed PTY shim for the WASM target.
//!
//! On native targets the terminal uses alacritty's event loop driving a local
//! PTY. On WASM there is no local PTY; instead we open a remote shell on the
//! server via JSON-RPC and feed incoming bytes into the same alacritty `Term`
//! that desktop uses.

use collections::HashMap;
use std::borrow::Cow;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedSender;
use serde::Deserialize;
use serde_json::json;
use wasm_rpc::RpcClient;

use crate::{PtyEvent, TerminalBackendEvent, TerminalBounds};

const RESUME_KEY_ENV: &str = "ZED_WEB_TERMINAL_RESUME_KEY";
static NEXT_NOTIFICATION_ID: AtomicU64 = AtomicU64::new(1);

/// Handle to a server-side PTY.
pub struct RemotePty {
    term_id: u64,
    client: RpcClient,
    events_tx: UnboundedSender<PtyEvent>,
    notification_id: Arc<Mutex<String>>,
}

#[derive(Deserialize)]
struct OpenResponse {
    term_id: u64,
    #[serde(default)]
    history: String,
    #[serde(default)]
    exit_status: Option<i32>,
}

#[derive(Deserialize)]
struct DataNotification {
    #[allow(dead_code)]
    term_id: u64,
    data: String,
}

#[derive(Deserialize)]
struct ExitNotification {
    #[allow(dead_code)]
    term_id: u64,
    #[allow(dead_code)]
    status: Option<i32>,
}

#[derive(Deserialize)]
struct AttachResponse {
    attached: bool,
    #[serde(default)]
    exit_status: Option<i32>,
}

impl RemotePty {
    /// Open a remote PTY on the server and register notification handlers that
    /// pump server output into the terminal's event channel.
    pub async fn open(
        client: RpcClient,
        shell: Option<(String, Vec<String>)>,
        working_directory: Option<std::path::PathBuf>,
        mut env: HashMap<String, String>,
        initial_bounds: TerminalBounds,
        events_tx: UnboundedSender<PtyEvent>,
    ) -> Result<Self> {
        let resume_key = env.remove(RESUME_KEY_ENV);
        let notification_id = resume_key
            .as_deref()
            .map(persistent_notification_id)
            .unwrap_or_else(|| {
                format!(
                    "pty-{}",
                    NEXT_NOTIFICATION_ID.fetch_add(1, Ordering::Relaxed)
                )
            });
        register_notification_handlers(&client, &notification_id, &events_tx);

        let shell = shell.map(|(program, args)| json!({ "program": program, "args": args }));
        let response: OpenResponse = client
            .call(
                "Terminal::open",
                &json!({
                    "shell": shell,
                    "working_directory": working_directory.map(|p| p.to_string_lossy().to_string()),
                    "env": env,
                    "resume_key": resume_key,
                    "notification_id": notification_id,
                    "cols": initial_bounds.num_columns(),
                    "rows": initial_bounds.num_lines(),
                }),
            )
            .await
            .context("Terminal::open failed")?;

        let term_id = response.term_id;
        if !response.history.is_empty()
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&response.history)
        {
            let _ = events_tx.unbounded_send(PtyEvent::Bytes(bytes));
        }
        if response.exit_status.is_some() {
            forward_exit(&events_tx);
        }

        let notification_id = Arc::new(Mutex::new(notification_id));
        let mut reconnects = client.subscribe_reconnect();
        let reconnect_client = client.clone();
        let reconnect_notification_id = Arc::downgrade(&notification_id);
        let reconnect_events_tx = events_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            while reconnects.next().await.is_some() {
                let Some(notification_id) = reconnect_notification_id.upgrade() else {
                    break;
                };
                let notification_id = notification_id
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let response = reconnect_client
                    .call::<_, AttachResponse>(
                        "Terminal::attach",
                        &json!({
                            "term_id": term_id,
                            "notification_id": notification_id,
                        }),
                    )
                    .await;
                if response
                    .is_ok_and(|response| !response.attached || response.exit_status.is_some())
                {
                    forward_exit(&reconnect_events_tx);
                    break;
                }
            }
        });

        Ok(Self {
            term_id,
            client,
            events_tx,
            notification_id,
        })
    }

    pub fn bind_persistence_key(&self, resume_key: String) {
        let notification_id = persistent_notification_id(&resume_key);
        *self
            .notification_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = notification_id.clone();
        register_notification_handlers(&self.client, &notification_id, &self.events_tx);
        let term_id = self.term_id;
        let client = self.client.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = client
                .call_void(
                    "Terminal::bind",
                    &json!({
                        "term_id": term_id,
                        "resume_key": resume_key,
                        "notification_id": notification_id,
                    }),
                )
                .await;
        });
    }

    pub fn write(&self, input: impl Into<Cow<'static, [u8]>>) {
        let data = base64::engine::general_purpose::STANDARD.encode(&input.into());
        let term_id = self.term_id;
        let client = self.client.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = client
                .call_void(
                    "Terminal::write",
                    &json!({ "term_id": term_id, "data": data }),
                )
                .await;
        });
    }

    pub fn resize(&self, bounds: TerminalBounds) {
        let term_id = self.term_id;
        let client = self.client.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = client
                .call_void(
                    "Terminal::resize",
                    &json!({
                        "term_id": term_id,
                        "cols": bounds.num_columns(),
                        "rows": bounds.num_lines(),
                    }),
                )
                .await;
        });
    }

    pub fn close(&self) {
        let term_id = self.term_id;
        let client = self.client.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = client
                .call_void("Terminal::close", &json!({ "term_id": term_id }))
                .await;
        });
    }
}

fn persistent_notification_id(resume_key: &str) -> String {
    format!("persisted-{resume_key}")
}

fn register_notification_handlers(
    client: &RpcClient,
    notification_id: &str,
    events_tx: &UnboundedSender<PtyEvent>,
) {
    client.on_notification(&format!("Terminal::data:{notification_id}"), {
        let events_tx = events_tx.clone();
        move |params| forward_data_notification(params, &events_tx)
    });
    client.on_notification(&format!("Terminal::exit:{notification_id}"), {
        let events_tx = events_tx.clone();
        move |params| forward_exit_notification(params, &events_tx)
    });
}

fn forward_data_notification(params: serde_json::Value, events_tx: &UnboundedSender<PtyEvent>) {
    let Ok(payload) = serde_json::from_value::<DataNotification>(params) else {
        return;
    };
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&payload.data) else {
        return;
    };
    let _ = events_tx.unbounded_send(PtyEvent::Bytes(bytes));
}

fn forward_exit_notification(params: serde_json::Value, events_tx: &UnboundedSender<PtyEvent>) {
    let Ok(_payload) = serde_json::from_value::<ExitNotification>(params) else {
        return;
    };
    forward_exit(events_tx);
}

fn forward_exit(events_tx: &UnboundedSender<PtyEvent>) {
    let _ = events_tx.unbounded_send(PtyEvent::Event(TerminalBackendEvent::Exit));
    let _ = events_tx.unbounded_send(PtyEvent::Event(TerminalBackendEvent::ChildExit(
        ExitStatus::default(),
    )));
}
