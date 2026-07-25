//! Pane-toolbar quick action bar (web) — the buttons under the file tab:
//! preview, buffer search, inline assist (agent sparkle), selections menu,
//! editor settings menu. A slim port of desktop `zed::quick_action_bar`
//! (which is a zed-binary module that pulls vim and can't link on wasm).

use agent_settings::AgentSettings;
use editor::actions::*;
use editor::{Editor, EditorSettings};
use gpui::{
    Action as _, App, Context, Entity, EventEmitter, FocusHandle, Focusable as _, Render,
    SharedString, Subscription, Task, WeakEntity, Window, actions,
};
use project::DisableAiSettings;
use search::{BufferSearchBar, buffer_search};
use settings::{Settings, SettingsStore};
use ui::{
    ButtonStyle, ContextMenu, IconButtonShape, IconPosition, PopoverMenu, PopoverMenuHandle,
    Tooltip, prelude::*,
};
use workspace::item::ItemBufferKind;
use workspace::{
    ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace, item::ItemHandle,
};
use zed_actions::{agent::AddSelectionToThread, assistant::InlineAssist, outline::ToggleOutline};

enum PreviewTarget {
    Markdown(Entity<Editor>),
    Svg(Entity<multi_buffer::MultiBuffer>),
}

pub struct WebQuickActionBar {
    _ai_settings_subscription: Subscription,
    active_item: Option<Box<dyn ItemHandle>>,
    buffer_search_bar: Entity<BufferSearchBar>,
    show: bool,
    toggle_selections_handle: PopoverMenuHandle<ContextMenu>,
    toggle_settings_handle: PopoverMenuHandle<ContextMenu>,
    workspace: WeakEntity<Workspace>,
}

impl WebQuickActionBar {
    pub fn new(
        buffer_search_bar: Entity<BufferSearchBar>,
        workspace: &Workspace,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut was_agent_enabled = AgentSettings::get_global(cx).enabled(cx);
        let mut was_agent_button = AgentSettings::get_global(cx).button;

        let ai_settings_subscription = cx.observe_global::<SettingsStore>(move |_, cx| {
            let agent_settings = AgentSettings::get_global(cx);
            let is_agent_enabled = agent_settings.enabled(cx);
            if was_agent_enabled != is_agent_enabled || was_agent_button != agent_settings.button {
                was_agent_enabled = is_agent_enabled;
                was_agent_button = agent_settings.button;
                cx.notify();
            }
        });

        let mut this = Self {
            _ai_settings_subscription: ai_settings_subscription,
            active_item: None,
            buffer_search_bar,
            show: true,
            toggle_selections_handle: Default::default(),
            toggle_settings_handle: Default::default(),
            workspace: workspace.weak_handle(),
        };
        this.apply_settings(cx);
        cx.observe_global::<SettingsStore>(|this, cx| this.apply_settings(cx))
            .detach();
        this
    }

    fn active_editor(&self) -> Option<Entity<Editor>> {
        self.active_item
            .as_ref()
            .and_then(|item| item.downcast::<Editor>())
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let new_show = EditorSettings::get_global(cx).toolbar.quick_actions;
        if new_show != self.show {
            self.show = new_show;
            cx.emit(ToolbarItemEvent::ChangeLocation(
                self.get_toolbar_item_location(),
            ));
        }
    }

