use anyhow::{Result, anyhow};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
#[cfg(target_family = "wasm")]
use std::sync::TryLockError;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

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

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
export function zedRpcCreate(url, onOpen, onMessage, onClose, onError) {
    let workspaceKey;
    try {
        const pageUrl = new URL(self.location.href);
        const paths = pageUrl.searchParams.getAll("path");
        workspaceKey = JSON.stringify(paths.length ? paths : [pageUrl.pathname]);
    } catch (_) {
        workspaceKey = self.location?.pathname ?? "/workspace";
    }
    const sessionId = `workspace:${self.location?.origin ?? "local"}:${workspaceKey}`;
    const state = {
        active: true,
        attempt: 0,
        queue: [],
        reconnectTimer: 0,
        sessionId,
        socket: null,
    };

    const identify = message => {
        const envelope = JSON.parse(message);
        envelope.session_id = state.sessionId;
        return JSON.stringify(envelope);
    };

    const scheduleReconnect = () => {
        if (!state.active || state.reconnectTimer) return;
        const base = Math.min(10000, 250 * (2 ** Math.min(state.attempt, 6)));
        const delay = Math.round(base * (0.8 + Math.random() * 0.4));
        state.attempt += 1;
        state.reconnectTimer = self.setTimeout(() => {
            state.reconnectTimer = 0;
            connect();
        }, delay);
    };

    const connect = () => {
        if (!state.active) return;
        if (self.navigator && !self.navigator.onLine) {
            self.__zedRpcConnectionState = "reconnecting";
            return;
        }
        let socket;
        try {
            socket = new WebSocket(url);
        } catch (error) {
            onError(String(error));
            scheduleReconnect();
            return;
        }
        state.socket = socket;
        socket.binaryType = "arraybuffer";
        socket.onopen = () => {
            if (state.socket !== socket) return;
            state.attempt = 0;
            self.__zedRpcConnectionState = "open";
            onOpen();
            while (state.queue.length && socket.readyState === WebSocket.OPEN) {
                socket.send(state.queue.shift());
            }
        };
        socket.onmessage = event => onMessage(event.data);
        socket.onerror = () => onError("WebSocket transport error");
        socket.onclose = event => {
            if (state.socket !== socket) return;
            state.socket = null;
            self.__zedRpcConnectionState = "reconnecting";
            onClose(event.code, event.reason || "connection closed");
            scheduleReconnect();
        };
    };

    const online = () => {
        if (!state.active || state.socket) return;
        if (state.reconnectTimer) {
            self.clearTimeout(state.reconnectTimer);
            state.reconnectTimer = 0;
        }
        connect();
    };
    const offline = () => {
        if (!state.active) return;
        self.__zedRpcConnectionState = "reconnecting";
        state.socket?.close(4000, "browser offline");
    };
    self.addEventListener?.("online", online);
    self.addEventListener?.("offline", offline);
    self.__zedRpcConnectionState = "connecting";
    connect();

    return {
        send(message) {
            message = identify(message);
            const socket = state.socket;
            if (socket?.readyState === WebSocket.OPEN) {
                socket.send(message);
                return;
            }
            if (state.queue.length >= 10000) {
                throw new Error("RPC reconnect queue is full");
            }
            state.queue.push(message);
        },
        close() {
            state.active = false;
            self.__zedRpcConnectionState = "closed";
            if (state.reconnectTimer) self.clearTimeout(state.reconnectTimer);
            self.removeEventListener?.("online", online);
            self.removeEventListener?.("offline", offline);
            state.socket?.close(1000, "client closed");
            state.socket = null;
            state.queue.length = 0;
        },
    };
}

export function zedRpcSend(client, message) {
    client.send(message);
}
"#)]
extern "C" {
    type ReconnectingSocket;

    #[wasm_bindgen::prelude::wasm_bindgen(catch, js_name = zedRpcCreate)]
    fn create_reconnecting_socket(
        url: &str,
        on_open: &js_sys::Function,
        on_message: &js_sys::Function,
        on_close: &js_sys::Function,
        on_error: &js_sys::Function,
    ) -> std::result::Result<ReconnectingSocket, wasm_bindgen::JsValue>;

    #[wasm_bindgen::prelude::wasm_bindgen(catch, js_name = zedRpcSend)]
    fn send_reconnecting_socket(
        socket: &ReconnectingSocket,
        message: &str,
    ) -> std::result::Result<(), wasm_bindgen::JsValue>;
}

