use futures::{
    Sink, Stream, StreamExt as _,
    channel::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use std::pin::Pin;
use std::task::{Context, Poll};
use tungstenite::Message as WebSocketMessage;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, Event, MessageEvent, WebSocket};

/// A futures `Stream` + `Sink` backed by a browser `WebSocket`.
pub struct WasmWebSocket {
    socket: WebSocket,
    outgoing_tx: UnboundedSender<WebSocketMessage>,
    incoming_rx: UnboundedReceiver<anyhow::Result<WebSocketMessage>>,
    _closures: Closures,
}

struct Closures {
    _on_open: Closure<dyn FnMut(Event)>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(Event)>,
    _on_error: Closure<dyn FnMut(Event)>,
}

impl WasmWebSocket {
    /// Opens a WebSocket to the given URL.
    pub fn new(url: &str) -> anyhow::Result<Self> {
        let socket = WebSocket::new(url).map_err(|e| {
            anyhow::anyhow!(
                "failed to create WebSocket: {}",
                js_error_string(e).unwrap_or_default()
            )
        })?;
        socket.set_binary_type(BinaryType::Arraybuffer);

        let (incoming_tx, incoming_rx) = mpsc::unbounded::<anyhow::Result<WebSocketMessage>>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<WebSocketMessage>();

        let socket_for_outgoing = socket.clone();
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(msg) = outgoing_rx.next().await {
                if send_to_socket(&socket_for_outgoing, msg).is_err() {
                    break;
                }
            }
        });

        let on_open = Closure::wrap(Box::new(move |_event: Event| {}) as Box<dyn FnMut(_)>);
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));

        let incoming_for_message = incoming_tx.clone();
        let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
            let msg = decode_message(event);
            let _ = incoming_for_message.unbounded_send(msg);
        }) as Box<dyn FnMut(_)>);
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let incoming_for_close = incoming_tx.clone();
        let on_close = Closure::wrap(Box::new(move |_event: Event| {
            let _ = incoming_for_close.unbounded_send(Err(anyhow::anyhow!("WebSocket closed")));
        }) as Box<dyn FnMut(_)>);
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        let on_error = Closure::wrap(Box::new(move |_event: Event| {
            let _ = incoming_tx.unbounded_send(Err(anyhow::anyhow!("WebSocket error")));
        }) as Box<dyn FnMut(_)>);
        socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        Ok(WasmWebSocket {
            socket,
            outgoing_tx,
            incoming_rx,
            _closures: Closures {
                _on_open: on_open,
                _on_message: on_message,
                _on_close: on_close,
                _on_error: on_error,
            },
        })
    }
}

fn decode_message(event: MessageEvent) -> anyhow::Result<WebSocketMessage> {
    if let Ok(array) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
        let uint8 = js_sys::Uint8Array::new(&array);
        let mut bytes = vec![0; uint8.length() as usize];
        uint8.copy_to(&mut bytes);
        Ok(WebSocketMessage::Binary(bytes.into()))
    } else if let Ok(text) = event.data().dyn_into::<js_sys::JsString>() {
        Ok(WebSocketMessage::Text(String::from(text).into()))
    } else {
        Err(anyhow::anyhow!("unsupported WebSocket message type"))
    }
}

fn send_to_socket(socket: &WebSocket, msg: WebSocketMessage) -> Result<(), anyhow::Error> {
    match msg {
        WebSocketMessage::Text(text) => socket.send_with_str(&text).map_err(|e| {
            anyhow::anyhow!(
                "failed to send text: {}",
                js_error_string(e).unwrap_or_default()
            )
        }),
        WebSocketMessage::Binary(bytes) => socket.send_with_u8_array(&bytes).map_err(|e| {
            anyhow::anyhow!(
                "failed to send binary: {}",
                js_error_string(e).unwrap_or_default()
            )
        }),
        WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) | WebSocketMessage::Frame(_) => {
            Ok(())
        }
        WebSocketMessage::Close(_) => {
            let _ = socket.close();
            Ok(())
        }
    }
}

fn js_error_string(value: JsValue) -> Option<String> {
    value
        .dyn_into::<js_sys::Error>()
        .ok()
        .and_then(|e| e.to_string().as_string())
}

impl Stream for WasmWebSocket {
    type Item = anyhow::Result<WebSocketMessage>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.incoming_rx.poll_next_unpin(cx)
    }
}

impl Sink<WebSocketMessage> for WasmWebSocket {
    type Error = anyhow::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.socket.ready_state() {
            WebSocket::CONNECTING => Poll::Pending,
            WebSocket::OPEN => Poll::Ready(Ok(())),
            _ => Poll::Ready(Err(anyhow::anyhow!("WebSocket is not open"))),
        }
    }

    fn start_send(self: Pin<&mut Self>, item: WebSocketMessage) -> Result<(), Self::Error> {
        self.outgoing_tx
            .unbounded_send(item)
            .map_err(|_| anyhow::anyhow!("WebSocket outgoing channel closed"))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.socket.ready_state() != WebSocket::CLOSED {
            let _ = self.socket.close();
        }
        Poll::Ready(Ok(()))
    }
}

/// Opens a WebSocket connection and returns an `rpc::Connection`.
pub fn connect(url: &str) -> anyhow::Result<crate::Connection> {
    let ws = WasmWebSocket::new(url)?;
    Ok(crate::Connection::new(ws))
}
