#![cfg_attr(target_arch = "wasm32", no_main)]

use gpui::prelude::*;
use gpui::{
    App, Bounds, Context, ElementId, SharedString, Window, WindowBounds, WindowOptions, div, px,
    rgb, size,
};
use serde::{Deserialize, Serialize};

#[cfg(target_arch = "wasm32")]
use futures::StreamExt;

#[cfg(target_arch = "wasm32")]
use fs::Fs as _;
#[cfg(target_arch = "wasm32")]
use std::{path::Path, sync::Arc};

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    name: String,
    is_dir: bool,
}

struct ZedWebApp {
    status: SharedString,
    files: Vec<FileEntry>,
    content: SharedString,
    #[cfg(target_arch = "wasm32")]
    fs: Option<Arc<wasm_remote::RemoteFs>>,
}

impl ZedWebApp {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            status: "Disconnected".into(),
            files: Vec::new(),
            content: "Click Connect to load files from the remote server.".into(),
            #[cfg(target_arch = "wasm32")]
            fs: None,
        };
        #[cfg(target_arch = "wasm32")]
        this.connect(cx);
        this
    }

    #[cfg(target_arch = "wasm32")]
    fn connect(&mut self, cx: &mut Context<Self>) {
        self.set_status("Connecting...", cx);

        cx.spawn(async move |this, cx| {
            let client = match wasm_remote::RemoteClient::connect("ws://127.0.0.1:8080/rpc") {
                Ok(client) => client,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.set_status(format!("WebSocket error: {}", err), cx);
                    })
                    .ok();
                    return;
                }
            };

            #[cfg(target_arch = "wasm32")]
            smol::set_remote_client(client.clone());

            let fs = Arc::new(wasm_remote::RemoteFs::new(client));

            this.update(cx, |this, _cx| {
                this.fs = Some(fs.clone());
            })
            .ok();

            let mut stream = match fs.read_dir(Path::new(".")).await {
                Ok(stream) => stream,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.set_status(format!("read_dir failed: {}", err), cx);
                    })
                    .ok();
                    return;
                }
            };

            let mut entries = Vec::new();
            while let Some(result) = stream.next().await {
                match result {
                    Ok(path) => {
                        let name = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        if name.is_empty() {
                            continue;
                        }
                        let is_dir = fs.is_dir(Path::new(&path)).await;
                        entries.push(FileEntry { name, is_dir });
                    }
                    Err(err) => {
                        web_sys::console::error_1(&format!("read_dir entry error: {}", err).into());
                    }
                }
            }

            this.update(cx, |this, cx| {
                this.handle_files(entries, cx);
            })
            .ok();
        })
        .detach();
    }

    fn handle_files(&mut self, files: Vec<FileEntry>, cx: &mut Context<Self>) {
        self.files = files;
        self.status = format!("Connected — {} files", self.files.len()).into();
        cx.notify();
    }

    fn handle_content(&mut self, content: Result<String, String>, cx: &mut Context<Self>) {
        self.content = match content {
            Ok(text) => text.into(),
            Err(err) => format!("Error: {}", err).into(),
        };
        cx.notify();
    }

    fn set_status(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.status = message.into().into();
        cx.notify();
    }
}

impl Render for ZedWebApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status.clone();
        let content = self.content.clone();
        let files = self.files.clone();

        let header = div()
            .flex()
            .flex_row()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_xl()
                    .text_color(rgb(0xcdd6f4))
                    .child("Zed Remote — GPUI Web"),
            )
            .child(
                div()
                    .id("connect-btn")
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(0x45475a))
                    .text_color(rgb(0xcdd6f4))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        #[cfg(target_arch = "wasm32")]
                        this.connect(cx);
                    }))
                    .child("Connect"),
            );

        let status_bar = div()
            .text_sm()
            .text_color(rgb(0xa6adc8))
            .child(status.to_string());

        let file_list = files.into_iter().enumerate().fold(
            div()
                .id("file-list")
                .flex()
                .flex_col()
                .gap_1()
                .w(px(280.))
                .h_full()
                .overflow_y_scroll(),
            |col, (index, file)| {
                let color = if file.is_dir { 0x89b4fa } else { 0xa6e3a1 };
                let name = file.name.clone();
                col.child(
                    div()
                        .id(ElementId::NamedInteger("file".into(), index as u64))
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .text_color(rgb(color))
                        .font_family("monospace")
                        .hover(|style| style.bg(rgb(0x313244)))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            if !file.is_dir {
                                this.content = format!("Loading {}...", name).into();
                                cx.notify();
                                #[cfg(target_arch = "wasm32")]
                                load_file(this, &name, cx);
                            }
                        }))
                        .child(file.name),
                )
            },
        );

        let content_view = div()
            .id("content-view")
            .flex_1()
            .h_full()
            .p_4()
            .rounded_lg()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .font_family("monospace")
            .text_sm()
            .overflow_y_scroll()
            .child(content.to_string());

        div()
            .flex()
            .flex_col()
            .size_full()
            .p_4()
            .gap_4()
            .bg(rgb(0x11111b))
            .child(header)
            .child(status_bar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .gap_4()
                    .overflow_hidden()
                    .child(file_list)
                    .child(content_view),
            )
    }
}

#[cfg(target_arch = "wasm32")]
fn load_file(this: &mut ZedWebApp, name: &str, cx: &mut Context<ZedWebApp>) {
    let name = name.to_string();
    let fs = this.fs.clone();
    cx.spawn(async move |this, cx| {
        let Some(fs) = fs else {
            this.update(cx, |this, cx| {
                this.handle_content(Err("not connected".to_string()), cx);
            })
            .ok();
            return;
        };

        let result = fs.load(Path::new(&name)).await.map_err(|e| e.to_string());
        this.update(cx, |this, cx| {
            this.handle_content(result, cx);
        })
        .ok();
    })
    .detach();
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(start)]
pub fn main() {
    gpui_platform::web_init();
    let handle = gpui_platform::single_threaded_web().run_embedded(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(ZedWebApp::new),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
    std::mem::forget(handle);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!("This binary is only meant to run as WASM in a browser.");
}