#[derive(Serialize)]
struct RequestEnvelope<P: Serialize> {
    id: u64,
    method: String,
    #[serde(flatten)]
    payload: RequestPayload<P>,
}

#[derive(Serialize)]
struct RequestPayload<P: Serialize> {
    params: P,
}

#[derive(Deserialize)]
struct ResponseEnvelope {
    id: u64,
    result: Option<Value>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct NotificationEnvelope {
    method: String,
    #[serde(default)]
    params: Value,
}

/// A JSON-RPC-over-WebSocket client that works in the browser.
///
/// It is `Send` so it can be used inside `#[async_trait]` implementations
/// (even though WASM is single-threaded, the generated futures carry a
/// `Send` bound).
#[derive(Clone)]
pub struct RpcClient {
    outgoing: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    notifications: Arc<Mutex<HashMap<String, Box<dyn Fn(Value) + Send>>>>,
    next_id: Arc<AtomicU64>,
    /// `true` once the browser WebSocket reaches the OPEN state.
    is_open: Arc<AtomicBool>,
    open: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

impl RpcClient {
    /// Open a WebSocket to `url` and start the read/write pumps.
    #[cfg(target_family = "wasm")]
    pub fn connect(url: &str) -> Result<Self> {
        use wasm_bindgen::JsCast;
        use wasm_bindgen::prelude::*;

        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<String>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notifications: Arc<Mutex<HashMap<String, Box<dyn Fn(Value) + Send>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let is_open = Arc::new(AtomicBool::new(false));

        // Gate the outgoing pump on the open event so we never drop messages
        // that race the CONNECTING → OPEN transition.
        let (gate_tx, gate_rx) = oneshot::channel::<()>();
        let gate_tx = Arc::new(Mutex::new(Some(gate_tx)));

        // Connection-open signal.
        let (open_tx, open_rx) = oneshot::channel::<()>();
        let open_tx = Arc::new(Mutex::new(Some(open_tx)));

        let mark_open = {
            let is_open = is_open.clone();
            let open_tx = open_tx.clone();
            let gate_tx = gate_tx.clone();
            move || {
                is_open.store(true, Ordering::SeqCst);
                if let Some(sender) = lock_shared(&open_tx).take() {
                    let _ = sender.send(());
                }
                if let Some(sender) = lock_shared(&gate_tx).take() {
                    let _ = sender.send(());
                }
            }
        };

        let onopen = Closure::<dyn FnMut()>::new(mark_open);

        // Incoming pump.
        let pending_for_messages = pending.clone();
        let notifications_for_messages = notifications.clone();
        let onmessage = Closure::<dyn FnMut(_)>::new(move |data: JsValue| {
            let text = if let Ok(s) = data.clone().dyn_into::<js_sys::JsString>() {
                String::from(s)
            } else if let Ok(buf) = data.clone().dyn_into::<js_sys::ArrayBuffer>() {
                let arr = js_sys::Uint8Array::new(&buf);
                let mut bytes = vec![0u8; arr.length() as usize];
                arr.copy_to(&mut bytes);
                match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => {
                        web_sys::console::error_1(&"wasm_rpc: non-utf8 binary message".into());
                        return;
                    }
                }
            } else {
                web_sys::console::error_1(
                    &format!("wasm_rpc: unsupported message data: {data:?}").into(),
                );
                return;
            };

            // Prefer notifications when `method` is present and `id` is absent —
            // otherwise a mis-parse can drop Terminal::data / Process::stdout.
            let parsed: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(err) => {
                    web_sys::console::error_1(
                        &format!("wasm_rpc: invalid JSON: {err}; raw={text}").into(),
                    );
                    return;
                }
            };

            let has_id = parsed.get("id").map(|v| !v.is_null()).unwrap_or(false);
            let has_method = parsed.get("method").and_then(|v| v.as_str()).is_some();

            if has_method && !has_id {
                if let Ok(notification) =
                    serde_json::from_value::<NotificationEnvelope>(parsed.clone())
                {
                    let handlers = lock_shared(&notifications_for_messages);
                    if let Some(handler) = handlers.get(&notification.method) {
                        handler(notification.params);
                    } else {
                        web_sys::console::warn_1(
                            &format!(
                                "wasm_rpc: no handler for notification {}",
                                notification.method
                            )
                            .into(),
                        );
                    }
                    return;
                }
            }

            if has_id {
                if let Ok(response) = serde_json::from_value::<ResponseEnvelope>(parsed) {
                    let mut pending = lock_shared(&pending_for_messages);
                    if let Some(sender) = pending.remove(&response.id) {
                        let result = if let Some(err) = response.error {
                            Err(err)
                        } else {
                            Ok(response.result.unwrap_or(Value::Null))
                        };
                        sender.send(result).ok();
                    }
                    return;
                }
            }

            web_sys::console::error_1(&format!("wasm_rpc: unrecognized message: {}", text).into());
        });

