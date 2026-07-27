//! Desktop-style account / settings dropdown for the web title bar.
//!
//! Mirrors `title_bar::TitleBar::render_user_menu_button` without the full
//! title_bar crate (collab / account / updates don't compile for wasm).

use agent_settings::{AgentSettings, WindowLayout};
use editor::Editor;
use fs::Fs;
use gpui::{
    Action, Anchor, AnyElement, App, Context, DismissEvent, Entity, EventEmitter, Focusable,
    IntoElement, Render, SharedString, WeakEntity, Window, actions,
};
use settings::Settings as _;
use ui::{ButtonLike, ContextMenu, ContextMenuEntry, IconPosition, PopoverMenu, prelude::*};
use workspace::{ModalView, Workspace};
use zed_actions;

actions!(
    workspace,
    [
        /// Switches to the classic, editor-focused panel layout.
        UseClassicLayout,
        /// Switches to the agentic panel layout.
        UseAgenticLayout,
        /// Relays a localhost OAuth callback to an ACP agent running on the host.
        CompleteAcpLogin,
    ]
);

pub fn init(cx: &mut App) {
    // The keymap editor handles OpenKeymap. Keep the explicit file action here
    // so users can still edit keymap.json directly, as on desktop.
    // NOTE: `OpenSettings` / `OpenSettingsPage` / `OpenSettingsAt` /
    // `OpenProjectSettings` are handled by the real `settings_ui` GUI window
    // (initialized before this). These JSON-file handlers remain only for the
    // explicit "open settings.json / keymap.json as text" actions.
    cx.on_action(|_: &zed_actions::OpenSettingsFile, cx| {
        with_workspace(cx, |workspace, window, cx| {
            open_config_file(
                paths::settings_file(),
                settings::initial_user_settings_content().as_ref(),
                workspace,
                window,
                cx,
            );
        });
    });
    cx.on_action(|_: &zed_actions::OpenKeymapFile, cx| {
        with_workspace(cx, |workspace, window, cx| {
            open_config_file(
                paths::keymap_file(),
                settings::initial_keymap_content().as_ref(),
                workspace,
                window,
                cx,
            );
        });
    });
    cx.on_action(|_: &CompleteAcpLogin, cx| {
        with_workspace(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |window, cx| AcpCallbackModal::new(window, cx));
        });
    });
    // Extensions action is handled by web_extensions::init → host install page.

    // Panel layout actions (registered on every workspace, same as desktop).
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|_ws, _: &UseClassicLayout, _window, cx| {
            set_window_layout(WindowLayout::Editor(None), cx);
        });
        workspace.register_action(|_ws, _: &UseAgenticLayout, _window, cx| {
            set_window_layout(WindowLayout::Agent(None), cx);
        });
    })
    .detach();
}

fn set_window_layout(layout: WindowLayout, cx: &App) {
    let fs = <dyn Fs>::global(cx);
    drop(AgentSettings::set_layout(layout, fs, cx));
}

fn with_workspace(
    cx: &mut App,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + Send + 'static,
) {
    workspace::with_active_or_new_workspace(cx, f);
}

fn open_config_file(
    abs_path: &std::path::Path,
    default_content: &str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let abs_path = abs_path.to_path_buf();
    let default_content = default_content.to_string();
    let fs = workspace.app_state().fs.clone();

    cx.spawn_in(window, async move |workspace, cx| {
        // Ensure the file exists on the remote host.
        if !fs.is_file(&abs_path).await {
            if let Some(parent) = abs_path.parent() {
                let _ = fs.create_dir(parent).await;
            }
            let _ = fs.atomic_write(abs_path.clone(), default_content).await;
        }

        let open_path = abs_path.clone();
        let _ = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    open_path,
                    workspace::OpenOptions {
                        visible: Some(workspace::OpenVisible::All),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })?
            .await;
        anyhow::Ok(())
    })
    .detach();
}

