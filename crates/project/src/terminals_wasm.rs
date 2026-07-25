use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use gpui::{App, AppContext as _, Context, Entity, Task};
use settings::{Settings, SettingsLocation};

use task::{Shell, SpawnInTerminal};
use terminal::{Terminal, TerminalBuilder, terminal_settings::TerminalSettings};
use util::{get_default_system_shell, rel_path::RelPath};

use crate::Project;

pub struct Terminals {
    pub(crate) local_handles: Vec<gpui::WeakEntity<Terminal>>,
}

impl Terminals {
    pub fn new() -> Self {
        Self {
            local_handles: Vec::new(),
        }
    }
}

impl Default for Terminals {
    fn default() -> Self {
        Self::new()
    }
}

impl Project {
    pub fn active_entry_directory(&self, cx: &App) -> Option<PathBuf> {
        let entry_id = self.active_entry()?;
        let worktree = self.worktree_for_entry(entry_id, cx)?;
        let worktree = worktree.read(cx);
        let entry = worktree.entry_for_id(entry_id)?;

        let absolute_path = worktree.absolutize(entry.path.as_ref());
        if entry.is_dir() {
            Some(absolute_path)
        } else {
            absolute_path.parent().map(|p| p.to_path_buf())
        }
    }

    pub fn active_project_directory(&self, cx: &App) -> Option<Arc<Path>> {
        self.active_entry()
            .and_then(|entry_id| self.worktree_for_entry(entry_id, cx))
            .into_iter()
            .chain(self.worktrees(cx))
            .find_map(|tree| tree.read(cx).root_dir())
    }

    pub fn create_terminal_task(
        &mut self,
        spawn_task: SpawnInTerminal,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let path: Option<Arc<Path>> = spawn_task
            .cwd
            .as_ref()
            .map(|cwd| Arc::from(cwd.as_ref()))
            .or_else(|| self.active_project_directory(cx));

        let mut settings_location = None;
        if let Some(path) = path.as_ref() {
            if let Some((worktree, _)) = self.find_worktree(path, cx) {
                settings_location = Some(SettingsLocation {
                    worktree_id: worktree.read(cx).id(),
                    path: RelPath::empty(),
                });
            }
        }
        let settings = TerminalSettings::get(settings_location, cx).clone();
        let shell = Shell::Program(get_default_system_shell());
        let path_style = self.path_style(cx);
        let cwd = path.map(|p| p.to_path_buf());
        let window_id = cx.entity_id().as_u64();

        cx.spawn(async move |project, cx| {
            let (completion_tx, completion_rx) = async_channel::bounded(1);
            let task_state = Some(terminal::TaskState {
                spawned_task: spawn_task.clone(),
                status: terminal::TaskStatus::Running,
                completion_rx,
            });

            let builder = cx
                .update(|cx| {
                    TerminalBuilder::new(
                        cwd,
                        task_state,
                        shell,
                        settings.env.clone(),
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        settings.max_scroll_history_lines,
                        settings.path_hyperlink_regexes.clone(),
                        settings.path_hyperlink_timeout_ms,
                        true,
                        window_id,
                        Some(completion_tx),
                        cx,
                        Vec::new(),
                        path_style,
                    )
                })
                .await?;

            project.update(cx, move |this, cx| {
                let terminal_handle = cx.new(|cx| builder.subscribe(cx));
                this.terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;
                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn create_terminal_shell(
        &mut self,
        cwd: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let path: Option<Arc<Path>> = cwd.map(|p| Arc::from(&*p));

        let mut settings_location = None;
        if let Some(path) = path.as_ref() {
            if let Some((worktree, _)) = self.find_worktree(path, cx) {
                settings_location = Some(SettingsLocation {
                    worktree_id: worktree.read(cx).id(),
                    path: RelPath::empty(),
                });
            }
        }
        let settings = TerminalSettings::get(settings_location, cx).clone();
        let shell = Shell::Program(get_default_system_shell());
        let path_style = self.path_style(cx);
        let cwd = path.map(|p| p.to_path_buf());
        let window_id = cx.entity_id().as_u64();

        cx.spawn(async move |project, cx| {
            let builder = cx
                .update(|cx| {
                    TerminalBuilder::new(
                        cwd,
                        None,
                        shell,
                        settings.env.clone(),
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        settings.max_scroll_history_lines,
                        settings.path_hyperlink_regexes.clone(),
                        settings.path_hyperlink_timeout_ms,
                        true,
                        window_id,
                        None,
                        cx,
                        Vec::new(),
                        path_style,
                    )
                })
                .await?;

            project.update(cx, move |this, cx| {
                let terminal_handle = cx.new(|cx| builder.subscribe(cx));
                this.terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;
                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn create_local_terminal(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let working_directory = self.active_project_directory(cx).map(|p| p.to_path_buf());
        self.create_terminal_shell(working_directory, cx)
    }

    pub fn clone_terminal(
        &mut self,
        terminal: &Entity<Terminal>,
        cx: &mut Context<Self>,
        cwd: Option<PathBuf>,
    ) -> Task<Result<Entity<Terminal>>> {
        let builder = terminal.read(cx).clone_builder(cx, cwd);
        cx.spawn(async move |project, cx| {
            let terminal = builder.await?;
            project.update(cx, |project, cx| {
                let terminal_handle = cx.new(|cx| terminal.subscribe(cx));
                project
                    .terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;
                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }
}