        let pending_for_close = pending.clone();
        let is_open_for_close = is_open.clone();
        let onclose = Closure::<dyn FnMut(u16, String)>::new(move |code, reason| {
            is_open_for_close.store(false, Ordering::SeqCst);
            let mut pending = lock_shared(&pending_for_close);
            for (_, sender) in pending.drain() {
                sender
                    .send(Err(format!(
                        "WebSocket disconnected ({code}): {reason}; reconnecting"
                    )))
                    .ok();
            }
        });

        let onerror = Closure::<dyn FnMut(String)>::new(move |message| {
            web_sys::console::error_1(&format!("wasm_rpc: {message}").into());
        });

        let socket = create_reconnecting_socket(
            url,
            onopen.as_ref().unchecked_ref(),
            onmessage.as_ref().unchecked_ref(),
            onclose.as_ref().unchecked_ref(),
            onerror.as_ref().unchecked_ref(),
        )
        .map_err(|error| anyhow!("failed to create WebSocket: {error:?}"))?;
        onopen.forget();
        onmessage.forget();
        onclose.forget();
        onerror.forget();

        wasm_bindgen_futures::spawn_local(async move {
            let _ = gate_rx.await;
            while let Some(message) = outgoing_rx.next().await {
                if let Err(error) = send_reconnecting_socket(&socket, &message) {
                    web_sys::console::error_1(
                        &format!("wasm_rpc: WebSocket queue failed: {error:?}").into(),
                    );
                }
            }
        });

        Ok(Self {
            outgoing: outgoing_tx,
            pending,
            notifications,
            next_id: Arc::new(AtomicU64::new(1)),
            is_open,
            open: Arc::new(Mutex::new(Some(open_rx))),
        })
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn connect(_url: &str) -> Result<Self> {
        Err(anyhow!("RpcClient is only available on WASM"))
    }

    async fn wait_until_open(&self) {
        let receiver = {
            let mut guard = lock_shared(&self.open);
            guard.take()
        };
        if let Some(receiver) = receiver {
            let _ = receiver.await;
        }
    }

    /// Make a remote procedure call and wait for a JSON response.
    pub async fn call<P: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        self.wait_until_open().await;

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = RequestEnvelope {
            id,
            method: method.to_string(),
            payload: RequestPayload { params },
        };
        let json = serde_json::to_string(&request)?;

        let (tx, rx) = oneshot::channel();
        {
            lock_shared(&self.pending).insert(id, tx);
        }

        self.outgoing
            .unbounded_send(json)
            .map_err(|_| anyhow!("remote outgoing channel closed"))?;

        match rx.await {
            Ok(Ok(value)) => {
                serde_json::from_value(value).map_err(|e| anyhow!("deserialize error: {}", e))
            }
            Ok(Err(message)) => Err(anyhow!("remote error: {}", message)),
            Err(_) => Err(anyhow!("remote request canceled")),
        }
    }

    pub async fn call_void<P: Serialize>(&self, method: &str, params: &P) -> Result<()> {
        self.call::<_, serde_json::Value>(method, params).await?;
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.is_open.load(Ordering::SeqCst)
    }

    pub fn on_notification<F: Fn(Value) + Send + 'static>(&self, method: &str, handler: F) {
        lock_shared(&self.notifications).insert(method.to_string(), Box::new(handler));
    }
}
