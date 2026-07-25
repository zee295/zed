//! Server-backed PTY shim for the WASM target.
//!
//! On native targets the terminal uses alacritty's event loop driving a local
//! PTY. On WASM there is no local PTY; instead we open a remote shell on the
//! server via JSON-RPC and feed incoming bytes into the same alacritty `Term`
//! that desktop uses.

use collections::HashMap;
use std::borrow::Cow;
use std::process::ExitStatus;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use futures::channel::mpsc::UnboundedSender;
use serde::Deserialize;
use serde_json::json;
use wasm_rpc::RpcClient;

use crate::{PtyEvent, TerminalBackendEvent, TerminalBounds};

/// Handle to a server-side PTY.
pub struct RemotePty {
    term_id: u64,
    client: RpcClient,
}

#[derive(Deserialize)]
struct OpenResponse {
    term_id: u64,
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

impl RemotePty {
    /// Open a remote PTY on the server and register notification handlers that
    /// pump server output into the terminal's event channel.
    pub async fn open(
        client: RpcClient,
        shell: Option<(String, Vec<String>)>,
        working_directory: Option<std::path::PathBuf>,
        env: HashMap<String, String>,
        initial_bounds: TerminalBounds,
        events_tx: UnboundedSender<PtyEvent>,
    ) -> Result<Self> {
        let shell = shell.map(|(program, args)| json!({ "program": program, "args": args }));
        let response: OpenResponse = client
            .call(
                "Terminal::open",
                &json!({
                    "shell": shell,
                    "working_directory": working_directory.map(|p| p.to_string_lossy().to_string()),
                    "env": env,
                    "cols": initial_bounds.num_columns(),
                    "rows": initial_bounds.num_lines(),
                }),
            )
            .await
            .context("Terminal::open failed")?;

        let term_id = response.term_id;

        // Forward server-side output/exit notifications into the terminal
        // event loop.
        let data_method = format!("Terminal::data:{term_id}");
        let exit_method = format!("Terminal::exit:{term_id}");

        client.on_notification(&data_method, {
            let events_tx = events_tx.clone();
            move |params| {
                let Ok(payload) = serde_json::from_value::<DataNotification>(params) else {
                    return;
                };
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&payload.data)
                else {
                    return;
                };
                let _ = events_tx.unbounded_send(PtyEvent::Bytes(bytes));
            }
        });

        client.on_notification(&exit_method, {
            let events_tx = events_tx.clone();
            move |params| {
                let Ok(_payload) = serde_json::from_value::<ExitNotification>(params) else {
                    return;
                };
                let _ = events_tx.unbounded_send(PtyEvent::Event(TerminalBackendEvent::Exit));
                let _ = events_tx.unbounded_send(PtyEvent::Event(TerminalBackendEvent::ChildExit(
                    ExitStatus::default(),
                )));
            }
        });

        Ok(Self { term_id, client })
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