    fn get_toolbar_item_location(&self) -> ToolbarItemLocation {
        if self.show && self.active_editor().is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    /// Preview (eye) button — only for previewable file types (markdown, SVG).
    /// Returns None for other files, so the button appears/disappears with the
    /// active tab's file type (dynamic, like desktop).
    fn render_preview_button(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        use markdown_preview::markdown_preview_view::MarkdownPreviewView;
        use svg_preview::svg_preview_view::SvgPreviewView;

        let active_item = self.active_item.as_ref()?;
        let editor = active_item.act_as::<Editor>(cx);

        let preview_target = if let Some(editor) = &editor
            && MarkdownPreviewView::is_markdown_file(editor, cx)
        {
            PreviewTarget::Markdown(editor.clone())
        } else if let Some(buffer) = active_item.act_as::<multi_buffer::MultiBuffer>(cx)
            && SvgPreviewView::is_svg_file(&buffer, cx)
        {
            PreviewTarget::Svg(buffer)
        } else {
            return None;
        };

        let (button_id, tooltip_text) = match &preview_target {
            PreviewTarget::Markdown(_) => ("toggle-markdown-preview", "Preview Markdown"),
            PreviewTarget::Svg(_) => ("toggle-svg-preview", "Preview SVG"),
        };

        let workspace_handle = self.workspace.clone();
        let button = IconButton::new(button_id, IconName::Eye)
            .icon_size(IconSize::Small)
            .style(ButtonStyle::Subtle)
            .tooltip(move |_window, cx| Tooltip::simple(tooltip_text, cx))
            .on_click({
                let active_item = active_item.boxed_clone();
                move |_, window, cx| {
                    let Some(workspace) = workspace_handle.upgrade() else {
                        return;
                    };
                    workspace.update(cx, |workspace, cx| {
                        let Some(pane) = workspace.pane_for(active_item.as_ref()) else {
                            return;
                        };
                        let open_to_the_side = window.modifiers().alt;
                        match &preview_target {
                            PreviewTarget::Markdown(editor) => {
                                let editor = editor.clone();
                                if open_to_the_side {
                                    MarkdownPreviewView::open_preview_to_the_side_of_pane(
                                        workspace, editor, pane, window, cx,
                                    );
                                } else {
                                    MarkdownPreviewView::open_preview_in_pane(
                                        workspace, editor, pane, window, cx,
                                    );
                                }
                            }
                            PreviewTarget::Svg(buffer) => {
                                let buffer = buffer.clone();
                                if open_to_the_side {
                                    SvgPreviewView::open_preview_to_the_side_of_pane(
                                        workspace, buffer, pane, window, cx,
                                    );
                                } else {
                                    SvgPreviewView::open_preview_in_pane(
                                        workspace, buffer, pane, window, cx,
                                    );
                                }
                            }
                        }
                    });
                }
            });
        Some(button.into_any_element())
    }
}

impl EventEmitter<ToolbarItemEvent> for WebQuickActionBar {}

impl ToolbarItemView for WebQuickActionBar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.active_item = active_pane_item.map(|item| item.boxed_clone());
        self.get_toolbar_item_location()
    }
}