/// Chevron-down account menu: Settings, Keymap, Themes, Icon Themes,
/// Extensions, Panel Layout (Classic / Agentic).
pub fn render_user_menu_button(_workspace: WeakEntity<Workspace>, _cx: &mut App) -> AnyElement {
    let trigger = ButtonLike::new("user-menu")
        .aria_label("User menu")
        .tab_index(0isize)
        .child(Icon::new(IconName::ChevronDown).size(IconSize::Small));

    PopoverMenu::new("user-menu")
        .trigger(trigger)
        .menu(move |window, cx| {
            let ai_enabled = !project::DisableAiSettings::get_global(cx).disable_ai;
            let current_layout = AgentSettings::get_layout(cx);
            let is_editor = matches!(current_layout, WindowLayout::Editor(_));
            let is_agent = matches!(current_layout, WindowLayout::Agent(_));
            let is_custom = matches!(current_layout, WindowLayout::Custom(_));

            ContextMenu::build(window, cx, |menu, _, _cx| {
                menu.action("Settings", zed_actions::OpenSettings.boxed_clone())
                    .action("Keymap", Box::new(zed_actions::OpenKeymap))
                    .action(
                        "Themes…",
                        zed_actions::theme_selector::Toggle::default().boxed_clone(),
                    )
                    .action(
                        "Icon Themes…",
                        zed_actions::icon_theme_selector::Toggle::default().boxed_clone(),
                    )
                    .action(
                        "Extensions",
                        zed_actions::Extensions::default().boxed_clone(),
                    )
                    .action("Complete ACP Login…", CompleteAcpLogin.boxed_clone())
                    .when(ai_enabled, |menu| {
                        menu.separator()
                            .submenu("Panel Layout", move |menu, _window, _cx| {
                                menu.toggleable_entry(
                                    "Classic",
                                    is_editor,
                                    IconPosition::Start,
                                    Some(UseClassicLayout.boxed_clone()),
                                    move |window, cx| {
                                        window.dispatch_action(UseClassicLayout.boxed_clone(), cx);
                                    },
                                )
                                .toggleable_entry(
                                    "Agentic",
                                    is_agent,
                                    IconPosition::Start,
                                    Some(UseAgenticLayout.boxed_clone()),
                                    move |window, cx| {
                                        window.dispatch_action(UseAgenticLayout.boxed_clone(), cx);
                                    },
                                )
                                .when(is_custom, |menu| {
                                    menu.item(
                                        ContextMenuEntry::new("Custom")
                                            .toggleable(IconPosition::Start, true)
                                            .disabled(true),
                                    )
                                })
                            })
                    })
                    .separator()
                    .custom_entry(
                        move |_window, _cx| {
                            h_flex()
                                .w_full()
                                .child(Label::new("Sign In").color(Color::Muted))
                                .into_any_element()
                        },
                        move |_, cx| {
                            // Account auth needs the collab client; open the
                            // account page until web sign-in is wired.
                            cx.open_url("https://zed.dev/sign_in");
                        },
                    )
            })
            .into()
        })
        .anchor(Anchor::TopRight)
        .into_any_element()
}

struct AcpCallbackModal {
    editor: Entity<Editor>,
    status: Option<SharedString>,
    submitting: bool,
}

impl AcpCallbackModal {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("http://localhost:PORT/auth/callback?...", window, cx);
            editor
        });
        Self {
            editor,
            status: None,
            submitting: false,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        if self.submitting {
            return;
        }
        let callback = self.editor.read(cx).text(cx).trim().to_string();
        if callback.is_empty() {
            return;
        }
        let Some(client) = wasm_remote::remote_client() else {
            self.status = Some("RPC connection is unavailable".into());
            cx.notify();
            return;
        };

        self.submitting = true;
        self.status = Some("Forwarding callback…".into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client
                .call_void(
                    "Browser::relay_localhost_callback",
                    &serde_json::json!({"url": callback}),
                )
                .await;
            this.update(cx, |this, cx| {
                this.submitting = false;
                match result {
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => {
                        this.status = Some(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }
}

impl EventEmitter<DismissEvent> for AcpCallbackModal {}
impl ModalView for AcpCallbackModal {}

impl Focusable for AcpCallbackModal {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for AcpCallbackModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("AcpCallbackModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(gpui::px(620.))
            .overflow_hidden()
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.editor.clone())
                    .child(
                        Button::new("relay-acp-callback", "Complete Login")
                            .disabled(self.submitting)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
            .when_some(self.status.clone(), |this, status| {
                this.child(
                    div().px_2().py_1().child(
                        Label::new(status)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
    }
}
