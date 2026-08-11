use agent_ui::AgentPanel;
use git_ui::git_panel::GitPanel;
use gpui::{App, Context, Render, WeakEntity, Window, px};
use project_panel::ProjectPanel;
use terminal_view::terminal_panel::TerminalPanel;
use ui::{ButtonSize, IconButton, IconName, IconSize, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, Panel, StatusItemView, Workspace};

pub struct MobileWorkspaceNav {
    workspace: WeakEntity<Workspace>,
}

impl MobileWorkspaceNav {
    pub fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self { workspace }
    }

    fn show_editor(&self, window: &mut Window, cx: &mut App) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.close_all_docks(window, cx);
                if workspace.active_item(cx).is_some() {
                    workspace.focus_center_pane(window, cx);
                }
            })
            .ok();
    }

    fn show_panel<P: Panel>(&self, window: &mut Window, cx: &mut App) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.close_all_docks(window, cx);
                workspace.open_panel::<P>(window, cx);
                workspace.focus_panel::<P>(window, cx);
            })
            .ok();
    }

    fn button(
        &self,
        id: &'static str,
        icon: IconName,
        label: &'static str,
        on_click: impl Fn(&Self, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> IconButton {
        IconButton::new(id, icon)
            .size(ButtonSize::Large)
            .width(px(44.))
            .icon_size(IconSize::Small)
            .tab_index(0isize)
            .aria_label(label)
            .tooltip(Tooltip::text(label))
            .on_click(cx.listener(move |this, _, window, cx| {
                on_click(this, window, cx);
            }))
    }
}

impl Render for MobileWorkspaceNav {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex().id("mobile-workspace-nav").gap_0p5().children([
            self.button(
                "mobile-editor",
                IconName::Code,
                "Editor",
                Self::show_editor,
                cx,
            ),
            self.button(
                "mobile-files",
                IconName::FileTree,
                "Files",
                Self::show_panel::<ProjectPanel>,
                cx,
            ),
            self.button(
                "mobile-git",
                IconName::GitBranch,
                "Git",
                Self::show_panel::<GitPanel>,
                cx,
            ),
            self.button(
                "mobile-terminal",
                IconName::Terminal,
                "Terminal",
                Self::show_panel::<TerminalPanel>,
                cx,
            ),
            self.button(
                "mobile-agent",
                IconName::ZedAssistant,
                "Agent",
                Self::show_panel::<AgentPanel>,
                cx,
            ),
        ])
    }
}

impl StatusItemView for MobileWorkspaceNav {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}
