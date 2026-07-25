//! Host-installed extensions UI (paint only).
//!
//! Install/search/list all run on the server via `Extensions::*` RPC.
//! Does not open external browser URLs for install.

use std::sync::OnceLock;

use editor::Editor;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Window, div, px,
};
use serde::Deserialize;
use serde_json::json;
use ui::{Button, ButtonStyle, IconButton, Tooltip, prelude::*};
use wasm_remote::RemoteClient;
use workspace::item::ItemEvent;
use workspace::{Item, Workspace};

static REMOTE_CLIENT: OnceLock<RemoteClient> = OnceLock::new();

pub fn set_remote_client(client: RemoteClient) {
    let _ = REMOTE_CLIENT.set(client);
}

fn remote_client() -> Option<RemoteClient> {
    REMOTE_CLIENT.get().cloned()
}

#[derive(Clone, Debug, Deserialize, Default)]
struct ExtInfo {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    installed: bool,
    #[serde(default)]
    provides: Vec<String>,
}

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    extensions: Vec<ExtInfo>,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    extensions: Vec<ExtInfo>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct OpResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    id: String,
    #[serde(default)]
    path: String,
}

pub struct WebExtensionsPage {
    focus_handle: FocusHandle,
    search: Entity<Editor>,
    installed: Vec<ExtInfo>,
    marketplace: Vec<ExtInfo>,
    status: SharedString,
    busy_id: Option<String>,
    tab: ExtTab,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExtTab {
    Installed,
    Marketplace,
}

impl WebExtensionsPage {
    pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let existing = workspace
            .active_pane()
            .read(cx)
            .items()
            .find_map(|item| item.downcast::<WebExtensionsPage>());
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let page = cx.new(|cx| Self::new(window, cx));
        workspace.add_item_to_active_pane(Box::new(page), None, true, window, cx);
    }

    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            let mut e = Editor::single_line(window, cx);
            e.set_placeholder_text("Search extensions…", window, cx);
            e
        });
        let this = Self {
            focus_handle: cx.focus_handle(),
            search,
            installed: Vec::new(),
            marketplace: Vec::new(),
            status: "Loading…".into(),
            busy_id: None,
            tab: ExtTab::Installed,
        };
        this.refresh_installed(cx);
        this
    }

    fn refresh_installed(&self, cx: &mut Context<Self>) {
        let Some(client) = remote_client() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            match client
                .call::<_, ListResponse>("Extensions::list", &json!({}))
                .await
            {
                Ok(resp) => {
                    let _ = this.update(cx, |this, cx| {
                        this.installed = resp.extensions;
                        this.status = format!("{} installed on host", this.installed.len()).into();
                        cx.notify();
                    });
                }
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        this.status = format!("list error: {err:#}").into();
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let q = self.search.read(cx).text(cx).trim().to_string();
        let Some(client) = remote_client() else {
            self.status = "No remote client".into();
            cx.notify();
            return;
        };
        self.tab = ExtTab::Marketplace;
        self.status = "Searching marketplace…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            match client
                .call::<_, SearchResponse>(
                    "Extensions::search",
                    &json!({ "query": q, "max_results": 40 }),
                )
                .await
            {
                Ok(resp) => {
                    let _ = this.update(cx, |this, cx| {
                        this.marketplace = resp.extensions;
                        if let Some(err) = resp.error {
                            this.status = format!("search: {err}").into();
                        } else {
                            this.status =
                                format!("{} marketplace results", this.marketplace.len()).into();
                        }
                        cx.notify();
                    });
                }
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        this.status = format!("search error: {err:#}").into();
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn install(&mut self, id: String, version: String, cx: &mut Context<Self>) {
        let Some(client) = remote_client() else {
            return;
        };
        self.busy_id = Some(id.clone());
        self.status = format!("Installing {id} on host…").into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client
                .call::<_, OpResponse>(
                    "Extensions::install",
                    &json!({ "id": id, "version": version }),
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy_id = None;
                match result {
                    Ok(op) if op.ok => {
                        this.status = format!("Installed {} → {}", op.id, op.path).into();
                        this.refresh_installed(cx);
                        // mark marketplace row installed
                        if let Some(e) = this.marketplace.iter_mut().find(|e| e.id == op.id) {
                            e.installed = true;
                        }
                    }
                    Ok(op) => {
                        this.status = format!(
                            "install failed: {}",
                            op.error.unwrap_or_else(|| "unknown".into())
                        )
                        .into();
                    }
                    Err(err) => {
                        this.status = format!("install error: {err:#}").into();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn uninstall(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(client) = remote_client() else {
            return;
        };
        self.busy_id = Some(id.clone());
        self.status = format!("Uninstalling {id}…").into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client
                .call::<_, OpResponse>("Extensions::uninstall", &json!({ "id": id }))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy_id = None;
                match result {
                    Ok(op) if op.ok => {
                        this.status = format!("Uninstalled {}", op.id).into();
                        this.refresh_installed(cx);
                        if let Some(e) = this.marketplace.iter_mut().find(|e| e.id == op.id) {
                            e.installed = false;
                        }
                    }
                    Ok(op) => {
                        this.status = format!(
                            "uninstall failed: {}",
                            op.error.unwrap_or_else(|| "unknown".into())
                        )
                        .into();
                    }
                    Err(err) => {
                        this.status = format!("uninstall error: {err:#}").into();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl EventEmitter<ItemEvent> for WebExtensionsPage {}

impl Focusable for WebExtensionsPage {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WebExtensionsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = match self.tab {
            ExtTab::Installed => self
                .installed
                .iter()
                .map(|e| {
                    let id = e.id.clone();
                    let busy = self.busy_id.as_deref() == Some(e.id.as_str());
                    h_flex()
                        .w_full()
                        .p_2()
                        .gap_2()
                        .items_start()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    Label::new(format!("{}  v{}", e.name, e.version))
                                        .size(LabelSize::Small)
                                        .weight(gpui::FontWeight::MEDIUM),
                                )
                                .child(
                                    Label::new(if e.description.is_empty() {
                                        e.id.clone()
                                    } else {
                                        e.description.clone()
                                    })
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                ),
                        )
                        .child(
                            Button::new(SharedString::from(format!("uninst-{id}")), "Uninstall")
                                .style(ButtonStyle::Subtle)
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, _w, cx| {
                                    this.uninstall(id.clone(), cx);
                                })),
                        )
                        .into_any_element()
                })
                .collect(),
            ExtTab::Marketplace => self
                .marketplace
                .iter()
                .map(|e| {
                    let id = e.id.clone();
                    let version = e.version.clone();
                    let busy = self.busy_id.as_deref() == Some(e.id.as_str());
                    let installed = e.installed;
                    h_flex()
                        .w_full()
                        .p_2()
                        .gap_2()
                        .items_start()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    Label::new(format!("{}  v{}", e.name, e.version))
                                        .size(LabelSize::Small)
                                        .weight(gpui::FontWeight::MEDIUM),
                                )
                                .child(
                                    Label::new(e.description.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .when(!e.provides.is_empty(), |this| {
                                    this.child(
                                        Label::new(format!("provides: {}", e.provides.join(", ")))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                }),
                        )
                        .child(if installed {
                            Button::new(SharedString::from(format!("inst-{id}")), "Installed")
                                .style(ButtonStyle::Subtle)
                                .disabled(true)
                        } else {
                            Button::new(
                                SharedString::from(format!("inst-{id}")),
                                if busy { "Installing…" } else { "Install" },
                            )
                            .style(ButtonStyle::Filled)
                            .disabled(busy)
                            .on_click(cx.listener(
                                move |this, _, _w, cx| {
                                    this.install(id.clone(), version.clone(), cx);
                                },
                            ))
                        })
                        .into_any_element()
                })
                .collect(),
        };

        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .w_full()
                    .h(px(40.))
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new("Extensions")
                            .size(LabelSize::Default)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new("(host install · no browser keys)")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(self.status.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        div()
                            .flex_1()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .px_2()
                            .child(self.search.clone()),
                    )
                    .child(
                        Button::new("ext-search", "Search")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, _w, cx| this.run_search(cx))),
                    )
                    .child(
                        Button::new("ext-installed", "Installed")
                            .style(if self.tab == ExtTab::Installed {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|this, _, _w, cx| {
                                this.tab = ExtTab::Installed;
                                this.refresh_installed(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("ext-market", "Marketplace")
                            .style(if self.tab == ExtTab::Marketplace {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|this, _, _w, cx| {
                                this.tab = ExtTab::Marketplace;
                                if this.marketplace.is_empty() {
                                    this.run_search(cx);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        IconButton::new("ext-refresh", IconName::ArrowCircle)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh installed"))
                            .on_click(cx.listener(|this, _, _w, cx| {
                                this.refresh_installed(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("ext-list")
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}

impl Item for WebExtensionsPage {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Extensions".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Blocks))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

/// Register Extensions action → open host extensions page (not external URL).
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _action: &zed_actions::Extensions, window, cx| {
            WebExtensionsPage::open(workspace, window, cx);
        });
    })
    .detach();
}