impl Render for WebQuickActionBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(editor) = self.active_editor() else {
            return div().id("empty quick action bar");
        };

        let selection_menu_enabled = editor.read(cx).selection_menu_enabled(cx);

        let search_button = (editor.buffer_kind(cx) == ItemBufferKind::Singleton).then(|| {
            IconButton::new("toggle buffer search", IconName::MagnifyingGlass)
                .icon_size(IconSize::Small)
                .style(ButtonStyle::Subtle)
                .toggle_state(!self.buffer_search_bar.read(cx).is_dismissed())
                .tooltip(Tooltip::for_action_title(
                    "Buffer Search",
                    &buffer_search::Deploy::find(),
                ))
                .on_click({
                    let buffer_search_bar = self.buffer_search_bar.clone();
                    move |_, window, cx| {
                        buffer_search_bar.update(cx, |search_bar, cx| {
                            search_bar.toggle(&buffer_search::Deploy::find(), window, cx)
                        });
                    }
                })
        });

        let assistant_button = IconButton::new("toggle inline assistant", IconName::ZedAssistant)
            .icon_size(IconSize::Small)
            .style(ButtonStyle::Subtle)
            .tooltip(Tooltip::for_action_title(
                "Inline Assist",
                &InlineAssist::default(),
            ))
            .on_click(move |_, window, cx| {
                window.dispatch_action(Box::new(InlineAssist::default()), cx);
            });

        let editor_selections_dropdown = selection_menu_enabled.then(|| {
            let has_diff_hunks = editor
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .has_diff_hunks();
            let has_selection = editor.update(cx, |editor, cx| {
                editor.has_non_empty_selection(&editor.display_snapshot(cx))
            });
            let focus_handle = editor.focus_handle(cx);
            let disable_ai = DisableAiSettings::get_global(cx).disable_ai;

            PopoverMenu::new("editor-selections-dropdown")
                .trigger_with_tooltip(
                    IconButton::new("toggle_editor_selections_icon", IconName::CursorIBeam)
                        .icon_size(IconSize::Small)
                        .style(ButtonStyle::Subtle)
                        .toggle_state(self.toggle_selections_handle.is_deployed()),
                    Tooltip::text("Selection Controls"),
                )
                .with_handle(self.toggle_selections_handle.clone())
                .anchor(gpui::Anchor::TopRight)
                .menu(move |window, cx| {
                    let focus = focus_handle.clone();
                    let menu = ContextMenu::build(window, cx, move |menu, _, _| {
                        menu.context(focus.clone())
                            .action("Select All", Box::new(SelectAll))
                            .action(
                                "Select Next Occurrence",
                                Box::new(SelectNext {
                                    replace_newest: false,
                                }),
                            )
                            .action("Expand Selection", Box::new(SelectLargerSyntaxNode))
                            .action("Shrink Selection", Box::new(SelectSmallerSyntaxNode))
                            .action(
                                "Add Cursor Above",
                                Box::new(AddSelectionAbove {
                                    skip_soft_wrap: true,
                                }),
                            )
                            .action(
                                "Add Cursor Below",
                                Box::new(AddSelectionBelow {
                                    skip_soft_wrap: true,
                                }),
                            )
                            .when(!disable_ai, |this| {
                                this.separator().action_disabled_when(
                                    !has_selection,
                                    "Add to Agent Thread",
                                    Box::new(AddSelectionToThread),
                                )
                            })
                            .separator()
                            .action("Go to Symbol", Box::new(ToggleOutline))
                            .action("Go to Line/Column", Box::new(ToggleGoToLine))
                            .separator()
                            .action("Next Problem", Box::new(GoToDiagnostic::default()))
                            .action(
                                "Previous Problem",
                                Box::new(GoToPreviousDiagnostic::default()),
                            )
                            .separator()
                            .action_disabled_when(!has_diff_hunks, "Next Hunk", Box::new(GoToHunk))
                            .action_disabled_when(
                                !has_diff_hunks,
                                "Previous Hunk",
                                Box::new(GoToPreviousHunk),
                            )
                            .separator()
                            .action("Move Line Up", Box::new(MoveLineUp))
                            .action("Move Line Down", Box::new(MoveLineDown))
                            .action("Duplicate Selection", Box::new(DuplicateLineDown))
                    });
                    Some(menu)
                })
        });

        let editor_focus_handle = editor.focus_handle(cx);
        let editor_weak = editor.downgrade();
        // Compute toggle states up front (avoids borrowing cx inside the menu closure).
        let line_numbers_enabled = editor_value_line_numbers(&editor_weak, cx);
        let git_blame_inline_enabled = editor_value_git_blame_inline(&editor_weak, cx);
        let editor_settings_dropdown = PopoverMenu::new("editor-settings")
            .trigger_with_tooltip(
                IconButton::new("toggle_editor_settings_icon", IconName::Filter)
                    .icon_size(IconSize::Small)
                    .style(ButtonStyle::Subtle)
                    .toggle_state(self.toggle_settings_handle.is_deployed()),
                Tooltip::text("Editor Controls"),
            )
            .anchor(gpui::Anchor::TopRight)
            .with_handle(self.toggle_settings_handle.clone())
            .menu(move |window, cx| {
                let focus_handle = editor_focus_handle.clone();
                let editor = editor_weak.clone();
                let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.context(focus_handle);

                    menu = menu.toggleable_entry(
                        "Line Numbers",
                        line_numbers_enabled,
                        IconPosition::Start,
                        Some(editor::actions::ToggleLineNumbers.boxed_clone()),
                        {
                            let editor = editor.clone();
                            move |window, cx| {
                                editor
                                    .update(cx, |editor, cx| {
                                        editor.toggle_line_numbers(
                                            &editor::actions::ToggleLineNumbers,
                                            window,
                                            cx,
                                        );
                                    })
                                    .ok();
                            }
                        },
                    );

                    menu = menu.toggleable_entry(
                        "Inline Git Blame",
                        git_blame_inline_enabled,
                        IconPosition::Start,
                        Some(editor::actions::ToggleGitBlameInline.boxed_clone()),
                        {
                            let editor = editor.clone();
                            move |window, cx| {
                                editor
                                    .update(cx, |editor, cx| {
                                        editor.toggle_git_blame_inline(
                                            &editor::actions::ToggleGitBlameInline,
                                            window,
                                            cx,
                                        )
                                    })
                                    .ok();
                            }
                        },
                    );

                    menu
                });
                Some(menu)
            });

        h_flex()
            .id("quick action bar")
            .gap_1()
            .children(self.render_preview_button(cx))
            .children(search_button)
            .when(
                AgentSettings::get_global(cx).enabled(cx) && AgentSettings::get_global(cx).button,
                |bar| bar.child(assistant_button),
            )
            .children(editor_selections_dropdown)
            .child(editor_settings_dropdown)
    }
}

fn editor_value_line_numbers(editor: &WeakEntity<Editor>, cx: &App) -> bool {
    editor
        .upgrade()
        .map(|e| e.read(cx).line_numbers_enabled(cx))
        .unwrap_or(true)
}

fn editor_value_git_blame_inline(editor: &WeakEntity<Editor>, cx: &App) -> bool {
    editor
        .upgrade()
        .map(|e| e.read(cx).git_blame_inline_enabled())
        .unwrap_or(false)
}
