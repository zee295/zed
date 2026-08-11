//! Hosts the real `settings_ui::SettingsWindow` as a workspace modal (popup).
//!
//! Desktop opens the settings GUI as a separate OS window, which the
//! single-canvas web build can't do. This wraps it in a `ModalView` so the same
//! full settings window renders as an in-window popup over the workspace.

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Subscription,
    Window,
};
use settings_ui::SettingsWindow;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{DismissDecision, ModalView, Workspace};

pub struct SettingsModal {
    settings: Entity<SettingsWindow>,
    focus_handle: FocusHandle,
    _dismiss_subscription: Subscription,
}

impl SettingsModal {
    pub fn new(settings: Entity<SettingsWindow>, cx: &mut Context<Self>) -> Self {
        let dismiss_subscription = cx.subscribe(&settings, |_this, _, _: &DismissEvent, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            settings,
            focus_handle: cx.focus_handle(),
            _dismiss_subscription: dismiss_subscription,
        }
    }
}

impl EventEmitter<DismissEvent> for SettingsModal {}

impl Focusable for SettingsModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // Forward focus into the settings window so its search bar / nav work.
        self.settings.focus_handle(cx)
    }
}

impl ModalView for SettingsModal {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> DismissDecision {
        DismissDecision::Dismiss(true)
    }

    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Render for SettingsModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(gpui::px(900.0))
            .h(gpui::px(600.0))
            .overflow_hidden()
            .rounded_lg()
            .child(self.settings.clone())
            // Close (X) button, top-right. Emits `DismissEvent`, which the modal
            // layer subscribes to and uses to hide the modal.
            .child(
                div().absolute().top_2().right_2().child(
                    IconButton::new("close-settings", IconName::Close)
                        .tooltip(move |_window, cx| Tooltip::simple("Close settings", cx))
                        .on_click(cx.listener(|_this, _event, _window, cx| {
                            cx.emit(DismissEvent);
                        })),
                ),
            )
    }
}

/// Opens the settings GUI as a modal popup. Called from a workspace action
/// handler, which provides `&mut Workspace` + `&mut Window` + `&mut
/// Context<Workspace>` directly — so no nested `workspace.update` is needed
/// (nesting would double-lease `Workspace` when `SettingsWindow::new` reads it).
pub fn open_settings_popup(
    original_window: Option<gpui::WindowHandle<workspace::MultiWorkspace>>,
    page: Option<String>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    log::info!("zed_web_workspace: open_settings_popup dispatched");
    // Two-phase, to avoid a `Workspace` double-lease:
    //   1. `spawn_in` runs AFTER the action handler's `Context<Workspace>` borrow
    //      is released. `new_window_entity` builds `SettingsWindow` via
    //      `update_window` (no `Workspace` lease) — safe, since it reads
    //      `Workspace` via AppState/settings fetch.
    //   2. `defer_in` then presents the pre-built entity via `toggle_modal`.
    let window_handle = window.window_handle();
    let context_server_store = workspace.project().read(cx).context_server_store();
    cx.spawn_in(window, {
        let original_window = original_window.clone();
        let page = page.clone();
        let context_server_store = context_server_store.clone();
        async move |_workspace, cx| {
            // Phase 1: build SettingsWindow lease-free via `new_window_entity`
            // (uses `update_window`, which doesn't hold a `Workspace` lease).
            let settings = cx.new_window_entity(|window, cx| {
                let mut settings = SettingsWindow::new_modal(original_window, window, cx);
                settings.set_context_server_store(context_server_store);
                if let Some(page) = page.as_deref() {
                    settings.open_page(page, window, cx);
                }
                settings
            });
            let Ok(settings) = settings else {
                log::error!("settings popup: failed to build SettingsWindow");
                return;
            };
            // Phase 2: present it. The builder only clones the pre-built entity
            // (it does NOT read `Workspace`), so no double-lease.
            let result = cx.update_window(window_handle, |root, window, cx| {
                // The window root may be a plain `Workspace` or a `MultiWorkspace`
                // (the web build uses MultiWorkspace). Resolve the active
                // `Workspace` from either. The builder only clones the pre-built
                // entity (no `Workspace` read), so no double-lease.
                // `AnyView::downcast` consumes the view, returning it on
                // mismatch, so chain: try Workspace, else MultiWorkspace.
                let workspace = match root.downcast::<Workspace>() {
                    Ok(ws) => Some(ws),
                    Err(root) => root
                        .downcast::<workspace::MultiWorkspace>()
                        .ok()
                        .map(|mw| mw.read(cx).workspace().clone()),
                };
                let Some(workspace) = workspace else {
                    log::error!("settings popup: window root is not a Workspace/MultiWorkspace");
                    return;
                };
                workspace.update(cx, |workspace, cx| {
                    workspace
                        .toggle_modal(window, cx, |_window, cx| SettingsModal::new(settings, cx));
                });
            });
            if let Err(err) = result {
                log::error!("settings popup: present failed: {err:#}");
            }
        }
    })
    .detach();
}
